package dev.penflow

import android.media.MediaCodec
import android.media.MediaFormat
import android.os.Build
import android.os.Handler
import android.os.HandlerThread
import android.util.Log
import android.view.Surface
import java.nio.ByteBuffer
import java.util.ArrayDeque

// API level constants for Surface.setFrameRate (declared inline so the file
// builds on min-SDK without a separate compatibility shim).
private const val FRAME_RATE_COMPATIBILITY_FIXED_SOURCE = 1
// `CHANGE_FRAME_RATE_ALWAYS` (API 31+) tells SurfaceFlinger to switch the
// panel mode immediately and skip the seamless-only check. We're streaming
// to a pen display where consistency > seamlessness, and Android's default
// (`CHANGE_FRAME_RATE_ONLY_IF_SEAMLESS`, value 0) sometimes refuses the
// rate change on MovinkPad's panel modes — leaving us at whatever the
// panel happened to be running at, which voids the FIXED_SOURCE intent.
private const val CHANGE_FRAME_RATE_ALWAYS = 1

/**
 * Async hardware decoder rendering directly into a Surface.
 *
 * **Setup philosophy:** start from the predecessor's exactly-working
 * configuration (KEY_OPERATING_RATE=240 + KEY_PRIORITY=0 on Qualcomm,
 * vendor.qti-ext-dec-low-latency.enable=1, simple `releaseOutputBuffer(idx,
 * true)` rendering) and add design.md §10 optimizations only behind
 * specific gates so they can't break working chips.
 *
 * Design.md §10.2 Adreno 620 SIGSEGV fix:
 *   The combination KEY_OPERATING_RATE=240 + KEY_PRIORITY=0 crashes
 *   Adreno 620 (Snapdragon 765G — Mi 10 lite 5G, Redmi K30i 5G), per
 *   moonlight-android's MediaCodecHelper.java:482. The dev rig (MovinkPad
 *   / Adreno 720, Snapdragon 8s Gen 3) is fine with the combo. We
 *   detect the affected chip via `Build.HARDWARE` and fall back to
 *   moonlight's Short.MAX_VALUE workaround on those devices only.
 *
 * Render strategy: simple `releaseOutputBuffer(idx, true)` per callback,
 * paired with a `Surface.setFrameRate(fps, FIXED_SOURCE)` hint to keep
 * the panel's VRR controller from downclocking during idle periods.
 *   We previously implemented design.md §10.6 MIN_LATENCY drain
 *   (newest-wins, drop the rest, post render at System.nanoTime()) but
 *   it caused a "ratchet" decoder degradation on the MovinkPad: dropping
 *   most buffers made SurfaceFlinger see few presented frames, the panel
 *   dropped to a low refresh rate, BufferQueue back-pressured the
 *   remaining renders, the codec callback handler stalled inside
 *   `releaseOutputBuffer`, and `decodedNs` (captured at callback entry)
 *   ratcheted up to 30-40 ms when idle with no recovery until user touch
 *   re-woke the panel. The simple form matches predecessor's measured
 *   7-8 ms baseline and the setFrameRate hint stops the panel from idling
 *   in the first place.
 *
 * Vendor key `vendor.qti-ext-dec-picture-order.enable=1`:
 *   Disables HEVC reorder buffering on Qualcomm. Saves 5-10 ms of decode
 *   delay (moonlight finding). On non-Qualcomm codecs it's a silent
 *   no-op since MediaFormat doesn't validate vendor keys.
 *
 * **Still deferred** (need lifecycle plumbing): §10.3 codec recovery
 * ladder, §10.4 hung-decoder watchdog, §10.5 surface-destroyed handler.
 *
 * Codec mime is chosen at construct time from the handshake's
 * `HELLO_PC.codec`: `video/avc` or `video/hevc`. The csd-0 layout is
 * opaque to us — MediaCodec consumes whatever the PC encoder produced.
 */
