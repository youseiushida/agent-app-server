package dev.aas.android.service

import android.content.res.Resources
import android.text.format.DateUtils
import dev.aas.android.R
import dev.aas.android.protocol.ShutdownReason

/** Title and detail line of a [ConnectionPresentation]. */
data class ConnectionText(val title: String, val detail: String?)

/**
 * The words for the connection state, shared by the status bar and the persistent notification
 * so they never disagree.
 */
object ConnectionTexts {
    fun title(res: Resources, summary: ConnectionSummary): String = when (summary) {
        is ConnectionSummary.Connected -> res.getString(R.string.connection_connected)
        is ConnectionSummary.Connecting ->
            if (summary.reconnecting) res.getString(R.string.connection_reconnecting) else res.getString(R.string.connection_connecting)
        is ConnectionSummary.WaitingToRetry -> when (summary.cause) {
            RetryCause.Unreachable -> res.getString(R.string.connection_unreachable)
            RetryCause.ServerRestarting -> res.getString(R.string.connection_server_restarting)
            RetryCause.ServerStopped -> res.getString(R.string.connection_server_stopped)
            RetryCause.Timeout, RetryCause.NetworkChanged, RetryCause.Other -> res.getString(R.string.connection_reconnecting)
            RetryCause.ProtocolError -> res.getString(R.string.connection_protocol_error)
        }
        ConnectionSummary.PhoneOffline -> res.getString(R.string.connection_phone_offline)
        ConnectionSummary.ConnectedElsewhere -> res.getString(R.string.connection_elsewhere)
        is ConnectionSummary.NeedsPairing -> when (summary.reason) {
            PairingNeed.NotPaired -> res.getString(R.string.connection_not_paired)
            PairingNeed.Revoked -> res.getString(R.string.connection_revoked)
            PairingNeed.TokenRejected -> res.getString(R.string.connection_token_rejected)
            PairingNeed.InvalidServerUrl -> res.getString(R.string.connection_invalid_url)
        }
        is ConnectionSummary.Incompatible -> res.getString(R.string.connection_incompatible)
        is ConnectionSummary.KeystoreUnavailable -> res.getString(R.string.connection_keystore_unavailable)
        ConnectionSummary.Stopped -> res.getString(R.string.connection_stopped)
    }

    /** "送信待ち 2 件 · 最終同期 3 分前" (the parts that apply), or `null`. */
    fun detail(res: Resources, presentation: ConnectionPresentation, nowMs: Long): String? {
        val parts = ArrayList<String>(3)
        val summary = presentation.summary
        if (summary is ConnectionSummary.WaitingToRetry && summary.attempt > 0) {
            parts += res.getString(R.string.connection_attempt, summary.attempt)
        }
        val shutdown = presentation.serverShutdown
        if (shutdown != null && summary !is ConnectionSummary.Connected) {
            parts += res.getString(
                when {
                    shutdown == ShutdownReason.StorageFailure -> R.string.connection_server_storage_failure
                    presentation.serverRestartExpected == false -> R.string.connection_server_stopped_detail
                    else -> R.string.connection_server_shutting_down
                },
            )
        }
        if (presentation.pendingOutbox > 0) parts += res.getString(R.string.connection_pending, presentation.pendingOutbox)
        val lastSync = presentation.lastSyncAtMs
        if (summary !is ConnectionSummary.Connected && lastSync != null) {
            parts += res.getString(R.string.connection_last_sync, relative(lastSync, nowMs))
        }
        return parts.takeIf { it.isNotEmpty() }?.joinToString(" · ")
    }

    fun describe(res: Resources, presentation: ConnectionPresentation, nowMs: Long): ConnectionText =
        ConnectionText(title(res, presentation.summary), detail(res, presentation, nowMs))

    /**
     * "3 分前" in the device's locale. Everything said this way has happened, so a time after
     * [nowMs] is said as now: screens pass the clock of [dev.aas.android.ui.components.rememberNow],
     * which ticks only every `relativeTimeRefreshMs` (a sync right after a tick is ahead of it),
     * and the PC's clock may run ahead of the phone's. Unclamped, the connection strip said
     * "最終同期 0 分後" ("In 0 min.") while reconnecting (found on the emulator).
     */
    fun relative(atMs: Long, nowMs: Long): String =
        DateUtils.getRelativeTimeSpanString(minOf(atMs, nowMs), nowMs, DateUtils.MINUTE_IN_MILLIS, DateUtils.FORMAT_ABBREV_RELATIVE).toString()
}
