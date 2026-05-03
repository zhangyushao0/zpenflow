package dev.penflow

import java.io.DataInputStream
import java.io.DataOutputStream
import java.nio.ByteBuffer
import java.nio.ByteOrder

/**
 * Binary wire protocol — see protocol/PROTOCOL.md.
 *
 * All multi-byte values are big-endian.
 */
object Protocol {

    // PC -> Android
    const val MSG_HELLO_PC: Byte = 0x01
    const val MSG_VIDEO_CONFIG: Byte = 0x02
    const val MSG_VIDEO_FRAME: Byte = 0x03
    const val MSG_BRUSH_HINT: Byte = 0x04
    const val MSG_TELEMETRY: Byte = 0x05
    const val MSG_TIME_SYNC_RESP: Byte = 0x06
    const val MSG_PC_GOODBYE: Byte = 0x7F

    // Android -> PC
    const val MSG_HELLO_ANDROID: Byte = 0x81.toByte()
    const val MSG_PEN_EVENT: Byte = 0x82.toByte()
    const val MSG_TOUCH_EVENT: Byte = 0x83.toByte()
    const val MSG_TIME_SYNC_REQ: Byte = 0x84.toByte()
    const val MSG_ANDROID_GOODBYE: Byte = 0xFF.toByte()

    // Codec ids in HELLO_PC.codec
    const val CODEC_H264: Byte = 1
    const val CODEC_HEVC: Byte = 2
    const val CODEC_AV1: Byte = 3

    // Frame flags in MSG_VIDEO_FRAME.flags
    const val FRAME_FLAG_KEYFRAME = 0x01
    const val FRAME_FLAG_EXTENDED = 0x80

    // codec_caps bitmask in HELLO_ANDROID
    const val CODEC_CAPS_H264 = 1 shl 0
    const val CODEC_CAPS_HEVC = 1 shl 1
    const val CODEC_CAPS_AV1 = 1 shl 2

    fun sendMsg(out: DataOutputStream, type: Byte, payload: ByteArray) {
        out.writeByte(type.toInt())
        out.writeInt(payload.size)
        out.write(payload)
        out.flush()
    }

    fun recvMsg(input: DataInputStream): Pair<Byte, ByteArray> {
        val type = input.readByte()
        val len = input.readInt()
        require(len in 0..(64 * 1024 * 1024)) { "absurd message length: $len" }
        val payload = ByteArray(len)
        input.readFully(payload)
        return type to payload
    }

    /** Encodes the HELLO_ANDROID payload reporting device capabilities. */
    fun encodeHelloAndroid(
        protocolVersion: Int,
        displayWidth: Int,
        displayHeight: Int,
        penMaxPressure: Int,
        penTiltMinDeg: Int,
        penTiltMaxDeg: Int,
        penButtonsCount: Int,
        codecCaps: Int,
    ): ByteArray {
        val buf = ByteBuffer.allocate(13).order(ByteOrder.BIG_ENDIAN)
        buf.put(protocolVersion.toByte())
        buf.putShort(displayWidth.toShort())
        buf.putShort(displayHeight.toShort())
        buf.putShort(penMaxPressure.toShort())
        buf.putShort(penTiltMinDeg.toShort())
        buf.putShort(penTiltMaxDeg.toShort())
        buf.put(penButtonsCount.toByte())
        buf.put(codecCaps.toByte())
        return buf.array()
    }

    /** Parses the HELLO_PC payload sent by the PC server. */
    data class HelloPc(
        val protocolVersion: Int,
        val width: Int,
        val height: Int,
        val codec: Byte,
        val bitrate: Int,
        val fps: Int
    )

    fun decodeHelloPc(payload: ByteArray): HelloPc {
        val buf = ByteBuffer.wrap(payload).order(ByteOrder.BIG_ENDIAN)
        return HelloPc(
            protocolVersion = buf.get().toInt() and 0xFF,
            width = buf.short.toInt() and 0xFFFF,
            height = buf.short.toInt() and 0xFFFF,
            codec = buf.get(),
            bitrate = buf.int,
            fps = buf.get().toInt() and 0xFF
        )
    }

    /** Decoded MSG_VIDEO_FRAME header + coded payload. */
    data class VideoFrameHeader(
        val ptsNs: Long,
        val flags: Int,
        val captureUs: Int?,   // null when extended bit not set
        val encodeUs: Int?,
        val coded: ByteArray,
    )

