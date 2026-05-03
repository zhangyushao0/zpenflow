package dev.penflow

import android.media.MediaCodec
import android.media.MediaFormat
import android.os.Build
import android.util.Log
import android.view.Surface
import java.nio.ByteBuffer
import java.util.ArrayDeque

/**
 * Async hardware decoder rendering directly into a Surface.
 *
 * Async mode (callbacks) keeps the decode pipeline pulling buffers as fast
 * as the codec produces them, with no thread context switches in the hot
 * path. Releasing output buffers with `render = true` hands frames straight
 * to the [Surface]'s buffer queue — zero CPU copy.
 *
 * Codec mime is chosen at construct time from the handshake's `HELLO_PC.codec`:
 * `video/avc` or `video/hevc`. The csd-0 layout is opaque to us — MediaCodec
 * consumes whatever NVENC produced.
 *
 * ## Latency-sensitive tuning
 *
 * - `KEY_LOW_LATENCY = 1` — Android 11+ canonical low-latency hint.
 * - `KEY_OPERATING_RATE = 240` — promise the codec we'll feed faster than
 *   real-time; lets the SoC pick a higher clock domain.
 * - `KEY_PRIORITY = 0` — realtime priority class (vs the default 1, which is
 *   "best effort").
 * - `vendor.qti-ext-dec-low-latency.enable = 1` — Qualcomm-private low-latency
 *   pipeline switch. Non-Qualcomm devices ignore unknown vendor keys.
 *
 * ## Input-buffer feeding
 *
 * The naive pattern (queue an empty buffer back when our packet queue is empty)
 * makes MediaCodec spin a no-op decode pass that adds a frame of latency. We
 * instead **park** the buffer index until a packet actually arrives, then feed
 * it directly. This eliminates the empty-buffer round-trip.
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

    // Single mutex protecting both queues. Producer (network thread) calls
    // feed(); consumer (codec callback thread) calls onInputBufferAvailable.
    private val lock = Any()
    private val pendingData = ArrayDeque<ByteArray>()
    private val parkedIndices = ArrayDeque<Int>()

    fun start() {
        val format = MediaFormat.createVideoFormat(mime, width, height).apply {
            setByteBuffer("csd-0", ByteBuffer.wrap(csd0))
            setInteger(MediaFormat.KEY_LOW_LATENCY, 1)
            setInteger(MediaFormat.KEY_FRAME_RATE, fps)

            // Latency-sensitive operation: tell the codec we'll feed at up to
            // 240 fps so it picks a high clock domain rather than throttling.
            if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.M) {
                setInteger(MediaFormat.KEY_OPERATING_RATE, 240)
                setInteger(MediaFormat.KEY_PRIORITY, 0)
            }

            // Qualcomm-private low-latency flag. Setting on non-Qualcomm
            // devices is a silent no-op (MediaFormat doesn't validate vendor
            // keys against the codec).
            setInteger("vendor.qti-ext-dec-low-latency.enable", 1)
        }

        codec.setCallback(object : MediaCodec.Callback() {
            override fun onInputBufferAvailable(c: MediaCodec, index: Int) {
                val data: ByteArray? = synchronized(lock) {
                    if (pendingData.isNotEmpty()) {
                        pendingData.removeFirst()
                    } else {
                        // No data yet — park this index for feed() to pick up.
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
                c.releaseOutputBuffer(index, true)
                onDecoded(decodedNs)
            }

            override fun onError(c: MediaCodec, e: MediaCodec.CodecException) {
                Log.e(TAG, "decoder error", e)
            }

            override fun onOutputFormatChanged(c: MediaCodec, format: MediaFormat) {
                Log.i(TAG, "decoder output format: $format")
            }
        })

        codec.configure(format, surface, null, 0)
        codec.start()
        Log.i(TAG, "started $mime decoder ${width}x${height}@${fps} (operating_rate=240)")
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
    }

    companion object {
        private const val TAG = "VideoDecoder"

        fun mimeFor(codec: Byte): String = when (codec) {
            Protocol.CODEC_H264 -> "video/avc"
            Protocol.CODEC_HEVC -> "video/hevc"
            else -> throw IllegalArgumentException("unknown codec id: $codec")
        }
    }
}
