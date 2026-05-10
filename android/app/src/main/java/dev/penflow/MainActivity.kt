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

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        window.addFlags(WindowManager.LayoutParams.FLAG_KEEP_SCREEN_ON)

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
        statusView.text = when (st) {
            PenflowClient.State.Disconnected -> "disconnected"
            PenflowClient.State.Connecting -> "connecting…"
            is PenflowClient.State.Connected -> "connected ${st.width}x${st.height}@${st.fps}"
            is PenflowClient.State.Error -> "error: ${st.message}"
        }
        if (st is PenflowClient.State.Connected) {
            applyContainLayout(st.width, st.height)
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
