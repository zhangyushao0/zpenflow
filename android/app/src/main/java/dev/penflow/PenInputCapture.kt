package dev.penflow

import android.view.MotionEvent

/**
 * Translates Android [MotionEvent]s into wire-format pen events.
 *
 * Button decoding for the Wacom Pro Pen 3 thick grip on the MovinkPad:
 *  - btn1 = [MotionEvent.BUTTON_STYLUS_PRIMARY]   (0x20)
 *  - btn2 = [MotionEvent.BUTTON_STYLUS_SECONDARY] (0x40)
 *  - btn3 = [MotionEvent.BUTTON_TERTIARY]         (0x04)
 *
 * Empirically verified via logcat on the MovinkPad: the firmware reports the
 * third barrel button as a distinct BUTTON_TERTIARY bit (not as a chord of
 * the two stylus bits, which is what older Wacom desktop tablets do).
 *
 * The chord-detection fallback is kept as a defensive code path in case we
 * ever encounter older firmware that does encode btn3 as a chord.
 */
class PenInputCapture(
    private val viewWidth: () -> Int,
    private val viewHeight: () -> Int,
    private val onEvent: (PenSample) -> Unit
) {

    private var lastButtonsRaw = 0  // bitmask straight from MotionEvent

    /** A normalized pen sample ready for wire encoding. */
    data class PenSample(
        val tsNs: Long,
        val phase: Int,         // 0=hover, 1=down, 2=move, 3=up, 4=leave
        val xNorm: Float,
        val yNorm: Float,
        val pressure: Float,
        val tiltX: Float,
        val tiltY: Float,
        val buttons: Int,       // bit0=btn1, bit1=btn2, bit2=btn3 (decoded)
        val tool: Int           // 0=tip, 1=eraser end
    )

    fun consume(ev: MotionEvent): Boolean {
        if (ev.getToolType(0) !in TOOL_TYPES) return false

        val tool = if (ev.getToolType(0) == MotionEvent.TOOL_TYPE_ERASER) 1 else 0
        val phase = mapPhase(ev.actionMasked)

        val w = viewWidth().coerceAtLeast(1).toFloat()
        val h = viewHeight().coerceAtLeast(1).toFloat()

        // pressure & orientation/tilt are reported per-pointer
        val pressure = ev.pressure.coerceIn(0f, 1f)
        // Android encodes tilt as a single AXIS_TILT (radians, 0..π/2 with
        // AXIS_ORIENTATION giving the direction). Convert to (tiltX, tiltY).
        val tilt = ev.getAxisValue(MotionEvent.AXIS_TILT)
        val orient = ev.getAxisValue(MotionEvent.AXIS_ORIENTATION)
        val tiltX = (Math.sin(orient.toDouble()) * tilt).toFloat()
        val tiltY = (-Math.cos(orient.toDouble()) * tilt).toFloat()

        val rawButtons = ev.buttonState
        val newPrimary = (rawButtons and MotionEvent.BUTTON_STYLUS_PRIMARY) != 0
        val newSecondary = (rawButtons and MotionEvent.BUTTON_STYLUS_SECONDARY) != 0
        val newTertiary = (rawButtons and MotionEvent.BUTTON_TERTIARY) != 0
        val oldPrimary = (lastButtonsRaw and MotionEvent.BUTTON_STYLUS_PRIMARY) != 0
        val oldSecondary = (lastButtonsRaw and MotionEvent.BUTTON_STYLUS_SECONDARY) != 0

        val primaryDown = newPrimary && !oldPrimary
        val secondaryDown = newSecondary && !oldSecondary
        val chordTransition = primaryDown && secondaryDown

        val decodedButtons = decodeButtons(
            newPrimary, newSecondary, newTertiary, chordTransition
        )
        lastButtonsRaw = rawButtons

        // emit historical samples first (Android may batch them at high rates)
        val historySize = ev.historySize
        for (i in 0 until historySize) {
            onEvent(
                PenSample(
                    tsNs = ev.getHistoricalEventTime(i) * 1_000_000L,
                    phase = phase,
                    xNorm = (ev.getHistoricalX(0, i) / w).coerceIn(0f, 1f),
                    yNorm = (ev.getHistoricalY(0, i) / h).coerceIn(0f, 1f),
                    pressure = ev.getHistoricalPressure(0, i).coerceIn(0f, 1f),
                    tiltX = tiltX,
                    tiltY = tiltY,
                    buttons = decodedButtons,
                    tool = tool
                )
            )
        }

        onEvent(
            PenSample(
                tsNs = ev.eventTime * 1_000_000L,
                phase = phase,
                xNorm = (ev.x / w).coerceIn(0f, 1f),
                yNorm = (ev.y / h).coerceIn(0f, 1f),
                pressure = pressure,
                tiltX = tiltX,
                tiltY = tiltY,
                buttons = decodedButtons,
                tool = tool
            )
        )
        return true
    }

    private fun mapPhase(action: Int): Int = when (action) {
        MotionEvent.ACTION_HOVER_ENTER, MotionEvent.ACTION_HOVER_MOVE -> 0
        MotionEvent.ACTION_DOWN, MotionEvent.ACTION_POINTER_DOWN -> 1
        MotionEvent.ACTION_MOVE -> 2
        MotionEvent.ACTION_UP, MotionEvent.ACTION_POINTER_UP -> 3
        MotionEvent.ACTION_HOVER_EXIT, MotionEvent.ACTION_CANCEL -> 4
        else -> 2
    }

    /**
     * Returns a 3-bit mask: bit0=btn1, bit1=btn2, bit2=btn3.
     *
     * Primary path (MovinkPad firmware): btn3 = BUTTON_TERTIARY set.
     * Defensive chord fallback (older Wacom desktop firmware): both stylus bits
     * transitioned 0→1 in the same event ⇒ btn3, with stylus bits suppressed
     * so the PC doesn't see a phantom btn1+btn2 combo.
     */
    private fun decodeButtons(
        nowPrimary: Boolean,
        nowSecondary: Boolean,
        nowTertiary: Boolean,
        chordTransition: Boolean,
    ): Int {
        if (chordTransition) {
            // Older firmware: both stylus bits flipped at once = btn3.
            return 0b100
        }
        var bits = 0
        if (nowPrimary)  bits = bits or 0b001
        if (nowSecondary) bits = bits or 0b010
        if (nowTertiary)  bits = bits or 0b100
        return bits
    }

    companion object {
        private val TOOL_TYPES = intArrayOf(
            MotionEvent.TOOL_TYPE_STYLUS,
            MotionEvent.TOOL_TYPE_ERASER
        )
    }
}
