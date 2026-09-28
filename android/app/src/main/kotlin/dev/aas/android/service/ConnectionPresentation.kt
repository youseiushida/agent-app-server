package dev.aas.android.service

import dev.aas.android.protocol.ShutdownReason
import dev.aas.android.security.PairingState
import dev.aas.android.sync.ConnectionState
import dev.aas.android.sync.DisconnectCause
import dev.aas.android.sync.SuspendReason
import dev.aas.android.sync.SyncStatus

/**
 * The connection state in the words the UI uses (status bar, persistent notification, banners).
 * It distinguishes "this phone is offline" from "the PC cannot be reached" (UX §8.1) and the
 * states that need the user (connected elsewhere, pairing again).
 */
sealed interface ConnectionSummary {
    /** Subscribed and live. */
    data class Connected(val sinceMs: Long) : ConnectionSummary

    /** Opening the connection and syncing. [reconnecting] once a session existed before. */
    data class Connecting(val attempt: Int, val reconnecting: Boolean) : ConnectionSummary

    /** The PC could not be reached (or the connection dropped); retrying at [retryAtMs]. */
    data class WaitingToRetry(val attempt: Int, val retryAtMs: Long, val cause: RetryCause) : ConnectionSummary

    /** This phone has no usable network. */
    data object PhoneOffline : ConnectionSummary

    /** Close code 4000: this device's token connected from somewhere else (UX: 別の場所で接続中). */
    data object ConnectedElsewhere : ConnectionSummary

    /** The device must pair (again). */
    data class NeedsPairing(val reason: PairingNeed) : ConnectionSummary

    /** The server speaks another protocol version. */
    data class Incompatible(val message: String) : ConnectionSummary

    /**
     * A pairing is stored, but the keystore could not decrypt its token just now; the app tries
     * again at [retryAtMs] (and when asked). Nothing is wrong with the pairing.
     */
    data class KeystoreUnavailable(val retryAtMs: Long) : ConnectionSummary

    /** The engine does not run (the connection service is stopped). */
    data object Stopped : ConnectionSummary
}

/** Why the device has to pair. */
enum class PairingNeed {
    /** Never paired (or unpaired). */
    NotPaired,

    /** Close code 4001: this device was revoked. */
    Revoked,

    /** The server refused the token (HTTP 401/403): e.g. the daemon's database was reset. */
    TokenRejected,

    /** The stored server URL is unusable. */
    InvalidServerUrl,
}

/** Why the last connection ended, reduced to what the user can act on. */
enum class RetryCause {
    /** The PC (or the path to it) did not answer: Tailscale off, PC asleep, daemon not running. */
    Unreachable,

    /** The daemon is restarting (close code 1001). */
    ServerRestarting,

    /**
     * The daemon stopped and said it will not start again by itself (`restartExpected: false`:
     * `stop`, drain, the end of the Windows session). The engine keeps retrying; it connects
     * once someone starts the daemon on the PC.
     */
    ServerStopped,

    /** The connection went silent and was dropped (watchdog / 4002). */
    Timeout,

    /** The phone switched networks. */
    NetworkChanged,

    /** The server reported a protocol violation (4003): an app bug; retries wait longest. */
    ProtocolError,

    /** Anything else (a local failure, the setup failed, other close codes). */
    Other,
}

/**
 * Everything the status bar and the persistent notification show. Equal values render equal
 * text, so `distinctUntilChanged` decides when the notification is re-posted.
 */
