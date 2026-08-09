package dev.penflow

import android.app.Activity
import android.graphics.Rect
import android.os.Bundle
import android.util.Log
import android.view.Gravity
import android.view.MotionEvent
import android.view.SurfaceHolder
import android.view.SurfaceView
import android.view.View
import android.view.WindowManager
import android.widget.FrameLayout
import android.widget.TextView

/**
 * Top-level entry. Wires the Surface, the network client, and the pen
 * capture together. Phase 1 is intentionally minimal — connect on launch,
 * forward pen events, render incoming video.
 */
class MainActivity : Activity() {

    private lateinit var surfaceView: SurfaceView
    private lateinit var statusView: TextView
    private lateinit var client: PenflowClient
    private lateinit var penCapture: PenInputCapture
    private lateinit var touchCapture: TouchInputCapture

    @Volatile
    private var currentSurface: android.view.Surface? = null

    /** Rect (root-view pixels) the SurfaceView covers; smaller than the
     *  root when source aspect ≠ panel. Recomputed on each Connected. */
    @Volatile
    private var activeRect: Rect = Rect()

    /** Mirrors the PC's CLIENT_CONFIG SCREEN_OFF bit. Sticky per
     *  session; re-evaluated on reconnect. */
    @Volatile
    private var screenOff: Boolean = false

    /** True while the client state is Connected. Drives the keep-screen-on
     *  window flag together with [screenOff]. */
    private var sessionActive: Boolean = false

    /** Last client state, so transient pen-key debug text can revert. */
    private var lastState: PenflowClient.State = PenflowClient.State.Disconnected

    /** Handler for the stylus-key minimum-hold latch + debug revert. */
    private val stylusKeyHandler = android.os.Handler(android.os.Looper.getMainLooper())

