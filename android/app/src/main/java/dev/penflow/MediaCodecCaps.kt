package dev.penflow

import android.media.MediaCodecList
import android.util.Log

/**
 * Queries [MediaCodecList] for hardware-accelerated video decoders.
 *
 * Result is the bitmask reported in HELLO_ANDROID.codec_caps:
 *   bit 0 = H.264, bit 1 = HEVC, bit 2 = AV1
 *
 * Software-only decoders are excluded — for low-latency display we want hardware paths.
 */
object MediaCodecCaps {

    private const val TAG = "MediaCodecCaps"

    fun queryHardwareDecodeBitmask(): Int {
        var caps = 0
        val list = MediaCodecList(MediaCodecList.REGULAR_CODECS)
        for (info in list.codecInfos) {
            if (info.isEncoder) continue
            // isHardwareAccelerated is API 29+; the MovinkPad is API 35
            if (!info.isHardwareAccelerated) continue
            for (mime in info.supportedTypes) {
                when (mime.lowercase()) {
                    "video/avc"  -> caps = caps or Protocol.CODEC_CAPS_H264
                    "video/hevc" -> caps = caps or Protocol.CODEC_CAPS_HEVC
                    "video/av01" -> caps = caps or Protocol.CODEC_CAPS_AV1
                }
            }
        }
        Log.i(TAG, "hardware decode caps: 0x${"%02x".format(caps)} " +
            "(H264=${caps and Protocol.CODEC_CAPS_H264 != 0}, " +
            "HEVC=${caps and Protocol.CODEC_CAPS_HEVC != 0}, " +
            "AV1=${caps and Protocol.CODEC_CAPS_AV1 != 0})")
        return caps
    }
}
