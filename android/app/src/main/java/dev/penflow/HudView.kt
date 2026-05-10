package dev.penflow

import android.content.Context
import android.os.SystemClock
import android.util.AttributeSet
import android.view.Choreographer
import android.widget.TextView

/**
 * Translucent fixed-position overlay that renders the running latency summary.
 *
 * The headline metric is **true end-to-end** latency: from the moment a frame
 * was captured on the PC (server-stamped pts_ns) to the moment it appears on
 * the Android display (decodedNs + 1 vsync). PC and Android clocks are bridged
 * by the NTP-style TimeSync ping/pong; until at least one sync sample arrives,
 * we show "—" instead of nonsense.
 *
 * Recv→display is also tracked separately as a lower-bound sanity check.
 */
class HudView @JvmOverloads constructor(
    context: Context,
    attrs: AttributeSet? = null,
) : TextView(context, attrs) {

    private val ringSize = 256
    private val ringTrueE2eUs = LongArray(ringSize)   // pts (PC, translated) → display
    private val ringRecvE2eUs = LongArray(ringSize)   // recv → display
    private val ringNetUs = LongArray(ringSize)       // pts (translated) → recv
    private val ringDecodeUs = LongArray(ringSize)    // recv → decoded
    private val ringDisplayUs = LongArray(ringSize)   // decoded → displayed (vsync est.)
    private val ringEncodeUs = IntArray(ringSize)     // server-reported encode_us
    private var ringHead = 0
    private val ringLock = Any()

    @Volatile private var serverTelemetry: Protocol.Telemetry? = null
    @Volatile private var timeSyncReady: Boolean = false

    /** Snapshot of TimeSync diagnostics last reported by [recordTimeSyncState].
     *  Surfaced in the HUD so operators can verify the offset estimator is
     *  healthy without scraping logcat. `null` until the first sample lands. */
    @Volatile private var timeSyncState: TimeSyncState? = null

    private data class TimeSyncState(
        val windowSamples: Int,
        val bestRttNs: Long,
        val oldestSampleAgeNs: Long,
    )

    private val frameCallback = object : Choreographer.FrameCallback {
        private var lastTickMs = 0L
        override fun doFrame(frameTimeNanos: Long) {
            val nowMs = SystemClock.uptimeMillis()
            if (nowMs - lastTickMs >= 100) {
                lastTickMs = nowMs
                refreshText()
            }
            Choreographer.getInstance().postFrameCallback(this)
        }
    }

    init {
        setTextColor(0xFFFFFFFF.toInt())
        setBackgroundColor(0x80000000.toInt())
        setPadding(16, 8, 16, 8)
        textSize = 12f
        typeface = android.graphics.Typeface.MONOSPACE
        text = "HUD: waiting for first frame…"
    }

    override fun onAttachedToWindow() {
        super.onAttachedToWindow()
        Choreographer.getInstance().postFrameCallback(frameCallback)
    }

    override fun onDetachedFromWindow() {
        Choreographer.getInstance().removeFrameCallback(frameCallback)
        super.onDetachedFromWindow()
    }

    /**
     * Called from the network thread on every video frame.
     *
     * @param ptsNs              PC monotonic_ns when the frame was stamped
     * @param captureUs          server-reported capture time (0 when unknown)
     * @param encodeUs           server-reported NVENC time
     * @param recvNs             android nanoTime() when MSG_VIDEO_FRAME parsed
     * @param decodedNs          android nanoTime() when MediaCodec produced output
     * @param displayedNs        decodedNs + 1 vsync (estimated)
     * @param pcMinusAndroidNs   clock offset from TimeSync; used to translate ptsNs
     * @param syncReady          true once at least one TimeSync sample has been observed
     */
    fun recordFrameSample(
        ptsNs: Long,
        captureUs: Int?,
        encodeUs: Int?,
        recvNs: Long,
        decodedNs: Long,
        displayedNs: Long,
        pcMinusAndroidNs: Long,
        syncReady: Boolean,
    ) {
        timeSyncReady = syncReady

        // Translate the PC timestamp to Android's clock basis.
        val ptsInAndroidNs = ptsNs - pcMinusAndroidNs
        val trueE2eUs = if (syncReady) (displayedNs - ptsInAndroidNs) / 1000 else -1L
        val netUs = if (syncReady) (recvNs - ptsInAndroidNs) / 1000 else -1L

        synchronized(ringLock) {
            ringTrueE2eUs[ringHead] = trueE2eUs
            ringRecvE2eUs[ringHead] = (displayedNs - recvNs) / 1000
            ringNetUs[ringHead] = netUs
            ringDecodeUs[ringHead] = (decodedNs - recvNs) / 1000
            ringDisplayUs[ringHead] = (displayedNs - decodedNs) / 1000
            ringEncodeUs[ringHead] = encodeUs ?: 0
            ringHead = (ringHead + 1) % ringSize
        }
    }

    fun recordServerTelemetry(t: Protocol.Telemetry) {
        serverTelemetry = t
    }

    /**
     * Record the latest TimeSync window state. Cheap; safe to call on every
     * frame. Surfaces three values needed to diagnose the long-session drift
     * bug: window sample count (should saturate around `windowMs / 1 Hz` =
     * 60), the current window-min RTT (should stay roughly stable, not drift
     * monotonically), and the age of the oldest in-window sample (should
     * also saturate near `windowMs`). If `windowSamples` stays low or
     * `bestRttNs` drifts upward over many minutes, the estimator is sick.
     */
    fun recordTimeSyncState(
        windowSamples: Int,
        bestRttNs: Long,
        oldestSampleAgeNs: Long,
    ) {
        timeSyncState = TimeSyncState(windowSamples, bestRttNs, oldestSampleAgeNs)
    }

    private data class Snapshot(
        val avgTrueE2eUs: Long,
        val p99TrueE2eUs: Long,
        val avgRecvE2eUs: Long,
        val avgNetUs: Long,
        val avgDecodeUs: Long,
        val avgDisplayUs: Long,
        val avgEncodeUs: Long,
    )

    private fun snapshot(): Snapshot = synchronized(ringLock) {
        val tE = ringTrueE2eUs.copyOf()
        val rE = ringRecvE2eUs.copyOf()
        val nE = ringNetUs.copyOf()
        val dE = ringDecodeUs.copyOf()
        val sE = ringDisplayUs.copyOf()
        val eE = LongArray(ringSize) { ringEncodeUs[it].toLong() }
        Snapshot(avg(tE), p99(tE), avg(rE), avg(nE), avg(dE), avg(sE), avg(eE))
    }

    private fun refreshText() {
        val s = snapshot()
        val st = serverTelemetry
        text = buildString {
            if (timeSyncReady) {
                append("e2e ").append(formatUs(s.avgTrueE2eUs))
                append("  p99 ").append(formatUs(s.p99TrueE2eUs)).append('\n')
                append("enc ").append(formatUs(s.avgEncodeUs))
                append("  net ").append(formatUs(s.avgNetUs))
                append("  dec ").append(formatUs(s.avgDecodeUs))
                append("  dsp ").append(formatUs(s.avgDisplayUs)).append('\n')
                append("recv→disp ").append(formatUs(s.avgRecvE2eUs))
            } else {
                append("e2e —  (waiting for time sync)\n")
                append("enc ").append(formatUs(s.avgEncodeUs))
                append("  dec ").append(formatUs(s.avgDecodeUs))
                append("  dsp ").append(formatUs(s.avgDisplayUs)).append('\n')
                append("recv→disp ").append(formatUs(s.avgRecvE2eUs))
            }
            if (st != null) {
                append('\n')
                append("srv frames=").append(st.frames)
                append(" drop=").append(st.dropped)
                append(" enc_p99=").append(formatUs(st.encodeUsP99.toLong()))
                append(" qd=").append(st.queueDepth)
            }
            val ts = timeSyncState
            if (ts != null) {
                append('\n')
                append("ts win=").append(ts.windowSamples)
                append(" rtt=").append(formatUs(ts.bestRttNs / 1000))
                append(" age=").append(ts.oldestSampleAgeNs / 1_000_000_000L).append('s')
            }
        }
    }

    private fun formatUs(us: Long): String =
        if (us <= 0) "—" else "%.1fms".format(us / 1000.0)

    private fun avg(arr: LongArray): Long {
        var sum = 0L
        var n = 0
        for (v in arr) if (v > 0) { sum += v; n++ }
        return if (n == 0) 0 else sum / n
    }

    private fun p99(arr: LongArray): Long {
        val sorted = arr.filter { it > 0 }.sorted()
        if (sorted.isEmpty()) return 0
        val idx = ((sorted.size - 1) * 0.99).toInt()
        return sorted[idx]
    }
}
