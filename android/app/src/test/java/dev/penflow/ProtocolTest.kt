package dev.penflow

import org.junit.Assert.assertArrayEquals
import org.junit.Assert.assertEquals
import org.junit.Assert.assertNull
import org.junit.Test
import java.nio.ByteBuffer
import java.nio.ByteOrder

class ProtocolTest {

    private fun u32(v: Int): ByteArray =
        ByteBuffer.allocate(4).order(ByteOrder.BIG_ENDIAN).putInt(v).array()

    private fun u64(v: Long): ByteArray =
        ByteBuffer.allocate(8).order(ByteOrder.BIG_ENDIAN).putLong(v).array()

    @Test
    fun decodeVideoFrameLegacyHeader() {
        val pts = 12345678901234L
        val flags = 0x01.toByte()  // keyframe only
        val coded = byteArrayOf(0, 0, 0, 1, 0x67.toByte(), 1, 2, 3)
        val payload = u64(pts) + byteArrayOf(flags) + coded
        val parsed = Protocol.decodeVideoFrame(payload)
        assertEquals(pts, parsed.ptsNs)
        assertEquals(flags.toInt() and 0xFF, parsed.flags)
        assertNull(parsed.captureUs)
        assertNull(parsed.encodeUs)
        assertArrayEquals(coded, parsed.coded)
    }

    @Test
    fun decodeVideoFrameExtendedHeader() {
        val pts = 99L
        val flags = (0x80 or 0x01).toByte()  // extended + keyframe
        val capUs = 1234
        val encUs = 5678
        val coded = ByteArray(32) { 0xAB.toByte() }
        val payload = u64(pts) + byteArrayOf(flags) + u32(capUs) + u32(encUs) + coded
        val parsed = Protocol.decodeVideoFrame(payload)
        assertEquals(pts, parsed.ptsNs)
        assertEquals(0x81, parsed.flags)
        assertEquals(capUs, parsed.captureUs)
        assertEquals(encUs, parsed.encodeUs)
        assertArrayEquals(coded, parsed.coded)
    }

    @Test
    fun decodeTelemetryRoundtrip() {
        val payload = ByteBuffer.allocate(21).order(ByteOrder.BIG_ENDIAN)
            .putInt(180)    // frames
            .putInt(2)      // dropped
            .putInt(950)    // capture_us_avg
            .putInt(1700)   // encode_us_avg
            .putInt(2400)   // encode_us_p99
            .put(1.toByte()) // queue_depth
            .array()
        val t = Protocol.decodeTelemetry(payload)
        assertEquals(180, t.frames)
        assertEquals(2, t.dropped)
        assertEquals(950, t.captureUsAvg)
        assertEquals(1700, t.encodeUsAvg)
        assertEquals(2400, t.encodeUsP99)
        assertEquals(1, t.queueDepth)
    }
}
