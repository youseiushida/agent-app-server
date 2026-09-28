package dev.aas.android.sync

import dev.aas.android.protocol.ClientPolicy
import dev.aas.android.protocol.Harness
import dev.aas.android.protocol.Interaction
import dev.aas.android.protocol.InteractionId
import dev.aas.android.protocol.InteractionStatus
import dev.aas.android.protocol.Item
import dev.aas.android.protocol.Operation
import dev.aas.android.protocol.Project
import dev.aas.android.protocol.QueuedInput
import dev.aas.android.protocol.RpcError
import dev.aas.android.protocol.ServerInfo
import dev.aas.android.protocol.ShutdownReason
import dev.aas.android.protocol.Thread
import dev.aas.android.protocol.ThreadId
import dev.aas.android.protocol.Turn
import dev.aas.android.protocol.TurnSummary
import kotlinx.coroutines.CompletableDeferred
import kotlinx.serialization.json.JsonElement

/** Where and how to connect: the paired server's WebSocket URL and this device's token. */
data class Credentials(val wsUrl: String, val token: String) {
    override fun toString(): String = "Credentials(wsUrl=$wsUrl, token=<redacted>)"
}

/** State of the connection to the server. */
sealed interface ConnectionState {
    /** The engine is not started (or was stopped). */
    data object Stopped : ConnectionState

    /** No credentials: the device has to be paired. */
    data object NotPaired : ConnectionState

    /** The app reported that no network is available; nothing is attempted until one is. */
    data object Offline : ConnectionState

    /**
     * Opening the socket, then `initialize` and the initial sync or resubscription.
     * [attempt] counts failed attempts since the last established session; [reconnecting] is
     * true when a session was established before (the UI shows "reconnecting").
     */
    data class Connecting(val attempt: Int, val reconnecting: Boolean) : ConnectionState

    /** Initialized and subscribed: events are live and the outbox is being sent. */
    data class Online(val sinceMs: Long) : ConnectionState

    /** Waiting until [retryAtMs] (wall clock) before the next attempt. */
    data class Reconnecting(val attempt: Int, val retryAtMs: Long, val cause: DisconnectCause) : ConnectionState

    /** Not connecting until the app acts; see [SuspendReason] for what resumes it. */
    data class Suspended(val reason: SuspendReason) : ConnectionState
}

/** Why the engine stopped reconnecting on its own (protocol.md §2.3). */
sealed interface SuspendReason {
    /** Close code 4001: this device was revoked. Pair again; new credentials resume. */
    data object Revoked : SuspendReason

    /** The upgrade was refused with HTTP [httpStatus] (401/403): the token is not valid. Pair again. */
    data class Unauthorized(val httpStatus: Int) : SuspendReason

    /**
     * Close code 4000: a newer connection of this device took over ("connected elsewhere").
     * Reconnecting on its own would fight that connection; [SyncEngine.reconnectNow] (user
     * action) and [SyncEngine.onAppForeground] resume.
     */
    data object Replaced : SuspendReason

    /** The server does not speak this client's protocol version. [SyncEngine.reconnectNow] retries. */
    data class Incompatible(val message: String) : SuspendReason

    /** The stored server URL cannot be used. New credentials resume. */
    data class InvalidServerUrl(val message: String) : SuspendReason
}

/** Why the last connection ended (shown while waiting to reconnect). */
sealed interface DisconnectCause {
    /** The socket could not be opened or failed (network error, refused, timed out). */
    data class Network(val message: String) : DisconnectCause

    /** No frame arrived within the client timeout (protocol.md §2.2); the socket was closed. */
    data class Watchdog(val timeoutMs: Long) : DisconnectCause

    /** Close code 1001 (after `server/shuttingDown`): the server is stopping or restarting. */
    data object ServerShutdown : DisconnectCause

    /** Close code 4002: the server heard nothing from this client within its timeout. */
    data object ServerTimeout : DisconnectCause

    /**
     * Close code 4003: the server saw a protocol violation. Reconnecting cannot fix it, so the
     * engine waits the maximum backoff before each attempt (only a user action skips it).
     */
    data class ProtocolViolation(val reason: String) : DisconnectCause

    /** Close code 1009: a request exceeded the server's frame limit; that request was failed. */
    data object MessageTooBig : DisconnectCause

    /** Any other close code. */
    data class Closed(val code: Int, val reason: String) : DisconnectCause

    /** `initialize`, the snapshot or the resubscription failed. */
    data class SetupFailed(val message: String) : DisconnectCause

