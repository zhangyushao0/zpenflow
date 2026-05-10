package dev.penflow

import android.util.Log

/**
 * NTP-style clock-offset estimator with sliding-window min-RTT selection.
 *
 * Two independent monotonic clocks (Windows QPC and Android `System.nanoTime()`)
 * drift relative to each other at ~5–50 ppm; over a 30 min session that is
 * 9–90 ms of accumulated error. The previous "all-time min-RTT" lock-in
 * pattern froze the offset within the first minute of a session and never
 * updated it, which manifested in the HUD as a `net` metric that ratcheted
 * upward at exactly the differential clock-drift rate (visible during long
 * Krita sessions: e2e starting at ~20 ms creeping to ~30 ms after 30 min,
 * with all of the growth concentrated in `net` because that is the only
 * cross-clock subtraction in the metrics).
 *
 * This implementation keeps a sliding window of recent samples and picks
 * the min-RTT sample within the window. As old "lucky" samples age out
 * past `windowMs`, newer samples become the window minimum, so the offset
 * tracks clock drift instead of getting stuck. Within-window drift is
 * bounded to roughly `windowMs * ppm` ≈ 0.6 ms at 60 s and 10 ppm — about
 * 30× better than the previous unbounded ratchet.
 *
 * Outlier rejection is implicit: a sudden RTT spike (e.g. a large I-frame
 * sharing the bulk-IN endpoint with a TIME_SYNC_RESP) cannot win min-RTT
 * within the window, so it never pollutes the offset.
 *
 * Usage:
 *   - Caller sends `MSG_TIME_SYNC_REQ` with `nanoTime()` as `t1`.
 *   - On `MSG_TIME_SYNC_RESP`, caller records `t4 = nanoTime()` and calls
 *     [observe].
 *   - To translate a PC `pts_ns` into Android time: [pcToAndroid].
 *
 * `pcMinusAndroidNs > 0` means the PC clock is "ahead" of Android (in
 * absolute ns).
 *
 * @param windowMs sliding-window size. 60 s keeps 60 samples at 1 Hz —
 *   enough that a single load spike has a <2 % chance of being the
 *   window min, while still adapting to drift quickly enough that
 *   metrics stay accurate to <1 ms over a multi-hour session.
 * @param logger log sink; defaults to Android `Log`. Tests inject [NoopLogger]
 *   because `android.util.Log` is unmocked in local JUnit tests and
 *   throws `RuntimeException("Method not mocked")`.
 */