class VideoDecoder(
    private val width: Int,
    private val height: Int,
    private val fps: Int,
    private val codecId: Byte,
    private val surface: Surface,
    private val csd0: ByteArray,
    /** Called from the codec callback thread for every emitted output frame.
     *  `framePtsNs` is the server-stamped PC pts_ns we round-tripped through
     *  `queueInputBuffer`'s presentationTimeUs, so the caller can match it
     *  against its own pending-sample map without any FIFO assumption. */
    private val onDecoded: (framePtsNs: Long, decodedNs: Long) -> Unit = { _, _ -> },
) {

    private val mime: String = mimeFor(codecId)
    // Use `MediaCodec.createDecoderByType` (platform default).
    //
    // We previously preferred the `.low_latency`-suffixed variant via
    // `createLowLatencyDecoder`, but that turned out to *increase*
    // per-frame decode latency on Snapdragon 8s Gen 3: the standard
    // c2.qti.{avc,hevc}.decoder reports `output_delay = 4` (small
    // internal pipeline), while the `.low_latency` variant reports
    // `output_delay = 24` regardless of the SPS we feed it (the
    // `.low_latency` variant trades a deeper output pipeline for
    // lower jitter — wrong trade for our zero-reorder, low-frame-
    // rate-variation workload). The combination of the encoder-side
    // SPS patcher (1-deep DPB declared in the bitstream) plus the
    // standard decoder gives the smallest measured per-frame
    // latency.
    //
    // `KEY_LOW_LATENCY = 1` plus the existing
    // `vendor.qti-ext-dec-low-latency.enable` vendor key still tell
    // the standard decoder to skip whatever output reordering it
    // would otherwise do.
    private val codec: MediaCodec = MediaCodec.createDecoderByType(mime)

    /** Owning codec callbacks; same handler is reused for posted render releases. */
    private val codecThread = HandlerThread("video-codec").apply { start() }
    private val codecHandler = Handler(codecThread.looper)

    // Single mutex protecting both INPUT queues. Producer (network thread)
    // calls feed(); consumer (codec callback thread) calls onInputBufferAvailable.
    // Each pending data carries its caller-supplied PTS (PC pts_ns) so
    // MediaCodec sees a stable, unique presentationTimeUs per frame and the
    // network thread can match outputs back without FIFO assumptions.
    private val lock = Any()
    private val pendingData = ArrayDeque<Pair<Long, ByteArray>>()
    private val parkedIndices = ArrayDeque<Int>()
    private val pendingKeyframeFlags = HashMap<Long, Boolean>()

    fun start() {
        // codec.name reflects the actual decoder we got from
        // createDecoderByType — known before configure() so we can
        // branch the format flags correctly.
        val codecName = codec.name.lowercase()
        val isQualcomm = codecName.startsWith("omx.qcom.") || codecName.startsWith("c2.qti.")
        val isAdreno620 = isAdreno620Hardware()

        val format = MediaFormat.createVideoFormat(mime, width, height).apply {
            setByteBuffer("csd-0", ByteBuffer.wrap(csd0))
            setInteger(MediaFormat.KEY_LOW_LATENCY, 1)
            setInteger(MediaFormat.KEY_FRAME_RATE, fps)

            // Colour metadata. The PC encoder writes BT.709 full-range NV12
            // (capture is sRGB scan-out, ColorConverter targets
            // YCBCR_FULL_G22_LEFT_P709, MF input type carries
            // MFNominalRange_0_255). Without telling MediaCodec this
            // explicitly, some implementations and SurfaceFlinger fall back
            // to the AVC/HEVC default of "limited range BT.709", clamping
            // 16->0 and 235->255 with full clipping above 235 — visible as
            // blown-out highlights on greyscale ramps (issue #1). Mirrors
            // Moonlight's MediaCodecDecoderRenderer.createBaseMediaFormat.
            if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.N) {
                setInteger(MediaFormat.KEY_COLOR_RANGE, MediaFormat.COLOR_RANGE_FULL)
                setInteger(MediaFormat.KEY_COLOR_STANDARD, MediaFormat.COLOR_STANDARD_BT709)
                setInteger(MediaFormat.KEY_COLOR_TRANSFER, MediaFormat.COLOR_TRANSFER_SDR_VIDEO)
            }

            if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.M) {
                if (isAdreno620) {
                    // §10.2: KEY_OPERATING_RATE=240 + KEY_PRIORITY=0
                    // SIGSEGVs Adreno 620 (Snapdragon 765G). Use
                    // moonlight's Short.MAX_VALUE workaround alone.
                    setInteger(MediaFormat.KEY_OPERATING_RATE, Short.MAX_VALUE.toInt())
                    // do NOT set KEY_PRIORITY here.
                } else {
                    // `Short.MAX_VALUE` (32767) is moonlight's value for
                    // "as fast as physically possible". Adreno's c2.qti
                    // codec service interprets this as a hard hint to
                    // hold the decoder block at peak clock — saves 1-3 ms
                    // of decode tail vs. the predecessor's 240 hint
                    // (which Qualcomm's DVFS treated as "I'll match real
                    // demand" and slowly downclocked between frames).
                    // Validated working on Adreno 720 (Snapdragon 8s Gen
                    // 3 — the dev rig); Adreno 620 takes the branch
                    // above to dodge a SIGSEGV from the same combo.
                    setInteger(MediaFormat.KEY_OPERATING_RATE, Short.MAX_VALUE.toInt())
                    setInteger(MediaFormat.KEY_PRIORITY, 0)
                }
            }

            if (isQualcomm && Build.VERSION.SDK_INT >= Build.VERSION_CODES.O) {
                // Qualcomm-private low-latency pipeline switch.
                setInteger("vendor.qti-ext-dec-low-latency.enable", 1)
                // Disables HEVC reorder buffering — saves 5-10 ms per frame.
                setInteger("vendor.qti-ext-dec-picture-order.enable", 1)
            }
        }

        codec.setCallback(object : MediaCodec.Callback() {
            override fun onInputBufferAvailable(c: MediaCodec, index: Int) {
                val pair: Pair<Long, ByteArray>? = synchronized(lock) {
                    if (pendingData.isNotEmpty()) {
                        pendingData.removeFirst()
                    } else {
                        parkedIndices.addLast(index)
                        null
                    }
                }
                if (pair != null) {
                    feedBuffer(c, index, pair.first, pair.second)
                }
            }

            override fun onOutputBufferAvailable(
                c: MediaCodec,
                index: Int,
                info: MediaCodec.BufferInfo
            ) {
                val decodedNs = System.nanoTime()
                // MediaCodec round-trips the input PTS as info.presentationTimeUs
                // (microseconds). We fed it pts_ns/1000 so * 1000 recovers
                // the original PC pts_ns; the caller uses it to look up the
                // matching recv sample without any FIFO ordering assumption.
                val framePtsNs = info.presentationTimeUs * 1000L
                c.releaseOutputBuffer(index, true)
                onDecoded(framePtsNs, decodedNs)
            }

            override fun onError(c: MediaCodec, e: MediaCodec.CodecException) {
                Log.e(
                    TAG,
                    "decoder error: code=${e.errorCode} diagnostic=${e.diagnosticInfo} " +
                        "recoverable=${e.isRecoverable} transient=${e.isTransient}",
                    e
                )
            }

            override fun onOutputFormatChanged(c: MediaCodec, format: MediaFormat) {
                Log.i(TAG, "decoder output format: $format")
            }
        }, codecHandler)

        codec.configure(format, surface, null, 0)
        codec.start()

        // Tell SurfaceFlinger our intended source frame rate. Without this
        // hint MovinkPad's VRR / Adaptive Refresh Rate controller can drop
        // the panel to a low rate when the rest of the system looks idle —
        // which then back-pressures our render path (the BufferQueue fills
        // because consumer is slower than producer), the codec callback
        // handler gets stuck waiting on `releaseOutputBuffer`, and dec_us
        // ratchets up with no recovery until user touch wakes the panel.
        // FRAME_RATE_COMPATIBILITY_FIXED_SOURCE = "I want this exact rate;
        // pick the panel mode that doesn't drop frames against my source."
        // On API 31+ also pass CHANGE_FRAME_RATE_ALWAYS so the platform
        // commits to the requested rate even when the transition isn't
        // seamless — better than getting silently downclocked because the
        // panel's seamless modes don't include our target.
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.S) {
            try {
                surface.setFrameRate(
                    fps.toFloat(),
                    FRAME_RATE_COMPATIBILITY_FIXED_SOURCE,
                    CHANGE_FRAME_RATE_ALWAYS,
                )
            } catch (t: Throwable) {
                Log.w(TAG, "Surface.setFrameRate($fps, ALWAYS) failed; panel VRR may downclock", t)
            }
        } else if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.R) {
            try {
                surface.setFrameRate(fps.toFloat(), FRAME_RATE_COMPATIBILITY_FIXED_SOURCE)
            } catch (t: Throwable) {
                Log.w(TAG, "Surface.setFrameRate($fps) failed; panel VRR may downclock", t)
            }
        }

        Log.i(
            TAG,
            "started $mime decoder ${width}x${height}@${fps} on $codecName " +
                "(qualcomm=$isQualcomm, adreno620=$isAdreno620)"
        )
    }

    /** Submit a coded video access unit (Annex-B framed). `framePtsNs` is the
     *  server-stamped PC pts_ns; we pass it through MediaCodec as the input
     *  buffer's `presentationTimeUs` (truncated to microseconds) so the
     *  network thread can match outputs back by PTS instead of FIFO order.
     *  `isKeyframe` propagates the server-side FRAME_FLAG_KEYFRAME so that
     *  IDR access units carry MediaCodec's BUFFER_FLAG_KEY_FRAME, hinting
     *  to the codec that it can drop any cached reference state and re-anchor. */
    fun feed(framePtsNs: Long, coded: ByteArray, isKeyframe: Boolean = false) {
        val parkedIndex: Int? = synchronized(lock) {
            // Stash the keyframe flag with the queued data so the codec
            // callback applies it correctly when the buffer becomes
            // available later.
            pendingKeyframeFlags[framePtsNs] = isKeyframe
            if (parkedIndices.isNotEmpty()) {
                parkedIndices.removeFirst()
            } else {
                pendingData.addLast(framePtsNs to coded)
                null
            }
        }
        if (parkedIndex != null) {
            feedBuffer(codec, parkedIndex, framePtsNs, coded)
        }
    }

    private fun feedBuffer(c: MediaCodec, index: Int, framePtsNs: Long, data: ByteArray) {
        val buf = c.getInputBuffer(index) ?: return
        buf.clear()
        buf.put(data)
        val isKey = synchronized(lock) {
            pendingKeyframeFlags.remove(framePtsNs) ?: false
        }
        val flags = if (isKey) MediaCodec.BUFFER_FLAG_KEY_FRAME else 0
        c.queueInputBuffer(index, 0, data.size, framePtsNs / 1000, flags)
    }

    fun stop() {
        try {
            codec.stop()
        } catch (_: IllegalStateException) {
        }
        codec.release()
        codecThread.quitSafely()
    }

    companion object {
        private const val TAG = "VideoDecoder"

        fun mimeFor(codec: Byte): String = when (codec) {
            Protocol.CODEC_H264 -> "video/avc"
            Protocol.CODEC_HEVC -> "video/hevc"
            else -> throw IllegalArgumentException("unknown codec id: $codec")
        }

        /**
         * Identify Adreno 620 chips (Snapdragon 765G — Xiaomi Mi 10 lite
         * 5G, Redmi K30i 5G, etc.) where KEY_OPERATING_RATE=240 +
         * KEY_PRIORITY=0 SIGSEGVs the decoder. moonlight-android's
         * MediaCodecHelper.java enumerates the same set via Build.HARDWARE
         * being "lito" (Snapdragon 765G's hardware identifier).
         *
         * False negatives are safer than false positives: a missed
         * detection means we use the same combo predecessor uses today,
         * which works on every other Adreno we've tested.
         */
        private fun isAdreno620Hardware(): Boolean {
            val hw = Build.HARDWARE.lowercase()
            // "lito" = Snapdragon 765G platform = Adreno 620.
            return hw == "lito"
        }
    }
}