data class ConnectionPresentation(
    val summary: ConnectionSummary,
    val pendingOutbox: Int,
    /** The last time in step with the server; only while not connected (it is not shown otherwise). */
    val lastSyncAtMs: Long?,
    /** The server announced its shutdown, with this reason (`null`: it did not, or a session was established since). */
    val serverShutdown: ShutdownReason?,
    /** From that announcement: whether the server starts again by itself (`null` without one). */
    val serverRestartExpected: Boolean? = null,
) {
    /** The user should act (not merely wait). */
    val needsAttention: Boolean
        get() = summary is ConnectionSummary.ConnectedElsewhere ||
            summary is ConnectionSummary.NeedsPairing ||
            summary is ConnectionSummary.Incompatible ||
            summary is ConnectionSummary.KeystoreUnavailable

    companion object {
        /**
         * The engine's [status] in the UI's words. [pairing] is the stored pairing: while its
         * token waits for the keystore the engine has no credentials, which is not "not paired".
         */
        fun of(status: SyncStatus, pairing: PairingState?): ConnectionPresentation {
            val waitingForKeystore = pairing is PairingState.KeystoreUnavailable &&
                (status.connection == ConnectionState.NotPaired || status.connection == ConnectionState.Stopped)
            val summary = when {
                waitingForKeystore -> ConnectionSummary.KeystoreUnavailable(pairing.retryAtMs)
                else -> when (val s = summarize(status.connection)) {
                    // "Restarting" only when the server said it would be started again.
                    is ConnectionSummary.WaitingToRetry ->
                        if (s.cause == RetryCause.ServerRestarting && status.serverShuttingDown && status.serverRestartExpected == false) s.copy(cause = RetryCause.ServerStopped) else s
                    else -> s
                }
            }
            return ConnectionPresentation(
                summary = summary,
                pendingOutbox = status.pendingOutbox,
                // The engine advances it with every batch and heartbeat; while connected it is
                // not displayed, and keeping it would re-post the notification each time.
                lastSyncAtMs = if (summary is ConnectionSummary.Connected) null else status.lastSyncAtMs,
                serverShutdown = if (status.serverShuttingDown) status.serverShutdownReason ?: ShutdownReason.Unknown else null,
                serverRestartExpected = if (status.serverShuttingDown) status.serverRestartExpected else null,
            )
        }

        fun summarize(state: ConnectionState): ConnectionSummary = when (state) {
            ConnectionState.Stopped -> ConnectionSummary.Stopped
            ConnectionState.NotPaired -> ConnectionSummary.NeedsPairing(PairingNeed.NotPaired)
            ConnectionState.Offline -> ConnectionSummary.PhoneOffline
            is ConnectionState.Connecting -> ConnectionSummary.Connecting(state.attempt, state.reconnecting)
            is ConnectionState.Online -> ConnectionSummary.Connected(state.sinceMs)
            is ConnectionState.Reconnecting -> ConnectionSummary.WaitingToRetry(state.attempt, state.retryAtMs, retryCause(state.cause))
            is ConnectionState.Suspended -> when (val reason = state.reason) {
                SuspendReason.Replaced -> ConnectionSummary.ConnectedElsewhere
                SuspendReason.Revoked -> ConnectionSummary.NeedsPairing(PairingNeed.Revoked)
                is SuspendReason.Unauthorized -> ConnectionSummary.NeedsPairing(PairingNeed.TokenRejected)
                is SuspendReason.InvalidServerUrl -> ConnectionSummary.NeedsPairing(PairingNeed.InvalidServerUrl)
                is SuspendReason.Incompatible -> ConnectionSummary.Incompatible(reason.message)
            }
        }

        fun retryCause(cause: DisconnectCause): RetryCause = when (cause) {
            is DisconnectCause.Network -> RetryCause.Unreachable
            is DisconnectCause.Watchdog, DisconnectCause.ServerTimeout -> RetryCause.Timeout
            DisconnectCause.ServerShutdown -> RetryCause.ServerRestarting
            DisconnectCause.NetworkChanged -> RetryCause.NetworkChanged
            is DisconnectCause.ProtocolViolation -> RetryCause.ProtocolError
            DisconnectCause.MessageTooBig, is DisconnectCause.Closed, is DisconnectCause.SetupFailed,
            is DisconnectCause.ClientError, DisconnectCause.Reset,
            -> RetryCause.Other
        }
    }
}
