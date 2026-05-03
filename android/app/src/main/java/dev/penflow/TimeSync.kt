package dev.penflow

import android.util.Log

/**
 * NTP-style clock-offset estimator between PC and Android monotonic clocks.
 *
 * Each ping/pong sample yields one (rtt, offset) estimate. We keep the sample
 * with the smallest measured RTT — that's the one with the least transport
 * jitter and therefore the most accurate offset (assuming symmetric one-way
 * latency, which holds well over loopback ADB).
 *
 * Usage:
 *   - Caller sends MSG_TIME_SYNC_REQ with `nanoTime()` as t1.
 *   - On MSG_TIME_SYNC_RESP, caller records t4 = nanoTime() and calls observe(...).
 *   - To translate a PC pts_ns into Android time: pcToAndroid(ptsNs).
 *
 * pcMinusAndroidNs > 0 means the PC clock is "ahead" of Android (in absolute ns).
 */
class TimeSync {

    @Volatile var pcMinusAndroidNs: Long = 0L
        private set

    @Volatile var bestRttNs: Long = Long.MAX_VALUE
        private set

    @Volatile var sampleCount: Int = 0
        private set

    /**
     * @param t1 android nanoTime() at REQ send
     * @param t2 PC monotonic_ns at REQ recv (echoed in RESP)
     * @param t3 PC monotonic_ns at RESP send (echoed in RESP)
     * @param t4 android nanoTime() at RESP recv
     */
    @Synchronized
    fun observe(t1: Long, t2: Long, t3: Long, t4: Long) {
        // RTT excluding PC's processing time between recv and send.
        val rtt = (t4 - t1) - (t3 - t2)
        if (rtt < 0) {
            // Garbage; reject.
            Log.w(TAG, "time sync sample with negative rtt=$rtt — rejected")
            return
        }
        sampleCount += 1
        if (rtt < bestRttNs) {
            bestRttNs = rtt
            // At PC's t2: PC clock = t2, Android clock = t1 + (one-way to PC) ≈ t1 + rtt/2.
            // Offset = pcClock - androidClock = t2 - (t1 + rtt/2)
            pcMinusAndroidNs = t2 - (t1 + rtt / 2)
            Log.i(TAG, "time sync: rtt=${rtt / 1000}us  pc-android=${pcMinusAndroidNs / 1000}us  (sample $sampleCount)")
        }
    }

    /** Translate a PC monotonic_ns timestamp to Android nanoTime() basis. */
    fun pcToAndroid(pcNs: Long): Long = pcNs - pcMinusAndroidNs

    /** True if at least one valid sample has been seen. */
    fun isReady(): Boolean = sampleCount > 0

    companion object {
        private const val TAG = "TimeSync"
    }
}
