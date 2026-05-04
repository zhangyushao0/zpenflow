package dev.penflow

import android.app.Activity
import android.os.Bundle
import android.util.Log
import android.view.MotionEvent
import android.view.SurfaceHolder
import android.view.SurfaceView
import android.view.View
import android.view.WindowManager
import android.widget.TextView
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.SupervisorJob
import kotlinx.coroutines.cancel
import kotlinx.coroutines.launch

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

    /**
     * USB accessory streams. Held as a member so the underlying
     * ParcelFileDescriptor isn't GC'd while the read coroutines on
     * `client` are still using its file descriptor — `FileInputStream`
     * and `FileOutputStream` keep weak refs to the PFD via the FD only,
     * so when the only strong ref (this field) drops, the PFD's
     * finalizer fires and closes the FD out from under the readers
     * (manifests as `InterruptedIOException: read interrupted by close()
     * on another thread` after ~5-10 s of GC delay).
     */
    private var usbStreams: UsbAccessoryConnection.Streams? = null

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
            viewWidth = { root.width },
            viewHeight = { root.height },
            onEvent = { sample ->
                client.sendPenEvent(sample)
            }
        )

        touchCapture = TouchInputCapture(
            viewWidth = { root.width },
            viewHeight = { root.height },
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
        )
    }

    override fun onStart() {
        super.onStart()
        // Prefer USB accessory transport when an Android Open Accessory
        // device is plugged in: bypasses ADB entirely (no daemon, no
        // localabstract socket, no TCP-over-USB framing). Falls back to
        // ADB localabstract for development / when no accessory is
        // present.
        //
        // Two paths to detect an accessory:
        //   1. We were *launched* by the platform's USB_ACCESSORY_ATTACHED
        //      intent (intent.action matches → accessory in extras).
        //   2. We were already foreground when the accessory plugged
        //      in / re-enumerated → ask UsbManager.
        val accessory = UsbAccessoryConnection.extractAccessoryFromIntent(intent)
            ?: UsbAccessoryConnection.firstConnectedAccessory(this)
        if (accessory != null) {
            connectViaUsbAccessory(accessory)
        } else {
            client.connect(detectDeviceCaps())
        }
    }

    private val activityScope = CoroutineScope(SupervisorJob() + Dispatchers.IO)

    private fun connectViaUsbAccessory(accessory: android.hardware.usb.UsbAccessory) {
        val ctx = this
        val caps = detectDeviceCaps()
        activityScope.launch {
            try {
                UsbAccessoryConnection.requestPermissionIfNeeded(ctx, accessory)
                val streams = UsbAccessoryConnection.open(ctx, accessory)
                // Keep a strong ref so PFD doesn't get finalized — see
                // `usbStreams` doc.
                usbStreams = streams
                Log.i(TAG, "USB accessory transport ready: ${streams.accessoryLabel}")
                client.connectViaStreams(streams.input, streams.output, caps)
            } catch (t: Throwable) {
                Log.e(TAG, "USB accessory connect failed; falling back to ADB", t)
                runOnUiThread { client.connect(caps) }
            }
        }
    }

    override fun onStop() {
        client.disconnect()
        super.onStop()
    }

    override fun onDestroy() {
        client.close()
        usbStreams?.close()
        usbStreams = null
        activityScope.cancel()
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
