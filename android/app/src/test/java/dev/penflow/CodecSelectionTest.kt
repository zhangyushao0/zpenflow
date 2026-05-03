package dev.penflow

import org.junit.Assert.assertEquals
import org.junit.Test

class CodecSelectionTest {

    @Test
    fun mimeForH264() {
        assertEquals("video/avc", VideoDecoder.mimeFor(Protocol.CODEC_H264))
    }

    @Test
    fun mimeForHevc() {
        assertEquals("video/hevc", VideoDecoder.mimeFor(Protocol.CODEC_HEVC))
    }

    @Test(expected = IllegalArgumentException::class)
    fun mimeForUnknownCodecThrows() {
        VideoDecoder.mimeFor(99.toByte())
    }
}
