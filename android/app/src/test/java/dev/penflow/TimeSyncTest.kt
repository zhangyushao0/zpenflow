package dev.penflow

import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNotEquals
import org.junit.Assert.assertTrue
import org.junit.Test
import kotlin.math.abs

/**
 * Tests cover the failure modes that motivated the rewrite:
 *
 *   1. Drift compensation — the original "all-time min-RTT" pattern froze
 *      the offset within the first lucky sample and never updated, even
 *      as the two clocks drifted by tens of ms over a long session.
 *   2. Outlier resilience — a single fat-RTT spike must not perturb the
 *      offset.
 *   3. Reset on reconnect — between PC server restarts the offset is
 *      meaningless and must be wiped.
 *   4. Window aging — old samples must fall out so they cannot dominate
 *      the min-RTT calculation forever.
 *
 * Tests use synthetic timestamps rather than wall-clock time so the
 * sliding-window math can be exercised deterministically.
 */
class TimeSyncTest {

    /** Construct a TimeSync with the no-op logger (Android `Log` would
     *  throw `RuntimeException("Method not mocked")` in local JUnit). */
    private fun newSync(windowMs: Long = TimeSync.DEFAULT_WINDOW_MS) =
        TimeSync(windowMs = windowMs, logger = TimeSync.NoopLogger)

    /**
     * Drive one ping/pong with synthetic timestamps.
     *
     * @param androidNowNs the android-clock time at which the REQ is sent.
     * @param pcOffsetNs   what (PC clock − Android clock) actually is at
     *                     this moment. The test harness uses this to
     *                     simulate clock drift.
     * @param oneWayNs     real one-way wire latency on each leg (assumed
     *                     symmetric — that's the assumption TimeSync's
     *                     `rtt/2` formula encodes).
     * @param pcDwellNs    time the PC spends between recv and send. The
     *                     RTT formula subtracts this off so it does not
     *                     pollute the offset.
     */
    private fun ping(
        sync: TimeSync,
        androidNowNs: Long,
        pcOffsetNs: Long,
        oneWayNs: Long = 500_000L,
        pcDwellNs: Long = 50_000L,
    ) {
        val t1 = androidNowNs
        val t2 = (t1 + oneWayNs) + pcOffsetNs        // PC clock at REQ recv
        val t3 = t2 + pcDwellNs                       // PC clock at RESP send
        val t4 = t1 + oneWayNs + pcDwellNs + oneWayNs // Android clock at RESP recv
        sync.observe(t1, t2, t3, t4)
    }

    @Test
    fun negativeRttRejected() {
        val sync = newSync()
        // Hand-craft a sample where PC dwell exceeds android RTT — gives
        // negative computed RTT. Estimator must drop it without bumping
        // window state.
        sync.observe(t1 = 1000, t2 = 2000, t3 = 5000, t4 = 1500)
        assertFalse(sync.isReady())
        assertEquals(0, sync.windowSampleCount)
        assertEquals(0, sync.totalSampleCount)
        assertEquals(0L, sync.pcMinusAndroidNs)
    }

    @Test
    fun firstSampleConverges() {
        val sync = newSync()
        ping(sync, androidNowNs = 1_000_000_000L, pcOffsetNs = 1_000_000_000_000L)
        assertTrue(sync.isReady())
        assertEquals(1, sync.windowSampleCount)
        assertEquals(1, sync.totalSampleCount)
        // Offset should be near pcOffsetNs, within rtt/2 ≈ 500us.
        assertTrue(abs(sync.pcMinusAndroidNs - 1_000_000_000_000L) < 1_000_000L)
    }

    /**
     * The headline regression: simulate 10 ppm clock drift over 30
     * minutes of 1 Hz ping/pong and verify the offset tracks it. The
     * pre-rewrite estimator would freeze on its first lucky sample and
     * the metric error would grow linearly to ~18 ms; the rewrite must
     * keep error bounded by the within-window drift budget.
     */
    @Test
    fun tracksClockDriftOverLongSession() {
        val sync = newSync(windowMs = 60_000L)

        val driftPpm = 10.0
        val sessionSeconds = 30 * 60 // 30 minutes
        val initialOffsetNs = 5_000_000_000L

        var maxAbsError = 0L
        for (sec in 0 until sessionSeconds) {
            val androidNowNs = sec.toLong() * 1_000_000_000L
            // PC offset grows linearly: pc clock runs `driftPpm` faster.
            val pcOffsetNs = initialOffsetNs +
                (androidNowNs * driftPpm / 1_000_000.0).toLong()
            ping(sync, androidNowNs, pcOffsetNs)

            val error = abs(sync.pcMinusAndroidNs - pcOffsetNs)
            if (error > maxAbsError) maxAbsError = error
        }

        // Within-window drift budget: windowMs * driftPpm.
        // 60 s * 10 ppm = 600 us. Add 500 us slack for the rtt/2 estimate.
        val budgetNs = 60_000L * driftPpm.toLong() * 1000 + 500_000L
        assertTrue(
            "drift error $maxAbsError ns exceeds budget $budgetNs ns",
            maxAbsError <= budgetNs,
        )

        // Sanity: the pre-rewrite ratchet would have been ~18 ms here.
        // We must be ≥10× better.
        assertTrue(
            "drift error $maxAbsError ns is regression-territory (>1.8 ms)",
            maxAbsError < 1_800_000L,
        )
    }

