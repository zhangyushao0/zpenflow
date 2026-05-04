package dev.penflow

import android.net.LocalSocket
import android.net.LocalSocketAddress
import android.os.SystemClock
import android.util.Log
import android.view.Surface
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.Job
import kotlinx.coroutines.SupervisorJob
import kotlinx.coroutines.cancel
import kotlinx.coroutines.channels.Channel
import kotlinx.coroutines.launch
import kotlinx.coroutines.sync.Mutex
import kotlinx.coroutines.sync.withLock
import kotlinx.coroutines.withContext
import java.io.DataInputStream
import java.io.DataOutputStream

/**
 * Owns the local-abstract socket to the PC server and pumps:
 *
 *  Inbound:  HELLO_PC, VIDEO_CONFIG, VIDEO_FRAME → MediaCodec
 *  Outbound: HELLO_ANDROID once, then PEN_EVENT for every pen sample
 */
class PenflowClient(
    private val abstractName: String = "penflow",
    private val onState: (State) -> Unit,
    private val surfaceProvider: () -> Surface?,
    private val hud: HudView? = null,
) {

    sealed class State {
        object Disconnected : State()
        object Connecting : State()
        data class Connected(val width: Int, val height: Int, val fps: Int) : State()
        data class Error(val message: String) : State()
    }

    private val scope = CoroutineScope(SupervisorJob() + Dispatchers.IO)
    private var socket: LocalSocket? = null
    private var output: DataOutputStream? = null
    private var decoder: VideoDecoder? = null
    private val sendMutex = Mutex()

    private var readerJob: Job? = null
    private var timeSyncJob: Job? = null
    private var penSendJob: Job? = null
    private var touchSendJob: Job? = null
    private val timeSync = TimeSync()

    // FIFO queue from MotionEvent dispatch (UI thread) to a single consumer
    // coroutine that performs the actual socket send. Replaces the previous
    // per-event `scope.launch { ... }` pattern, which delivered events out of
    // order whenever the IO dispatcher scheduled them across multiple cores.
    //
    // UNLIMITED capacity: at 240 Hz × 31 bytes/event = 7.5 KB/s, even seconds
    // of consumer lag fits comfortably in memory. Dropping samples on
    // back-pressure would cause the same "flying line" artefact the channel
    // is meant to fix.
    private val penChannel: Channel<PenInputCapture.PenSample> =
        Channel(Channel.UNLIMITED)

    // Same FIFO discipline as pen events. Touch state is order-sensitive (a
    // late-arriving "down" after a "move" would confuse the server-side
    // diff that derives DOWN/UP transitions).
    private val touchChannel: Channel<TouchInputCapture.TouchSnapshot> =
        Channel(Channel.UNLIMITED)

    private data class PendingSample(
        val ptsNs: Long, val captureUs: Int?, val encodeUs: Int?, val recvNs: Long,
    )

    // Keyed by PC `pts_ns`. Earlier this was a FIFO `ConcurrentLinkedQueue`,
    // but MediaCodec on the MovinkPad is not strictly 1-input/1-output for
    // ultra-static content: when the encoder hands it a long stream of
    // identical keepalive frames the decoder occasionally elides an output
    // (1 input → 0 callbacks). With FIFO matching, every dropped output left
    // one extra sample on the queue, head-of-queue grew progressively older,
    // and `dec_us = decodedNs - recvNs_FIFO_head` ratcheted up monotonically
    // — exactly the "stays high once it goes high" symptom.
    //
    // Match by PC `pts_ns` instead: each MSG_VIDEO_FRAME puts the sample
    // keyed by its server-stamped PTS, and the codec callback looks up by
    // the same PTS via `info.presentationTimeUs * 1000` (we feed PC-PTS
    // into `queueInputBuffer`, MediaCodec round-trips it on the output
    // sample). Robust against any input/output count mismatch.
    private val pendingFrameSamples =
        java.util.concurrent.ConcurrentHashMap<Long, PendingSample>()

    /** Drop pending samples older than this; keeps the map bounded if the
     * decoder genuinely drops outputs forever. 1 second ≈ 60 frames at our
     * default fps which is plenty of slack for any transient hiccup. */
    private val pendingSampleMaxAgeNs = 1_000_000_000L

    private fun onDecoderFrameDone(framePtsNs: Long, decodedNs: Long) {
        val s = pendingFrameSamples.remove(framePtsNs) ?: return
        // Approximate displayedNs as decodedNs + 1 vsync (8.33 ms @ 120 Hz).
        val displayedNs = decodedNs + 8_333_333L
        hud?.recordFrameSample(
            s.ptsNs, s.captureUs, s.encodeUs, s.recvNs, decodedNs, displayedNs,
            timeSync.pcMinusAndroidNs, timeSync.isReady(),
        )
    }

    fun connect(deviceCaps: DeviceCaps) {
        scope.launch {
            try {
                onState(State.Connecting)
                val sock = LocalSocket().apply {
                    connect(LocalSocketAddress(abstractName, LocalSocketAddress.Namespace.ABSTRACT))
                }
                socket = sock
                val out = DataOutputStream(sock.outputStream)
                val input = DataInputStream(sock.inputStream)
                output = out

                // 1. send HELLO_ANDROID
                Protocol.sendMsg(
                    out,
                    Protocol.MSG_HELLO_ANDROID,
                    Protocol.encodeHelloAndroid(
                        protocolVersion = 0,
                        displayWidth = deviceCaps.displayWidth,
                        displayHeight = deviceCaps.displayHeight,
                        penMaxPressure = deviceCaps.penMaxPressure,
                        penTiltMinDeg = deviceCaps.penTiltMinDeg,
                        penTiltMaxDeg = deviceCaps.penTiltMaxDeg,
                        penButtonsCount = deviceCaps.penButtonsCount,
                        codecCaps = deviceCaps.codecCaps,
                    )
                )

                // 2. wait for HELLO_PC
                val (helloType, helloPayload) = Protocol.recvMsg(input)
                require(helloType == Protocol.MSG_HELLO_PC) {
                    "expected HELLO_PC, got 0x${"%02x".format(helloType.toInt() and 0xFF)}"
                }
                val hello = Protocol.decodeHelloPc(helloPayload)
                Log.i(TAG, "HELLO_PC ${hello.width}x${hello.height}@${hello.fps} codec=${hello.codec}")

                // 3. wait for VIDEO_CONFIG (csd-0)
                var csd0: ByteArray? = null
                while (csd0 == null) {
                    val (type, payload) = Protocol.recvMsg(input)
                    if (type == Protocol.MSG_VIDEO_CONFIG) {
                        csd0 = payload
                    } else {
                        Log.w(TAG, "expected VIDEO_CONFIG, got 0x${"%02x".format(type.toInt() and 0xFF)}; dropping")
                    }
                }

                // 4. start the decoder once we have a Surface to render to
                val surface = waitForSurface()
                val dec = VideoDecoder(
                    hello.width, hello.height, hello.fps, hello.codec, surface, csd0!!,
                    onDecoded = { framePtsNs, decodedNs -> onDecoderFrameDone(framePtsNs, decodedNs) },
                )
                dec.start()
                decoder = dec
                onState(State.Connected(hello.width, hello.height, hello.fps))

                // 5. read frames forever
                readerJob = scope.launch { readLoop(input, dec) }

                // 6. start periodic time-sync ping (1 Hz)
                timeSyncJob = scope.launch { timeSyncLoop(out) }

                // 7. single-consumer pen-event sender (preserves FIFO order)
                penSendJob = scope.launch { penSendLoop(out) }

                // 8. single-consumer touch sender
                touchSendJob = scope.launch { touchSendLoop(out) }
            } catch (t: Throwable) {
                Log.e(TAG, "connect failed", t)
                onState(State.Error(t.message ?: t.javaClass.simpleName))
                disconnect()
            }
        }
    }

    private suspend fun waitForSurface(): Surface {
        // Spin until the SurfaceView has produced a surface. This usually
        // resolves in a few frames; the activity creates it eagerly.
        for (i in 0 until 200) {
            val s = surfaceProvider()
            if (s != null && s.isValid) return s
            kotlinx.coroutines.delay(25)
        }
        error("timed out waiting for output Surface")
    }

    private suspend fun readLoop(input: DataInputStream, dec: VideoDecoder) {
        while (true) {
            val (type, payload) = Protocol.recvMsg(input)
            when (type) {
                Protocol.MSG_VIDEO_FRAME -> {
                    val recvNs = System.nanoTime()
                    val header = Protocol.decodeVideoFrame(payload)
                    // Record sample BEFORE feeding the decoder — the codec
                    // callback can fire as soon as feed() returns, and we
                    // want the matching entry already in the map.
                    if (hud != null) {
                        pendingFrameSamples[header.ptsNs] =
                            PendingSample(header.ptsNs, header.captureUs, header.encodeUs, recvNs)
                        // Evict samples older than `pendingSampleMaxAgeNs`
                        // so the map stays bounded if MediaCodec ever
                        // permanently elides outputs (e.g. for some
                        // long static run). O(n) on the map but n is
                        // tiny in steady state (≤ 2 frames in flight).
                        val cutoff = header.ptsNs - pendingSampleMaxAgeNs
                        pendingFrameSamples.keys.removeAll { it < cutoff }
                    }
                    dec.feed(header.ptsNs, header.coded)
                }
                Protocol.MSG_TELEMETRY -> {
                    hud?.recordServerTelemetry(Protocol.decodeTelemetry(payload))
                }
                Protocol.MSG_TIME_SYNC_RESP -> {
                    val t4 = System.nanoTime()
                    val resp = Protocol.decodeTimeSyncResp(payload)
                    timeSync.observe(resp.androidT1Ns, resp.pcT2Ns, resp.pcT3Ns, t4)
                }
                Protocol.MSG_VIDEO_CONFIG -> {
                    Log.w(TAG, "VIDEO_CONFIG mid-stream not yet handled (${payload.size} bytes)")
                }
                Protocol.MSG_PC_GOODBYE -> {
                    Log.i(TAG, "PC sent goodbye")
                    return
                }
                else -> {
                    Log.d(TAG, "unhandled msg 0x${"%02x".format(type.toInt() and 0xFF)} len=${payload.size}")
                }
            }
        }
    }

    private suspend fun timeSyncLoop(out: DataOutputStream) {
        // Burst 5 pings at startup to converge offset quickly, then 1 Hz forever.
        repeat(5) {
            sendOneSync(out)
            kotlinx.coroutines.delay(50)
        }
        while (true) {
            sendOneSync(out)
            kotlinx.coroutines.delay(1000)
        }
    }

    private suspend fun sendOneSync(out: DataOutputStream) {
        val t1 = System.nanoTime()
        val payload = Protocol.encodeTimeSyncReq(t1)
        try {
            sendMutex.withLock {
                Protocol.sendMsg(out, Protocol.MSG_TIME_SYNC_REQ, payload)
            }
        } catch (t: Throwable) {
            Log.w(TAG, "time-sync send failed", t)
        }
    }

    /**
     * Called from the MotionEvent dispatch thread (UI thread) for every pen
     * sample, including high-rate historical batches. Just enqueues the sample;
     * the actual socket send happens in [penSendLoop] on a dedicated coroutine
     * to preserve FIFO order on the wire.
     */
    fun sendPenEvent(s: PenInputCapture.PenSample) {
        val r = penChannel.trySend(s)
        if (r.isFailure) Log.w(TAG, "pen channel rejected sample (closed?)")
    }

    /** Called from MotionEvent dispatch thread; just enqueues. */
    fun sendTouchSnapshot(snap: TouchInputCapture.TouchSnapshot) {
        val r = touchChannel.trySend(snap)
        if (r.isFailure) Log.w(TAG, "touch channel rejected snapshot (closed?)")
    }

    private suspend fun touchSendLoop(out: DataOutputStream) {
        for (snap in touchChannel) {
            val payload = Protocol.encodeTouchEvent(snap.tsNs, snap.contacts)
            try {
                sendMutex.withLock {
                    Protocol.sendMsg(out, Protocol.MSG_TOUCH_EVENT, payload)
                }
            } catch (t: Throwable) {
                Log.w(TAG, "touch send failed", t)
            }
        }
    }

    private suspend fun penSendLoop(out: DataOutputStream) {
        for (s in penChannel) {
            val payload = Protocol.encodePenEvent(
                tsNs = s.tsNs,
                phase = s.phase,
                x = s.xNorm,
                y = s.yNorm,
                pressure = s.pressure,
                tiltX = s.tiltX,
                tiltY = s.tiltY,
                buttonsBitmask = s.buttons,
                tool = s.tool
            )
            try {
                sendMutex.withLock {
                    Protocol.sendMsg(out, Protocol.MSG_PEN_EVENT, payload)
                }
            } catch (t: Throwable) {
                Log.w(TAG, "pen send failed", t)
            }
        }
    }

    fun disconnect() {
        touchSendJob?.cancel()
        penSendJob?.cancel()
        timeSyncJob?.cancel()
        readerJob?.cancel()
        try {
            output?.let {
                Protocol.sendMsg(it, Protocol.MSG_ANDROID_GOODBYE, ByteArray(0))
            }
        } catch (_: Throwable) {
        }
        try { socket?.close() } catch (_: Throwable) {}
        socket = null
        output = null
        decoder?.stop()
        decoder = null
        onState(State.Disconnected)
    }

    fun close() {
        disconnect()
        scope.cancel()
    }

    /** Static info about this device, sent in HELLO_ANDROID. */
    data class DeviceCaps(
        val displayWidth: Int,
        val displayHeight: Int,
        val penMaxPressure: Int,
        val penTiltMinDeg: Int,
        val penTiltMaxDeg: Int,
        val penButtonsCount: Int,
        val codecCaps: Int,
    )

    companion object {
        private const val TAG = "PenflowClient"
    }
}