    /** Keycode -> press wall-time, for the minimum-hold latch. */
    private val stylusKeysDown = HashMap<Int, Long>()

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)

        setContentView(R.layout.activity_main)
        surfaceView = findViewById(R.id.video_surface)
        statusView = findViewById(R.id.status)

        // Hide system UI for fullscreen pen-display experience.
        window.decorView.systemUiVisibility = (
            View.SYSTEM_UI_FLAG_HIDE_NAVIGATION or
            View.SYSTEM_UI_FLAG_FULLSCREEN or
            View.SYSTEM_UI_FLAG_IMMERSIVE_STICKY or
            View.SYSTEM_UI_FLAG_LAYOUT_STABLE or
            View.SYSTEM_UI_FLAG_LAYOUT_HIDE_NAVIGATION or
            View.SYSTEM_UI_FLAG_LAYOUT_FULLSCREEN
        )

        surfaceView.holder.addCallback(object : SurfaceHolder.Callback {
            override fun surfaceCreated(holder: SurfaceHolder) {
                currentSurface = holder.surface
                Log.i(TAG, "surface ready ${surfaceView.width}x${surfaceView.height}")
            }

            override fun surfaceChanged(holder: SurfaceHolder, fmt: Int, w: Int, h: Int) {
                currentSurface = holder.surface
            }

            override fun surfaceDestroyed(holder: SurfaceHolder) {
                currentSurface = null
            }
        })

        // Capture pen events anywhere on the root view.
        val root = findViewById<View>(android.R.id.content)
        root.isFocusable = true
        root.isFocusableInTouchMode = true

        penCapture = PenInputCapture(
            activeRect = { activeRect },
            onEvent = { sample ->
                client.sendPenEvent(sample)
            }
        )

        touchCapture = TouchInputCapture(
            activeRect = { activeRect },
            onSnapshot = { snap ->
                client.sendTouchSnapshot(snap)
            }
        )

        // Both touch and hover events go through dispatchGenericMotionEvent /
        // dispatchTouchEvent. Subclassing the root view would be cleaner;
        // for now we override the activity-level hooks below.

        val hud = findViewById<HudView>(R.id.hud)

        client = PenflowClient(
            abstractName = "penflow",
            onState = { st -> runOnUiThread { renderState(st) } },
            surfaceProvider = { currentSurface },
            hud = hud,
            onClientConfig = { cfg ->
                runOnUiThread {
                    val vis = if (cfg.hudEnabled) android.view.View.VISIBLE
                              else android.view.View.GONE
                    // The HUD toggle hides BOTH overlays the user sees on the
                    // tablet: the right-side latency panel (HudView) and the
                    // top-left status / resolution readout. They're separate
                    // Views but conceptually one "instrumentation overlay".
                    hud.visibility = vis
                    statusView.visibility = vis

                    // Screen-off: hide the video surface and dim the panel
                    // (no video will arrive). Pen + touch input still flow.
                    screenOff = cfg.screenOff
                    updateKeepScreenOn()
                    surfaceView.visibility =
                        if (cfg.screenOff) android.view.View.GONE
                        else android.view.View.VISIBLE
                    applyScreenBrightness(cfg.screenOff)
                }
            },
        )
    }

    override fun onStart() {
        super.onStart()
        client.connect(detectDeviceCaps())
    }

    override fun onStop() {
        client.disconnect()
        super.onStop()
    }

    override fun onDestroy() {
        client.close()
        super.onDestroy()
    }

    /** Map a stylus-ish keycode to our button bitmask (bit0..bit2).
     *
     *  - 632..635: Android 14 KEYCODE_STYLUS_BUTTON_PRIMARY/SECONDARY/
     *    TERTIARY/TAIL (raw ints so we compile on older SDKs).
     *  - >= 700: OEM extension range. Huawei/Honor pencils deliver their
     *    tap gestures here (M-Pencil 3 double-tap = 718). Mapped to btn1
     *    so the GUI's "Barrel button 1" binding drives it.
     */
    private fun stylusKeyToBits(keyCode: Int): Int = when (keyCode) {
        632 -> 0x1          // STYLUS_BUTTON_PRIMARY
        633 -> 0x2          // STYLUS_BUTTON_SECONDARY
        634 -> 0x4          // STYLUS_BUTTON_TERTIARY
        635 -> 0x4          // STYLUS_BUTTON_TAIL -> btn3
        else -> if (keyCode >= 700) 0x1 else 0
    }

    private fun refreshExternalButtons() {
        var bits = 0
        for (k in stylusKeysDown.keys) bits = bits or stylusKeyToBits(k)
        penCapture.externalButtonBits = bits
    }

    /** Show what the pen's key channel is doing — the discovery tool for
     *  non-Wacom pens. Visible whenever the status line is (HUD toggle). */
    private fun showPenKeyDebug(keyCode: Int, down: Boolean) {
        statusView.text = "pen key $keyCode ${if (down) "DOWN" else "UP"}"
        stylusKeyHandler.postDelayed({ renderState(lastState) }, 1500)
    }

    override fun dispatchKeyEvent(event: android.view.KeyEvent): Boolean {
        val bits = stylusKeyToBits(event.keyCode)
        if (bits == 0) return super.dispatchKeyEvent(event)

        android.util.Log.i(
            "PenflowPenKey",
            "stylus key ${event.keyCode} action=${event.action} " +
                "device=${event.device?.name}"
        )
        when (event.action) {
            android.view.KeyEvent.ACTION_DOWN -> {
                stylusKeysDown[event.keyCode] = android.os.SystemClock.uptimeMillis()
                refreshExternalButtons()
                showPenKeyDebug(event.keyCode, true)
            }
            android.view.KeyEvent.ACTION_UP -> {
                // Minimum-hold latch: a gesture "press" can be a few ms —
                // shorter than the gap between pen samples — and a press no
                // sample ever carried is a press the PC never sees. Hold the
                // bit at least 90 ms so it lands in the sample stream.
                val downAt = stylusKeysDown[event.keyCode]
                val heldMs = if (downAt != null)
                    android.os.SystemClock.uptimeMillis() - downAt else 999L
                val clearIn = (90L - heldMs).coerceAtLeast(0L)
                stylusKeyHandler.postDelayed({
                    stylusKeysDown.remove(event.keyCode)
                    refreshExternalButtons()
                }, clearIn)
                showPenKeyDebug(event.keyCode, false)
            }
        }
        return true
    }

    override fun dispatchTouchEvent(ev: MotionEvent): Boolean {
        // Pen events first (they use a different toolType so don't conflict with
        // touch). If the pen capture rejects the event (toolType=FINGER), fall
        // through to touch capture.
        if (penCapture.consume(ev)) return true
        if (touchCapture.consume(ev)) return true
        return super.dispatchTouchEvent(ev)
    }

    override fun dispatchGenericMotionEvent(ev: MotionEvent): Boolean {
        // Hover events from the pen go through here while not contacting.
        if (penCapture.consume(ev)) return true
        return super.dispatchGenericMotionEvent(ev)
    }

    private fun renderState(st: PenflowClient.State) {
        lastState = st
        sessionActive = st is PenflowClient.State.Connected
        updateKeepScreenOn()
        statusView.text = when (st) {
            PenflowClient.State.Disconnected -> "disconnected"
            PenflowClient.State.Connecting -> "connecting…"
            is PenflowClient.State.Connected -> if (screenOff)
                "pen tablet — display off (${st.width}x${st.height} target)"
            else
                "connected ${st.width}x${st.height}@${st.fps}"
            is PenflowClient.State.Error -> "error: ${st.message}"
        }
        if (st is PenflowClient.State.Connected) {
            // Run the contain layout in both modes so activeRect preserves
            // the target monitor's aspect ratio — otherwise pen strokes
            // would be stretched in screen_off when panel and monitor
            // aspects differ. SurfaceView resize is a no-op when GONE.
            // TODO: expose a "Mapping" setting in the GUI (aspect-fit /
            // stretch / custom rect) so power users can pick.
            applyContainLayout(st.width, st.height)
        }
    }

    /** Per-window brightness override; no WRITE_SETTINGS needed. */
    /**
     * Keep the panel awake ONLY while a session is live and actually
     * showing video. An earlier unconditional FLAG_KEEP_SCREEN_ON in
     * onCreate was removed (93ea1d2) because it pinned the screen awake
     * for the whole app lifetime — including while sitting disconnected
     * and in screen-off pen-tablet mode, whose entire purpose is letting
     * the panel rest. Scoping the flag to (Connected && !screenOff)
     * keeps that fix intact while stopping the display timeout from
     * blanking the tablet mid-session: a pen display that dozes off
     * between strokes is not usable as a display.
     *
     * A window flag needs no permissions and cannot leak: the flag dies
     * with the window, and every path out of Connected (disconnect,
     * error, reconnect) lands in renderState which clears it.
     */
    private fun updateKeepScreenOn() {
        val keep = sessionActive && !screenOff
        if (keep) {
            window.addFlags(WindowManager.LayoutParams.FLAG_KEEP_SCREEN_ON)
        } else {
            window.clearFlags(WindowManager.LayoutParams.FLAG_KEEP_SCREEN_ON)
        }
    }

    private fun applyScreenBrightness(dim: Boolean) {
        val lp = window.attributes
        val target = if (dim) {
            WindowManager.LayoutParams.BRIGHTNESS_OVERRIDE_OFF
        } else {
            WindowManager.LayoutParams.BRIGHTNESS_OVERRIDE_NONE
        }
        if (lp.screenBrightness != target) {
            lp.screenBrightness = target
            window.attributes = lp
            Log.i(TAG, "screen_off=$dim — brightness override = $target")
        }
    }

    /** Contain-fit the SurfaceView to source dimensions. Posted to root
     *  so layout has finished — `Connected` can fire before `onLayout`. */
    private fun applyContainLayout(sourceWidth: Int, sourceHeight: Int) {
        if (sourceWidth <= 0 || sourceHeight <= 0) return
        val root = findViewById<View>(android.R.id.content)
        root.post {
            val pw = root.width
            val ph = root.height
            if (pw <= 0 || ph <= 0) return@post

            // contain: smaller scale fits both axes; other axis = bars.
            val scale = minOf(pw.toFloat() / sourceWidth, ph.toFloat() / sourceHeight)
            val rectW = (sourceWidth * scale).toInt().coerceAtLeast(1)
            val rectH = (sourceHeight * scale).toInt().coerceAtLeast(1)
            val left = (pw - rectW) / 2
            val top = (ph - rectH) / 2

            activeRect = Rect(left, top, left + rectW, top + rectH)

            val lp = surfaceView.layoutParams as? FrameLayout.LayoutParams
                ?: FrameLayout.LayoutParams(rectW, rectH)
            lp.width = rectW
            lp.height = rectH
            lp.gravity = Gravity.CENTER
            surfaceView.layoutParams = lp

            Log.i(TAG, "contain layout: panel=${pw}x${ph} source=${sourceWidth}x${sourceHeight} active=$activeRect")
        }
    }

    /**
     * Reports our static device capabilities to the PC. These are read
     * from the actual InputDevice when possible, with safe defaults for
     * the Wacom Pro Pen 3 if no device is enumerated yet.
     */
    private fun detectDeviceCaps(): PenflowClient.DeviceCaps {
        val display = windowManager.defaultDisplay
        val size = android.graphics.Point()
        @Suppress("DEPRECATION")
        display.getRealSize(size)

        // Defaults match Wacom Pro Pen 3 specs. Android InputDevice
        // normalizes pressure to 0..1, so reading getMotionRange().max
        // for AXIS_PRESSURE always yields 1.0 — useless. We hardcode the
        // raw resolution because PEN_EVENT carries normalized floats over
        // the wire anyway, and this field is informational for the PC.
        val pressureMax = 8191
        var tiltMin = -90
        var tiltMax = 90
        val buttons = 3

        // Read real tilt range from any present stylus InputDevice.
        for (id in android.view.InputDevice.getDeviceIds()) {
            val dev = android.view.InputDevice.getDevice(id) ?: continue
            if (dev.sources and android.view.InputDevice.SOURCE_STYLUS == 0) continue
            dev.getMotionRange(MotionEvent.AXIS_TILT)?.let {
                // AXIS_TILT in Android is reported in radians.
                tiltMin = Math.toDegrees(it.min.toDouble()).toInt()
                tiltMax = Math.toDegrees(it.max.toDouble()).toInt()
            }
            break
        }

        return PenflowClient.DeviceCaps(
            displayWidth = size.x,
            displayHeight = size.y,
            penMaxPressure = pressureMax,
            penTiltMinDeg = tiltMin,
            penTiltMaxDeg = tiltMax,
            penButtonsCount = buttons,
            codecCaps = MediaCodecCaps.queryHardwareDecodeBitmask(),
        )
    }

    companion object {
        private const val TAG = "MainActivity"
    }
}