    /**
     * This client failed while handling the connection (e.g. the local store threw); the
     * session was dropped and is re-established from the stored state. See [SyncStatus.lastError].
     */
    data class ClientError(val message: String) : DisconnectCause

    /** The app reported a new default network; the old socket was dropped on purpose. */
    data object NetworkChanged : DisconnectCause

    /** New credentials or a local data reset replaced the connection. */
    data object Reset : DisconnectCause
}

/** A problem the UI may show (with the time it happened). */
data class SyncError(val atMs: Long, val message: String)

/** Connection and synchronisation status (status bar, settings, diagnostics). */
data class SyncStatus(
    val connection: ConnectionState = ConnectionState.Stopped,
    /** Last time the local state was known to be in step with the server (persisted). */
    val lastSyncAtMs: Long? = null,
    /** Requests in the outbox (not yet answered definitively). */
    val pendingOutbox: Int = 0,
    val server: ServerInfo? = null,
    val deviceId: String? = null,
    /** Policy announced by the server in the last `initialize`. */
    val policy: ClientPolicy? = null,
    /** The server announced its shutdown (`server/shuttingDown`); the engine reconnects with backoff. */
    val serverShuttingDown: Boolean = false,
    /** Why, from that announcement (e.g. [ShutdownReason.StorageFailure]); `null` once a session is established again. */
    val serverShutdownReason: ShutdownReason? = null,
    /**
     * From that announcement: the server will be started again by itself (`restartExpected`).
     * `false`: it stopped for good until someone starts it (the engine still retries with
     * backoff). `null` once a session is established again.
     */
    val serverRestartExpected: Boolean? = null,
    val lastError: SyncError? = null,
    /** Sessions established after the first one since [SyncEngine.start]. */
    val reconnects: Int = 0,
    /** Streams resubscribed because the heartbeat head stayed ahead of the cursor. */
    val stallResubscribes: Int = 0,
    /** Signals dropped because the collector of [SyncEngine.signals] fell far behind. */
    val droppedSignals: Int = 0,
    /** Stored read positions (diagnostics). */
    val cursors: Map<String, Long> = emptyMap(),
    /** Stream heads from the last heartbeat (diagnostics: how far behind the client is). */
    val serverHeads: Map<String, Long> = emptyMap(),
    val lastHeartbeatAtMs: Long? = null,
) {
    val isOnline: Boolean get() = connection is ConnectionState.Online
}

/** A thread of the workspace list with its local unread flag. */
data class ThreadEntry(val thread: Thread, val unread: Boolean)

/** The synced workspace (from `workspace/snapshot` and the workspace stream). */
data class WorkspaceState(
    /** The store holds data of a server (it synced at least once). */
    val synced: Boolean,
    val harnesses: List<Harness>,
    /** Sorted by name (case-insensitive), then id. Archived projects are included. */
    val projects: List<Project>,
    /** Sorted like `thread/list`: `lastActivityAt` descending, then id descending. */
    val threads: List<ThreadEntry>,
    /** Pending interactions of every thread, oldest first. */
    val pendingInteractions: List<Interaction>,
    /** Newest first. */
    val operations: List<Operation>,
) {
    companion object {
        val Empty = WorkspaceState(false, emptyList(), emptyList(), emptyList(), emptyList(), emptyList())
    }
}

/** How current an open thread's content is. */
enum class ThreadSync {
    /** Content comes from the local store (no live subscription on the current connection). */
    Cached,

    /** `thread/read` is in flight. */
    Loading,

    /** Subscribed: events are applied as they arrive. */
    Live,

    /**
     * Loading it failed on this connection (see [ThreadState.loadError]); the stored content is
     * shown and its stream is not followed. Only this thread is affected: the rest of the
     * session goes on. [SyncEngine.retryThread] and the next connection load it again.
     */
    Failed,

    /** The thread no longer exists on the server. */
    Removed,
}

