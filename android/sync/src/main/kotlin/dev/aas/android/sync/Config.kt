package dev.aas.android.sync

/**
 * Policy values of the client. Timeouts, intervals and limits live here (never as literals in
 * the code), each with the reason for its default. Values the server announces in
 * `initialize` (`clientTimeoutMs`, `maxClientFrameBytes`) replace the local ones where noted.
 */
data class SyncConfig(
    /**
     * Upper bound for TCP connect + TLS + WebSocket upgrade. 20 s tolerates a cold Tailscale
     * DERP relay on a slow mobile network; a dead path fails well before the user gives up.
     */
    val connectTimeoutMs: Long = 20_000,
    /**
     * Watchdog timeout from the upgrade until `initialize` answers with the server's
     * `clientTimeoutMs` (protocol.md §2.2). Equal to the server's default `client_timeout`
     * (45 s, design.md §13), the value the server most likely announces.
     */
    val initialClientTimeoutMs: Long = 45_000,
    /**
     * Upper bound for one request/response pair on a connection that otherwise stays alive.
     * The server answers every request; this only guards against a lost response. It is above
     * the server's `tool_timeout` (120 s, e.g. `git worktree add` inside `thread/create`) so
     * slow but healthy calls finish. A timed-out mutation stays in the outbox and is resent
     * with the same `clientRequestId`, so the server still runs it at most once.
     */
    val callTimeoutMs: Long = 180_000,
    /** First reconnect delay window (full jitter: uniform in 0..500 ms), doubling per failure. */
    val backoffBaseMs: Long = 500,
    /**
     * Upper bound of the reconnect delay (design.md §15: full jitter, capped at 30 s). Also the
     * fixed delay after a protocol violation (close code 4003), which reconnecting cannot fix.
     */
    val backoffCapMs: Long = 30_000,
    /**
     * First delay before resending an outbox entry that failed with a non-definitive error
     * (`draining`, `rateLimited`, `internal`, no answer, …) on a live connection. Such causes
     * last seconds to minutes; the delay doubles per failure up to [outboxRetryCapMs].
     * `harnessUnavailable` is not resent on this timer while the harness is known to be
     * unavailable: the request waits for `harness/updated` ([OutboxEntry.waitingForHarness]).
     */
    val outboxRetryBaseMs: Long = 2_000,
    /** Upper bound of the outbox retry delay: a cleared cause is noticed within a minute. */
    val outboxRetryCapMs: Long = 60_000,
    /** Turns per `thread/read` page: a screenful of recent history with room to scroll. */
    val threadPageTurns: Int = 20,
    /**
     * How long a dropped socket may take to report its end (OkHttp's `onFailure` / `onClosed`)
     * before the engine opens the next one. Dropping closes the TCP socket at once and the
     * report follows within milliseconds; waiting for it makes "one socket at a time" hold by
     * construction rather than by OkHttp's timing. The bound only guards against a report that
     * never comes: the engine then logs an error and goes on (the socket is closed anyway).
     */
    val socketReleaseTimeoutMs: Long = 5_000,
) {
    init {
        require(connectTimeoutMs > 0) { "connectTimeoutMs must be positive" }
        require(socketReleaseTimeoutMs > 0) { "socketReleaseTimeoutMs must be positive" }
        require(initialClientTimeoutMs > 0) { "initialClientTimeoutMs must be positive" }
        require(callTimeoutMs > 0) { "callTimeoutMs must be positive" }
        require(backoffBaseMs > 0 && backoffCapMs >= backoffBaseMs) { "0 < backoffBaseMs <= backoffCapMs" }
        require(outboxRetryBaseMs > 0 && outboxRetryCapMs >= outboxRetryBaseMs) { "0 < outboxRetryBaseMs <= outboxRetryCapMs" }
        require(threadPageTurns in 1..200) { "threadPageTurns must be within the server's 1..200" }
    }

    /** Delay before resending an outbox entry after its [failures]-th non-definitive error. */
    fun outboxRetryDelayMs(failures: Int): Long {
        val shift = (failures - 1).coerceIn(0, MAX_SHIFT)
        val delay = outboxRetryBaseMs shl shift
        return if (delay <= 0 || delay > outboxRetryCapMs) outboxRetryCapMs else delay
    }

    private companion object {
        /** Doubling stops here (2^30 × base is far beyond any cap); larger shifts would overflow. */
        const val MAX_SHIFT = 30
    }
}

/** Time sources. Wall-clock time is shown to users; monotonic time measures durations. */
interface Clock {
    /** Unix epoch milliseconds. */
    fun nowMs(): Long

    /** Milliseconds of a monotonic clock (only differences are meaningful). */
    fun monotonicMs(): Long

    companion object System : Clock {
        override fun nowMs(): Long = java.lang.System.currentTimeMillis()

        override fun monotonicMs(): Long = java.lang.System.nanoTime() / 1_000_000
    }
}

/** Diagnostic log sink (the app forwards it to logcat). */
fun interface SyncLogger {
    enum class Level { Debug, Info, Warn, Error }

    fun log(level: Level, message: String, error: Throwable?)

    companion object {
        val None: SyncLogger = SyncLogger { _, _, _ -> }
    }
}

/** Observes every text frame on the wire (debug screens, tests). Called on I/O threads. */
interface WireTap {
    fun sent(text: String) {}

    fun received(text: String) {}
}
