package dev.aas.android.sync

import kotlin.random.Random

/**
 * Reconnect delays: exponential with full jitter, `uniform(0, min(cap, base * 2^attempt))`
 * (design.md §15). The randomness only spreads simultaneous reconnects of several clients; it
 * is a policy, not an inference about the server.
 */
class Backoff(
    private val baseMs: Long,
    val capMs: Long,
    private val random: Random = Random.Default,
) {
    init {
        require(baseMs > 0 && capMs >= baseMs) { "0 < baseMs <= capMs" }
    }

    /** Delay before reconnect attempt number [attempt] (0 = the first retry after a success). */
    fun delayMs(attempt: Int): Long = random.nextLong(0, maxDelayMs(attempt) + 1)

    /** Upper bound of [delayMs] for [attempt] (the jitter window). */
    fun maxDelayMs(attempt: Int): Long {
        val shift = attempt.coerceIn(0, MAX_SHIFT)
        val window = baseMs shl shift
        return if (window <= 0 || window > capMs) capMs else window
    }

    private companion object {
        /** Doubling stops here (2^30 × base is far beyond any cap); larger shifts would overflow. */
        const val MAX_SHIFT = 30
    }
}