/** An open thread: summary, content and the requests of it still in the outbox. */
data class ThreadState(
    val threadId: ThreadId,
    val sync: ThreadSync,
    val thread: Thread?,
    /** Ascending by index. Only the loaded pages (see [hasMoreBefore]). */
    val turns: List<Turn>,
    /** Turn order, then arrival order. */
    val items: List<Item>,
    /** Every interaction of the loaded turns (all statuses), oldest first. */
    val interactions: List<Interaction>,
    val queued: List<QueuedInput>,
    /** Older turns exist on the server ([SyncEngine.loadOlder]). */
    val hasMoreBefore: Boolean,
    /** Changes when the harness command list changed (refetch `command/list`). */
    val commandsVersion: Int,
    /** Requests for this thread still in the outbox (e.g. a `turn/start` sent while offline). */
    val pending: List<OutboxEntry>,
    /** Why the last load failed while [sync] is [ThreadSync.Failed]. */
    val loadError: SyncError? = null,
) {
    companion object {
        fun empty(threadId: ThreadId) =
            ThreadState(threadId, ThreadSync.Cached, null, emptyList(), emptyList(), emptyList(), emptyList(), false, 0, emptyList())
    }
}

/**
 * Something the app may notify about. Emitted once, after the change is committed to the
 * store; replays of already applied events never emit again.
 */
sealed interface SyncSignal {
    /** An interaction (approval or question) became pending. [thread] is its summary, if known. */
    data class InteractionPending(val interaction: Interaction, val thread: Thread?) : SyncSignal

    /** A pending interaction was resolved or expired (dismiss its notification). */
    data class InteractionClosed(val interactionId: InteractionId, val threadId: ThreadId, val status: InteractionStatus) :
        SyncSignal

    /**
     * The last turn of [thread] ended ([turn] has a terminal status). Derived from the thread
     * summary: its `lastTurn` went from running (or another turn) to a terminal status.
     */
    data class TurnFinished(val thread: Thread, val turn: TurnSummary) : SyncSignal

    /** An operation (git clone) ended. */
    data class OperationFinished(val operation: Operation) : SyncSignal

    /** A thread was deleted on the server (its local data is gone). */
    data class ThreadRemoved(val threadId: ThreadId) : SyncSignal
}

/** The final outcome of an outbox entry. */
sealed interface OutboxResult {
    val entry: OutboxEntry

    data class Succeeded(override val entry: OutboxEntry, val result: JsonElement) : OutboxResult

    /** A definitive error (protocol.md §1.3): resending would not change it. */
    data class Failed(override val entry: OutboxEntry, val error: RpcError) : OutboxResult

    /** Dropped without an answer: by [SyncEngine.resetLocalData] or [SyncEngine.discardOutbox]. */
    data class Discarded(override val entry: OutboxEntry) : OutboxResult
}

/** Outcome of [SyncEngine.discardOutbox]. */
enum class OutboxDiscard {
    /** The entry left the outbox; it is never sent again. */
    Discarded,

    /**
     * Its frame is on the wire right now, so the server may be running it: it stays. It can be
     * discarded once it failed (then it waits for its retry) or it is answered.
     */
    InFlight,

    /** No such entry (it was answered or discarded meanwhile). */
    NotFound,
}

/**
 * A request committed to the outbox by [SyncEngine.submit]. The request is sent whether or not
 * anyone waits; [await] returns its final answer.
 */
class PendingMutation<R> internal constructor(
    val clientRequestId: String,
    private val answer: CompletableDeferred<JsonElement>,
    private val decode: (JsonElement) -> R,
) {
    /**
     * Waits for the final answer. Cancelling the wait does not cancel the request.
     *
     * @throws dev.aas.android.protocol.RpcException the server answered with a definitive error.
     * @throws OutboxClearedException the entry was dropped before an answer.
     */
    suspend fun await(): R = decode(answer.await())

    /**
     * Waits until the server accepted the request, without reading its result (for a caller
     * that only needs to know whether it was refused). Cancelling the wait does not cancel the
     * request.
     *
     * @throws dev.aas.android.protocol.RpcException the server answered with a definitive error.
     * @throws OutboxClearedException the entry was dropped before an answer.
     */
    suspend fun awaitAccepted() {
        answer.await()
    }
}

/** Thrown by read-only calls when no session is established. */
class NotConnectedException : Exception("not connected to the server")

/** The connection closed before the response arrived; the call may or may not have run. */
class ConnectionLostException(message: String) : Exception(message)

/** A call got no response within [SyncConfig.callTimeoutMs] (the connection stays open). */
class CallTimeoutException(val method: String, val timeoutMs: Long) : Exception("$method: no response within $timeoutMs ms")

/**
 * Thrown to callers waiting in [SyncEngine.mutate] (or [PendingMutation.await]) when the entry
 * left the outbox without an answer: [SyncEngine.resetLocalData] or [SyncEngine.discardOutbox].
 */
class OutboxClearedException : Exception("the pending request was discarded")