    fun decodeVideoFrame(payload: ByteArray): VideoFrameHeader {
        require(payload.size >= 9) { "frame payload too short: ${payload.size}" }
        val buf = ByteBuffer.wrap(payload).order(ByteOrder.BIG_ENDIAN)
        val pts = buf.long
        val flags = buf.get().toInt() and 0xFF
        return if (flags and FRAME_FLAG_EXTENDED != 0) {
            require(payload.size >= 17) { "extended header truncated: ${payload.size}" }
            val capture = buf.int
            val encode = buf.int
            val data = ByteArray(payload.size - 17)
            buf.get(data)
            VideoFrameHeader(pts, flags, capture, encode, data)
        } else {
            val data = ByteArray(payload.size - 9)
            buf.get(data)
            VideoFrameHeader(pts, flags, null, null, data)
        }
    }

    /** Encodes a PEN_EVENT payload. */
    fun encodePenEvent(
        tsNs: Long,
        phase: Int,
        x: Float,
        y: Float,
        pressure: Float,
        tiltX: Float,
        tiltY: Float,
        buttonsBitmask: Int,
        tool: Int
    ): ByteArray {
        // 8 + 1 + 4*5 + 1 + 1 = 31 bytes
        val buf = ByteBuffer.allocate(31).order(ByteOrder.BIG_ENDIAN)
        buf.putLong(tsNs)
        buf.put(phase.toByte())
        buf.putFloat(x)
        buf.putFloat(y)
        buf.putFloat(pressure)
        buf.putFloat(tiltX)
        buf.putFloat(tiltY)
        buf.put(buttonsBitmask.toByte())
        buf.put(tool.toByte())
        return buf.array()
    }

    /** MSG_TELEMETRY payload from the server (Phase 0+). */
    data class Telemetry(
        val frames: Int,
        val dropped: Int,
        val captureUsAvg: Int,
        val encodeUsAvg: Int,
        val encodeUsP99: Int,
        val queueDepth: Int,
    )

    fun decodeTelemetry(payload: ByteArray): Telemetry {
        require(payload.size == 21) { "TELEMETRY length ${payload.size} != 21" }
        val buf = ByteBuffer.wrap(payload).order(ByteOrder.BIG_ENDIAN)
        return Telemetry(
            frames = buf.int,
            dropped = buf.int,
            captureUsAvg = buf.int,
            encodeUsAvg = buf.int,
            encodeUsP99 = buf.int,
            queueDepth = buf.get().toInt() and 0xFF,
        )
    }

    /** MSG_TIME_SYNC_REQ payload: u64 android nanoTime() at send. */
    fun encodeTimeSyncReq(androidT1Ns: Long): ByteArray =
        ByteBuffer.allocate(8).order(ByteOrder.BIG_ENDIAN).putLong(androidT1Ns).array()

    /** MSG_TIME_SYNC_RESP payload: echoed t1 + PC monotonic_ns at recv (t2) and send (t3). */
    data class TimeSyncResp(val androidT1Ns: Long, val pcT2Ns: Long, val pcT3Ns: Long)

    fun decodeTimeSyncResp(payload: ByteArray): TimeSyncResp {
        require(payload.size == 24) { "TIME_SYNC_RESP length ${payload.size} != 24" }
        val buf = ByteBuffer.wrap(payload).order(ByteOrder.BIG_ENDIAN)
        return TimeSyncResp(buf.long, buf.long, buf.long)
    }

    /** One contact in a multi-finger touch snapshot. */
    data class TouchContact(
        val pointerId: Int,
        val xNorm: Float,
        val yNorm: Float,
        val pressure: Float,
    )

    /** Encodes a MSG_TOUCH_EVENT payload: u64 ts + u8 count + N * (u8 id + 3*f32). */
    fun encodeTouchEvent(tsNs: Long, contacts: List<TouchContact>): ByteArray {
        val n = contacts.size.coerceAtMost(255)
        val buf = ByteBuffer.allocate(9 + n * 13).order(ByteOrder.BIG_ENDIAN)
        buf.putLong(tsNs)
        buf.put(n.toByte())
        for (i in 0 until n) {
            val c = contacts[i]
            buf.put(c.pointerId.coerceIn(0, 255).toByte())
            buf.putFloat(c.xNorm)
            buf.putFloat(c.yNorm)
            buf.putFloat(c.pressure)
        }
        return buf.array()
    }
}