class TimeSync(
    private val windowMs: Long = DEFAULT_WINDOW_MS,
    private val logger: Logger = AndroidLogger,
) {

    /** Pluggable log sink; see [AndroidLogger] / [NoopLogger]. */
    interface Logger {
        fun info(tag: String, msg: String)
        fun warn(tag: String, msg: String)
    }

    private object AndroidLogger : Logger {
        override fun info(tag: String, msg: String) { Log.i(tag, msg) }
        override fun warn(tag: String, msg: String) { Log.w(tag, msg) }
    }

    /** No-op logger for tests and for callers that want silence. */
    object NoopLogger : Logger {
        override fun info(tag: String, msg: String) {}
        override fun warn(tag: String, msg: String) {}
    }

    private data class Sample(
        val rttNs: Long,
        val offsetNs: Long,
        /** Android `nanoTime()` at which this sample's `t4` was observed. */
        val recvNs: Long,
    )

    /** Newest at tail. Bounded by aging in [observe]. */
    private val samples = ArrayDeque<Sample>(64)

    @Volatile var pcMinusAndroidNs: Long = 0L
        private set

    /** Smallest RTT observed within the active sliding window. `MAX_VALUE`
     *  before any sample lands. */
    @Volatile var bestRttNs: Long = Long.MAX_VALUE
        private set

    /** Total samples observed across the lifetime of this estimator,
     *  including rejected ones. Diagnostic only. */
    @Volatile var totalSampleCount: Int = 0
        private set

    /** Number of valid samples currently held in the sliding window. */
    @Volatile var windowSampleCount: Int = 0
        private set

    /** Android `nanoTime()` at which the oldest in-window sample was
     *  received, or 0 when the window is empty. Snapshot for diagnostics. */
    @Volatile private var oldestSampleRecvNs: Long = 0L

    /**
     * @param t1 android `nanoTime()` at REQ send
     * @param t2 PC monotonic_ns at REQ recv (echoed in RESP)
     * @param t3 PC monotonic_ns at RESP send (echoed in RESP)
     * @param t4 android `nanoTime()` at RESP recv
     */
    @Synchronized
    fun observe(t1: Long, t2: Long, t3: Long, t4: Long) {
        // RTT excluding the PC's recv→send processing time.
        val rtt = (t4 - t1) - (t3 - t2)
        if (rtt < 0) {
            // Garbage; reject without bumping any counter beyond the
            // total. Negative RTT means t1/t4 came from different
            // PenflowClient instances or one of the clocks ran backward,
            // both of which are caller bugs, not real samples.
            logger.warn(TAG, "time sync sample rejected: rtt=$rtt ns is negative")
            return
        }
        totalSampleCount += 1

        val offset = t2 - (t1 + rtt / 2)
        samples.addLast(Sample(rtt, offset, t4))

        // Age out samples older than the window. Cutoff is computed
        // from t4 (the freshest sample's recv time) rather than a free
        // wall-clock read so the test harness can drive the estimator
        // with synthetic timestamps.
        val cutoffNs = t4 - windowMs * 1_000_000L
        while (samples.isNotEmpty() && samples.first().recvNs < cutoffNs) {
            samples.removeFirst()
        }

        // Pick the window's min-RTT sample. With samples non-empty
        // (we just appended) this is guaranteed to exist.
        var best = samples.first()
        for (s in samples) {
            if (s.rttNs < best.rttNs) best = s
        }

        val previousOffset = pcMinusAndroidNs
        val previousBestRtt = bestRttNs
        pcMinusAndroidNs = best.offsetNs
        bestRttNs = best.rttNs
        windowSampleCount = samples.size
        oldestSampleRecvNs = samples.first().recvNs

        // Be quiet on routine sample arrivals. Log only the events
        // that matter for verifying the estimator is healthy:
        //   - first sample (initial lock)
        //   - the offset stepped by > 1 ms (new window-min sample took
        //     over; expected when drift accumulates or after reset)
        //   - bestRttNs improved by > 100 µs (network conditions
        //     genuinely got better, or burst convergence)
        val offsetDelta = best.offsetNs - previousOffset
        val rttImproved = best.rttNs < previousBestRtt - 100_000L
        val firstLock = totalSampleCount == 1
        val largeOffsetJump = kotlin.math.abs(offsetDelta) > 1_000_000L
        if (firstLock || largeOffsetJump || rttImproved) {
            logger.info(
                TAG,
                "time sync: rtt_min=${bestRttNs / 1000}us " +
                    "pc-android=${pcMinusAndroidNs / 1000}us " +
                    "Δoffset=${offsetDelta / 1000}us " +
                    "win=${samples.size} total=$totalSampleCount",
            )
        }
    }

    /** Translate a PC `monotonic_ns` timestamp to Android `nanoTime()` basis. */
    fun pcToAndroid(pcNs: Long): Long = pcNs - pcMinusAndroidNs

    /** True if at least one valid sample is held in the window. Reads of
     *  `pcMinusAndroidNs` only have meaning once this flips true. */
    fun isReady(): Boolean = windowSampleCount > 0

    /**
     * Age in nanoseconds of the oldest in-window sample relative to
     * `nowNs` (caller-supplied so tests can drive synthetic time).
     * Returns 0 when the window is empty.
     */
    fun oldestSampleAgeNs(nowNs: Long): Long {
        val recv = oldestSampleRecvNs
        return if (recv == 0L) 0L else nowNs - recv
    }

    /**
     * Drop all state. Call this at the start of every fresh PC
     * connection: the previous PC server may have restarted and rebased
     * its monotonic clock, in which case carrying over old samples
     * would yield a wildly wrong offset until they aged out (up to
     * `windowMs`). Cheap; safe to call defensively.
     */
    @Synchronized
    fun reset() {
        samples.clear()
        pcMinusAndroidNs = 0L
        bestRttNs = Long.MAX_VALUE
        totalSampleCount = 0
        windowSampleCount = 0
        oldestSampleRecvNs = 0L
    }

    companion object {
        private const val TAG = "TimeSync"

        /** Default sliding-window span. Sized to outlast the burst-5
         *  startup convergence and a comfortable margin of normal
         *  drift, while staying well under typical session length. */
        const val DEFAULT_WINDOW_MS: Long = 60_000L
    }
}