    /**
     * A single huge-RTT outlier (e.g. a TIME_SYNC_RESP queued behind a
     * 50 KB I-frame on the bulk-IN endpoint) must not perturb the
     * offset. Min-RTT-within-window gives us this for free, but pin
     * it down with a test so future "improvements" don't break it.
     */
    @Test
    fun outlierDoesNotPerturbOffset() {
        val sync = newSync()

        // Lay down 10 clean samples first.
        for (sec in 0 until 10) {
            ping(sync, androidNowNs = sec.toLong() * 1_000_000_000L,
                 pcOffsetNs = 1_000_000_000L, oneWayNs = 500_000L)
        }
        val cleanOffset = sync.pcMinusAndroidNs

        // One sample with 50 ms one-way (= 100 ms RTT).
        ping(sync, androidNowNs = 10_000_000_000L,
             pcOffsetNs = 1_000_000_000L, oneWayNs = 50_000_000L)

        // Offset is unchanged: the outlier did not win window-min RTT.
        assertEquals(cleanOffset, sync.pcMinusAndroidNs)

        // It WAS counted, though.
        assertEquals(11, sync.totalSampleCount)
        assertEquals(11, sync.windowSampleCount)
    }

    /**
     * Old samples falling out of the window let newer samples become the
     * window min. Without this, the rewrite degenerates to the same
     * "all-time min freezes" bug.
     */
    @Test
    fun oldSamplesAgeOut() {
        val sync = newSync(windowMs = 5_000L) // small window for the test

        // T=0: a "lucky" sample with very low RTT (200 us one-way).
        ping(sync, androidNowNs = 0L, pcOffsetNs = 100_000_000L,
             oneWayNs = 200_000L)
        val lockedRtt = sync.bestRttNs

        // T=1..4 s: normal samples (500 us one-way → 1 ms RTT). The
        // lucky sample is still the window min.
        for (sec in 1..4) {
            ping(sync, androidNowNs = sec.toLong() * 1_000_000_000L,
                 pcOffsetNs = 100_000_000L, oneWayNs = 500_000L)
        }
        assertEquals(lockedRtt, sync.bestRttNs)
        assertEquals(5, sync.windowSampleCount)

        // T=6 s: lucky sample is now > 5 s old → aged out. Window min
        // becomes one of the 1 ms-RTT samples.
        ping(sync, androidNowNs = 6_000_000_000L, pcOffsetNs = 100_000_000L,
             oneWayNs = 500_000L)
        assertNotEquals(
            "lucky sample should have aged out, but bestRttNs is still $lockedRtt",
            lockedRtt,
            sync.bestRttNs,
        )
        // bestRttNs ≈ 1 ms now (2 * 500 us one-way), should be > lockedRtt.
        assertTrue(sync.bestRttNs > lockedRtt)
        // Window contains the four normal samples from sec 1..4 plus the
        // sec-6 sample (sec-1 is exactly at the cutoff age 5 s and may or
        // may not survive depending on the >/>= boundary; we accept either).
        assertTrue(sync.windowSampleCount in 4..5)
    }

    @Test
    fun resetClearsAllState() {
        val sync = newSync()
        for (sec in 0 until 10) {
            ping(sync, androidNowNs = sec.toLong() * 1_000_000_000L,
                 pcOffsetNs = 1_000_000_000L)
        }
        assertTrue(sync.isReady())
        assertNotEquals(0L, sync.pcMinusAndroidNs)
        assertNotEquals(Long.MAX_VALUE, sync.bestRttNs)

        sync.reset()

        assertFalse(sync.isReady())
        assertEquals(0L, sync.pcMinusAndroidNs)
        assertEquals(Long.MAX_VALUE, sync.bestRttNs)
        assertEquals(0, sync.windowSampleCount)
        assertEquals(0, sync.totalSampleCount)
        assertEquals(0L, sync.oldestSampleAgeNs(1_000_000_000_000L))
    }

    @Test
    fun pcToAndroidTranslation() {
        val sync = newSync()
        // Offset = 5 s exactly, zero one-way for clean math.
        ping(sync, androidNowNs = 1_000_000_000L,
             pcOffsetNs = 5_000_000_000L, oneWayNs = 0L, pcDwellNs = 0L)
        // PC clock value 12 s → Android clock value 12 s − 5 s = 7 s.
        assertEquals(7_000_000_000L, sync.pcToAndroid(12_000_000_000L))
    }

    @Test
    fun oldestSampleAgeReportsCorrectly() {
        val sync = newSync(windowMs = 60_000L)
        // Place samples at t = 0, 1 s, 2 s.
        ping(sync, androidNowNs = 0L, pcOffsetNs = 0L)
        ping(sync, androidNowNs = 1_000_000_000L, pcOffsetNs = 0L)
        ping(sync, androidNowNs = 2_000_000_000L, pcOffsetNs = 0L)
        // "Now" is 5 s; oldest in-window sample is at t = 0, so age = 5 s
        // (allowing rtt/2 jitter: the recvNs we store is t4 ≈ t1 + RTT,
        // so add a small slack).
        val ageNs = sync.oldestSampleAgeNs(5_000_000_000L)
        assertTrue(
            "expected ~5s age, got ${ageNs / 1_000_000} ms",
            abs(ageNs - 5_000_000_000L) < 5_000_000L,
        )
    }
}
