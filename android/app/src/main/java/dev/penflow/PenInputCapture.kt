package dev.penflow

import android.graphics.Rect
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
 *
 * **Pointer-index handling**: Android merges pen and finger events into the
 * same [MotionEvent] when they happen simultaneously. The "primary pointer"
 * (index 0) is whichever physical contact landed first — if the user's palm
 * touches before the pen tip, the palm becomes index 0 and the pen ends up
 * at a higher index. The earlier `getToolType(0)` check made us drop the
 * entire event in that case (visible symptom: strokes leaving no mark when
 * the palm rests on the screen first). We now scan all pointers, latch on
 * the first STYLUS/ERASER index, and decode phase from the action's
 * `actionIndex` relative to that — preserving correct DOWN/UP transitions
 * when fingers join or leave a gesture mid-stroke. This is the
 * software-side analogue of the firmware-level "pen present → suppress
 * touch" behaviour real Wacom tablets do in hardware.
 */
class PenInputCapture(
    /** Rect (root-view pixels) the decoded video covers. Events outside
     *  the rect are dropped; events inside are normalized against it. */
    private val activeRect: () -> Rect,
    private val onEvent: (PenSample) -> Unit
) {

    private var lastButtonsRaw = 0  // bitmask straight from MotionEvent

    // Spatial dead zone after stylus lift (HANDOFF §1.3, design §10.7).
    // Without this, fast tap-and-go strokes can register a phantom second
    // contact within ~5px of the lift coordinates as a double-click.
    // Moonlight enforces ~5 px — adopting the same value.
    private var lastUpX: Float = Float.NaN
    private var lastUpY: Float = Float.NaN
    private var lastUpTimeMs: Long = 0L

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
        // Find the stylus pointer at any index — not just 0. With palm-first
        // contact, the pen lands at index 1+ and the old `getToolType(0)`
        // check would silently drop the entire stroke.
        val penIndex = findPenIndex(ev)
        if (penIndex < 0) return false

        val tool = if (ev.getToolType(penIndex) == MotionEvent.TOOL_TYPE_ERASER) 1 else 0
        val phase = mapPhase(ev, penIndex)

        // Dead-zone gate: suppress new DOWN/HOVER samples that land within
        // DEAD_ZONE_PX of the last UP, within DEAD_ZONE_MS. Tap-then-tap
        // sequences come through reliably (separated by enough time);
        // accidental "fast lift, finger trembled, brief re-contact" does
        // not (HANDOFF §1.3 / design §10.7).
        val penX = ev.getX(penIndex)
        val penY = ev.getY(penIndex)
        val isFreshContact = phase == 1 || phase == 0  // DOWN or HOVER_*
        if (isFreshContact && !lastUpX.isNaN()) {
            val dx = penX - lastUpX
            val dy = penY - lastUpY
            val dt = ev.eventTime - lastUpTimeMs
            if (dt < DEAD_ZONE_MS && (dx * dx + dy * dy) < DEAD_ZONE_PX_SQ) {
                return true  // consume but emit nothing
            }
        }
        if (phase == 3) {  // UP at the pen's index
            lastUpX = penX
            lastUpY = penY
            lastUpTimeMs = ev.eventTime
        }

        // Drop events in the letterbox bars; normalize against activeRect.
        // Empty rect = pre-handshake; fall through to root-view bounds.
        val rect = activeRect()
        val rectW = rect.width().coerceAtLeast(1).toFloat()
        val rectH = rect.height().coerceAtLeast(1).toFloat()
        val rectL = rect.left.toFloat()
        val rectT = rect.top.toFloat()
        if (rect.width() > 0 && rect.height() > 0) {
            if (penX < rectL || penX > rectL + rectW
                || penY < rectT || penY > rectT + rectH
            ) {
                return true  // in a bar — consume but emit nothing
            }
        }

        // pressure & orientation/tilt are reported per-pointer
        val pressure = ev.getPressure(penIndex).coerceIn(0f, 1f)
        // Android reports AXIS_TILT in **radians** (0..π/2; 0 = perpendicular,
        // π/2 = laying flat) and AXIS_ORIENTATION in **radians** for the
        // azimuth. The wire protocol carries Tilt-X / Tilt-Y in **degrees**
        // (signed, ±90 max) — that's what the PC injector forwards verbatim
        // to `POINTER_PEN_INFO.tiltX/tiltY`, which Win32 also documents as
        // degrees. Decompose tilt into X/Y components AND convert to degrees
        // here; the previous code skipped the radians→degrees step, which
        // meant we were sending values in the [-1.57, +1.57] range as if
        // they were degrees and tilt-aware brushes (Rebelle, Clip Studio
        // Paint) saw effectively zero tilt (issue #5).
        val tiltRad = ev.getAxisValue(MotionEvent.AXIS_TILT, penIndex)
        val orient = ev.getAxisValue(MotionEvent.AXIS_ORIENTATION, penIndex)
        val tiltDeg = Math.toDegrees(tiltRad.toDouble())
        val tiltX = (Math.sin(orient.toDouble()) * tiltDeg).toFloat()
        val tiltY = (-Math.cos(orient.toDouble()) * tiltDeg).toFloat()

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

        // Emit historical samples first (Android batches at high rates),
        // each rect-clipped so a brief excursion produces a gap, not a teleport.
        val historySize = ev.historySize
        for (i in 0 until historySize) {
            val hx = ev.getHistoricalX(penIndex, i)
            val hy = ev.getHistoricalY(penIndex, i)
            if (rect.width() > 0 && rect.height() > 0) {
                if (hx < rectL || hx > rectL + rectW || hy < rectT || hy > rectT + rectH) continue
            }
            onEvent(
                PenSample(
                    tsNs = ev.getHistoricalEventTime(i) * 1_000_000L,
                    phase = phase,
                    xNorm = ((hx - rectL) / rectW).coerceIn(0f, 1f),
                    yNorm = ((hy - rectT) / rectH).coerceIn(0f, 1f),
                    pressure = ev.getHistoricalPressure(penIndex, i).coerceIn(0f, 1f),
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
                xNorm = ((penX - rectL) / rectW).coerceIn(0f, 1f),
                yNorm = ((penY - rectT) / rectH).coerceIn(0f, 1f),
                pressure = pressure,
                tiltX = tiltX,
                tiltY = tiltY,
                buttons = decodedButtons,
                tool = tool
            )
        )
        return true
    }

    /** First pointer index whose toolType is STYLUS or ERASER, or -1. */
    private fun findPenIndex(ev: MotionEvent): Int {
        for (i in 0 until ev.pointerCount) {
            if (ev.getToolType(i) in TOOL_TYPES) return i
        }
        return -1
    }

    /**
     * Map an Android action to our wire phase, considering whether the
     * action is about the pen pointer or about another (finger) pointer
     * coexisting with it.
     *
     * `ACTION_POINTER_DOWN` / `ACTION_POINTER_UP` carry an [actionIndex];
     * if it matches `penIndex`, the pen itself is going down/up. If it
     * doesn't, a non-pen pointer joined or left a gesture and the pen
     * state is unchanged — we emit `move` so the PC keeps tracking the
     * pen's coordinates without spurious DOWN/UP transitions.
     */
    private fun mapPhase(ev: MotionEvent, penIndex: Int): Int {
        val action = ev.actionMasked
        val isPenAction = ev.actionIndex == penIndex
        return when (action) {
            MotionEvent.ACTION_HOVER_ENTER, MotionEvent.ACTION_HOVER_MOVE -> 0
            MotionEvent.ACTION_DOWN -> if (isPenAction) 1 else 2
            MotionEvent.ACTION_POINTER_DOWN -> if (isPenAction) 1 else 2
            MotionEvent.ACTION_MOVE -> 2
            MotionEvent.ACTION_UP -> if (isPenAction) 3 else 2
            MotionEvent.ACTION_POINTER_UP -> if (isPenAction) 3 else 2
            MotionEvent.ACTION_HOVER_EXIT, MotionEvent.ACTION_CANCEL -> 4
            else -> 2
        }
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

        /** Spatial dead zone radius in view pixels (squared for cheap compare). */
        private const val DEAD_ZONE_PX_SQ = 5f * 5f
        /** Time window in ms during which the dead zone applies after ACTION_UP. */
        private const val DEAD_ZONE_MS = 80L
    }
}
