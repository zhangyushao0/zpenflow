package dev.penflow

import android.graphics.Rect
import android.util.Log
import android.view.MotionEvent

/**
 * Translates Android multi-finger touch [MotionEvent]s into wire-format
 * touch snapshots.
 *
 * Each consumed event emits a *snapshot* of all currently active fingers.
 * The PC server diffs successive snapshots to compute DOWN / MOVE / UP
 * transitions for the WinRT InputInjector touch API. This avoids encoding
 * Android's per-event action semantics (ACTION_POINTER_DOWN, _UP, etc.) into
 * the wire — server only cares about "what fingers are currently down".
 *
 * On `ACTION_POINTER_UP` / `ACTION_UP`, the lifted pointer is **excluded**
 * from the snapshot (Android keeps it in the pointer list of that single
 * event for the action-index lookup). On `ACTION_CANCEL` we emit an empty
 * snapshot so the server lifts everything.
 *
 * Stylus / pen events are handled exclusively by [PenInputCapture] — the
 * activity dispatches to it first, so by the time we run the event has no
 * stylus pointers in it. As cheap defence-in-depth we still filter the
 * per-contact loop to `TOOL_TYPE_FINGER`, in case an event slips through
 * with mixed tool types (e.g. a `TOOL_TYPE_PALM` at index 0 plus real
 * fingers at higher indices) and we'd otherwise inject a non-finger
 * coordinate as a touch contact on the PC.
 */
class TouchInputCapture(
    /** Same semantics as [PenInputCapture.activeRect] — fingers landing in
     *  the letterbox bars are excluded from snapshots. */
    private val activeRect: () -> Rect,
    private val onSnapshot: (TouchSnapshot) -> Unit,
) {
    data class TouchSnapshot(
        val tsNs: Long,
        val contacts: List<Protocol.TouchContact>,
    )

    fun consume(ev: MotionEvent): Boolean {
        // Diagnostic: log every event reaching us so we can verify finger
        // events are actually arriving and have the expected toolType.
        Log.d(TAG, "ev action=${ev.actionMasked} pointers=${ev.pointerCount} " +
            "tool0=${ev.getToolType(0)} (FINGER=${MotionEvent.TOOL_TYPE_FINGER} " +
            "STYLUS=${MotionEvent.TOOL_TYPE_STYLUS})")
        if (ev.getToolType(0) != MotionEvent.TOOL_TYPE_FINGER) return false

        val rect = activeRect()
        val rectW = rect.width().coerceAtLeast(1).toFloat()
        val rectH = rect.height().coerceAtLeast(1).toFloat()
        val rectL = rect.left.toFloat()
        val rectT = rect.top.toFloat()
        val haveRect = rect.width() > 0 && rect.height() > 0

        val contacts: List<Protocol.TouchContact> = when (ev.actionMasked) {
            MotionEvent.ACTION_CANCEL -> emptyList()
            else -> {
                // The pointer at actionIndex is lifted on POINTER_UP / UP and
                // must not appear in the next "currently active" snapshot.
                val liftedIndex = when (ev.actionMasked) {
                    MotionEvent.ACTION_POINTER_UP, MotionEvent.ACTION_UP -> ev.actionIndex
                    else -> -1
                }
                val n = ev.pointerCount
                val list = ArrayList<Protocol.TouchContact>(n)
                for (i in 0 until n) {
                    if (i == liftedIndex) continue
                    // Only forward genuine finger contacts — drops any
                    // stylus/eraser/palm pointer that managed to coexist
                    // with fingers in this event so we never inject a
                    // non-finger position as a touch on the PC side.
                    if (ev.getToolType(i) != MotionEvent.TOOL_TYPE_FINGER) continue
                    val fx = ev.getX(i)
                    val fy = ev.getY(i)
                    if (haveRect) {
                        if (fx < rectL || fx > rectL + rectW
                            || fy < rectT || fy > rectT + rectH
                        ) continue
                    }
                    list.add(
                        Protocol.TouchContact(
                            pointerId = ev.getPointerId(i),
                            xNorm = ((fx - rectL) / rectW).coerceIn(0f, 1f),
                            yNorm = ((fy - rectT) / rectH).coerceIn(0f, 1f),
                            pressure = ev.getPressure(i).coerceIn(0f, 1f),
                        )
                    )
                }
                list
            }
        }

        Log.d(TAG, "snapshot: ${contacts.size} contacts -> sending")
        onSnapshot(TouchSnapshot(ev.eventTime * 1_000_000L, contacts))
        return true
    }

    companion object {
        private const val TAG = "TouchInputCapture"
    }
}
