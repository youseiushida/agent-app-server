package dev.aas.android.diagnostics

import android.util.Log
import dev.aas.android.sync.Clock
import dev.aas.android.sync.SyncLogger
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.flow.asStateFlow
import kotlinx.coroutines.flow.update
import java.time.Instant
import java.util.concurrent.atomic.AtomicLong

/** One line of the connection log; [seq] numbers the lines of this process. */
data class LogEntry(val seq: Long, val atMs: Long, val level: SyncLogger.Level, val source: String, val message: String)

/**
 * The diagnostics log shown in 設定 → 診断: the sync engine's log, connection state changes,
 * network callbacks and service lifecycle. Kept in memory (the newest [capacity] lines) and
 * forwarded to logcat; it is not persisted, so it describes the current process only.
 */
class ConnectionLog(
    private val capacity: Int,
    private val clock: Clock = Clock.System,
    /** Forward to logcat (off in JVM tests, where android.util.Log is not available). */
    private val logcat: Boolean = true,
) {
    private val nextSeq = AtomicLong()
    private val _entries = MutableStateFlow<List<LogEntry>>(emptyList())
    val entries: StateFlow<List<LogEntry>> = _entries.asStateFlow()

    fun add(level: SyncLogger.Level, source: String, message: String, error: Throwable? = null) {
        val text = if (error == null) message else "$message (${error.javaClass.simpleName}: ${error.message})"
        val entry = LogEntry(nextSeq.incrementAndGet(), clock.nowMs(), level, source, text)
        _entries.update { current -> (current + entry).takeLast(capacity) }
        if (!logcat) return
        when (level) {
            SyncLogger.Level.Debug -> Log.d(TAG, "[$source] $text", error)
            SyncLogger.Level.Info -> Log.i(TAG, "[$source] $text", error)
            SyncLogger.Level.Warn -> Log.w(TAG, "[$source] $text", error)
            SyncLogger.Level.Error -> Log.e(TAG, "[$source] $text", error)
        }
    }

    fun info(source: String, message: String) = add(SyncLogger.Level.Info, source, message)

    fun warn(source: String, message: String, error: Throwable? = null) = add(SyncLogger.Level.Warn, source, message, error)

    /** The sink handed to the sync engine. */
    val syncLogger: SyncLogger = SyncLogger { level, message, error -> add(level, SOURCE_SYNC, message, error) }

    /** A sink that writes lines of [source] (e.g. [SOURCE_DATA] for the repositories). */
    fun logger(source: String): SyncLogger = SyncLogger { level, message, error -> add(level, source, message, error) }

    /** All lines as plain text (copied to the clipboard from the diagnostics screen). */
    fun asText(entries: List<LogEntry> = this.entries.value): String =
        entries.joinToString("\n") { "${Instant.ofEpochMilli(it.atMs)} ${it.level} [${it.source}] ${it.message}" }

    companion object {
        const val TAG = "aas"
        const val SOURCE_SYNC = "sync"
        const val SOURCE_SERVICE = "service"
        const val SOURCE_NETWORK = "network"
        const val SOURCE_NOTIFY = "notify"
        const val SOURCE_PAIRING = "pairing"

        /** Corrections of the daemon's data where it enters the app (data/ServerLists.kt). */
        const val SOURCE_DATA = "data"
    }
}
