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
 * Design.md §10.6 MIN_LATENCY frame pacing:
 *   Replaces unconditional `releaseOutputBuffer(idx, true)` with a
 *   newest-buffer-wins drain that posts the render release on the codec
 *   handler with `System.nanoTime()` as the PTS. Coalesces callback
 *   bursts to the latest output index, releases older ones with
 *   render=false, lets SurfaceFlinger drop superseded buffers under load.
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
    private val onDecoded: (decodedNs: Long) -> Unit = {},
) {

    private val mime: String = mimeFor(codecId)
    private val codec: MediaCodec = MediaCodec.createDecoderByType(mime)

    /** Owning codec callbacks; same handler is reused for posted render releases. */
    private val codecThread = HandlerThread("video-codec").apply { start() }
    private val codecHandler = Handler(codecThread.looper)

    // Single mutex protecting both INPUT queues. Producer (network thread)
    // calls feed(); consumer (codec callback thread) calls onInputBufferAvailable.
    private val lock = Any()
    private val pendingData = ArrayDeque<ByteArray>()
    private val parkedIndices = ArrayDeque<Int>()

    // OUTPUT-side MIN_LATENCY state — drains to newest output index, drops the rest.
    private val outputLock = Any()
    private var newestOutputIndex: Int? = null
    private var renderPosted = false

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

            if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.M) {
                if (isAdreno620) {
                    // §10.2: KEY_OPERATING_RATE=240 + KEY_PRIORITY=0
                    // SIGSEGVs Adreno 620 (Snapdragon 765G). Use
                    // moonlight's Short.MAX_VALUE workaround alone.
                    setInteger(MediaFormat.KEY_OPERATING_RATE, Short.MAX_VALUE.toInt())
                    // do NOT set KEY_PRIORITY here.
                } else {
                    // Predecessor's combo. Validated working on Adreno 720
                    // (Snapdragon 8s Gen 3 — the dev rig).
                    setInteger(MediaFormat.KEY_OPERATING_RATE, 240)
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
                val data: ByteArray? = synchronized(lock) {
                    if (pendingData.isNotEmpty()) {
                        pendingData.removeFirst()
                    } else {
                        parkedIndices.addLast(index)
                        null
                    }
                }
                if (data != null) {
                    feedBuffer(c, index, data)
                }
            }

            override fun onOutputBufferAvailable(
                c: MediaCodec,
                index: Int,
                info: MediaCodec.BufferInfo
            ) {
                val decodedNs = System.nanoTime()

                // §10.6 MIN_LATENCY drain: keep only the newest index, release
                // any older one with render=false. Post the render release on
                // the codec handler so SurfaceFlinger can supersede it if a
                // newer buffer arrives in the same vsync window.
                var shouldPostRender = false
                val dropped: Int? = synchronized(outputLock) {
                    val d = newestOutputIndex
                    newestOutputIndex = index
                    if (!renderPosted) {
                        renderPosted = true
                        shouldPostRender = true
                    }
                    d
                }
                dropped?.let { c.releaseOutputBuffer(it, false) }

                if (shouldPostRender) {
                    codecHandler.post {
                        val toRender: Int? = synchronized(outputLock) {
                            val chosen = newestOutputIndex
                            newestOutputIndex = null
                            renderPosted = false
                            chosen
                        }
                        toRender?.let { idx ->
                            // Pass System.nanoTime() as the render PTS so
                            // SurfaceFlinger schedules for the next vsync
                            // and drops late buffers if superseded.
                            c.releaseOutputBuffer(idx, System.nanoTime())
                        }
                    }
                }

                onDecoded(decodedNs)
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
        Log.i(
            TAG,
            "started $mime decoder ${width}x${height}@${fps} on $codecName " +
                "(qualcomm=$isQualcomm, adreno620=$isAdreno620)"
        )
    }

    /** Submit a coded video access unit (Annex-B framed). */
    fun feed(coded: ByteArray) {
        val parkedIndex: Int? = synchronized(lock) {
            if (parkedIndices.isNotEmpty()) {
                parkedIndices.removeFirst()
            } else {
                pendingData.addLast(coded)
                null
            }
        }
        if (parkedIndex != null) {
            feedBuffer(codec, parkedIndex, coded)
        }
    }

    private fun feedBuffer(c: MediaCodec, index: Int, data: ByteArray) {
        val buf = c.getInputBuffer(index) ?: return
        buf.clear()
        buf.put(data)
        c.queueInputBuffer(index, 0, data.size, System.nanoTime() / 1000, 0)
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
