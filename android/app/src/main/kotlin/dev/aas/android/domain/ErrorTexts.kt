package dev.aas.android.domain

import dev.aas.android.R
import dev.aas.android.protocol.ErrorKind
import dev.aas.android.protocol.RpcError
import dev.aas.android.ui.common.UiText

/**
 * Errors as the app says them (protocol.md §1.3 `adapterError`, §3.1 `Turn.error.kind`): a
 * Japanese lead-in chosen by the error's kind, then the harness's own text verbatim. The daemon
 * sends the harness's text without its English prefix and without terminal escape sequences
 * (`data.detail`, `Turn.error.message`), so nothing is stripped or interpreted here.
 */
object ErrorTexts {
    /**
     * A definitive error answer of a request, for a message in place (a list that failed to load,
     * a command): the harness's words after a lead-in for `adapterError`, the refused command for
     * `sessionSwitchingCommand`, the server's message otherwise.
     */
    fun server(error: RpcError): UiText = when (error.kind) {
        ErrorKind.AdapterError -> UiText.of(R.string.error_harness_reported, error.detail ?: error.message)
        ErrorKind.SessionSwitchingCommand -> switching(error)
        ErrorKind.HarnessUnavailable -> UiText.of(R.string.error_harness_unavailable, error.reason ?: error.message)
        else -> UiText.of(R.string.error_server, error.message)
    }

    /**
     * A queued request the daemon refused definitively ("<what> に失敗しました: …"); a typed
     * session-switching command says what to use instead.
     */
    fun requestFailed(what: UiText, error: RpcError): UiText = when (error.kind) {
        ErrorKind.SessionSwitchingCommand -> switching(error)
        ErrorKind.AdapterError -> UiText.of(R.string.request_failed_harness, what, error.detail ?: error.message)
        else -> UiText.of(R.string.request_failed, what, error.message)
    }

    /** The refused `/command`: one thread is one native session, so `/new` and `/resume` are the ways instead. */
    private fun switching(error: RpcError): UiText = UiText.of(R.string.request_refused_switching, error.command ?: error.message)

    /** How a failed or stopped turn ended: the lead-in for its kind and the message (two lines). */
    fun turnError(kind: String, message: String): UiText = UiText.of(R.string.turn_error_format, leadIn(kind), message)

    /** The same on one line (notifications, lists). */
    fun turnErrorLine(kind: String, message: String): UiText = UiText.of(R.string.turn_error_line, leadIn(kind), message)

    /** The lead-in of a `Turn.error.kind` (a kind this app does not know names itself). */
    fun leadIn(kind: String): UiText = LEAD_INS[kind]?.let { UiText.of(it) }
        ?: if (kind.startsWith(CODEX_PREFIX)) UiText.of(R.string.turn_error_codex, kind.removePrefix(CODEX_PREFIX)) else UiText.of(R.string.turn_error_other, kind)

    /** The kind of a turn whose resume of the native session failed (protocol.md §3.1). */
    const val RESUME_FAILED = "resumeFailed"

    private const val CODEX_PREFIX = "codex:"

    private val LEAD_INS: Map<String, Int> = mapOf(
        "agentExited" to R.string.turn_error_agent_exited,
        "adapterError" to R.string.turn_error_adapter,
        "spawnFailed" to R.string.turn_error_spawn,
        RESUME_FAILED to R.string.turn_error_resume,
        "harnessUnavailable" to R.string.turn_error_unavailable,
        "forced" to R.string.turn_error_forced,
        "interrupted" to R.string.turn_error_interrupted,
        "stopped" to R.string.turn_error_stopped,
        "daemonShutdown" to R.string.turn_error_daemon_shutdown,
        "systemShutdown" to R.string.turn_error_system_shutdown,
        "daemonRestarted" to R.string.turn_error_daemon_restarted,
        "forkOutdated" to R.string.turn_error_fork_outdated,
        "harnessError" to R.string.turn_error_harness,
        "refusal" to R.string.turn_error_refusal,
    )
}
