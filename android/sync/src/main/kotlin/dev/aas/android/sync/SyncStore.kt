package dev.aas.android.sync

import dev.aas.android.protocol.BackgroundTask
import dev.aas.android.protocol.BackgroundTaskId
import dev.aas.android.protocol.Harness
import dev.aas.android.protocol.Interaction
import dev.aas.android.protocol.InteractionId
import dev.aas.android.protocol.Item
import dev.aas.android.protocol.ItemId
import dev.aas.android.protocol.JsonKeys
import dev.aas.android.protocol.Operation
import dev.aas.android.protocol.OperationId
import dev.aas.android.protocol.Project
import dev.aas.android.protocol.ProjectId
import dev.aas.android.protocol.QueuedInput
import dev.aas.android.protocol.Thread
import dev.aas.android.protocol.ThreadId
import dev.aas.android.protocol.Turn
import dev.aas.android.protocol.TurnId
import kotlinx.serialization.json.JsonObject
import kotlinx.serialization.json.JsonPrimitive
import kotlinx.serialization.json.contentOrNull

/**
 * Durable local storage of the sync engine (Room on Android, [InMemorySyncStore] in tests).
 *
 * ## Transactions
 * Every read and write goes through [transaction]. The contract an implementation must keep:
 *
 * * **Atomic.** All writes of one block commit together, or — when the block throws, including
 *   `CancellationException` — none do. The engine applies a `stream/batch` and advances that
 *   stream's cursor in one block (protocol.md §2.1, §7.1), so after a crash the cursor never
 *   points past events that were not stored, nor before events that were.
 * * **Serialized.** Blocks do not interleave (Room: `withTransaction`, which runs one writer at
 *   a time). The engine never nests transactions.
 * * **Durable on return.** When [transaction] returns, the writes survive process death. The
 *   engine relies on this for the outbox: an entry is committed *before* its frame is sent
 *   (protocol.md §1.2, §7.4), so a request the server may have executed is never forgotten.
 *
 * Methods of [SyncTx] must only be called inside the block that received it.
 */
interface SyncStore {
    /** Runs [block] atomically and returns its result. */
    suspend fun <T> transaction(block: suspend (SyncTx) -> T): T
}

/**
 * Operations inside a [SyncStore.transaction]. Values are protocol objects; a Room
 * implementation stores them as JSON columns (`AasJson`) next to the columns it queries by.
 */
interface SyncTx {
    // ----- sync metadata ------------------------------------------------------------------------

    /** Epoch of the server database the synced data comes from, `null` before the first sync. */
    suspend fun epoch(): String?

    suspend fun setEpoch(epoch: String)

    /**
     * The [StoredModels.VERSION][dev.aas.android.protocol.StoredModels.VERSION] of the build that
     * stored the synced data, `null` when none is recorded (before the first sync, or data stored
     * by a build that did not record it). The engine trusts stored data only when it is this
     * build's version; otherwise it reads everything again (docs/android.md 6.1).
     */
    suspend fun modelVersion(): Int?

    suspend fun setModelVersion(version: Int)

    /** Highest applied sequence number of [stream] (the read position), or `null`. */
    suspend fun cursor(stream: String): Long?

    /** Every stored read position. */
    suspend fun cursors(): Map<String, Long>

    suspend fun setCursor(stream: String, seq: Long)

    /** Last time the engine knew the local state to be in step with the server (wall clock). */
    suspend fun lastSyncAtMs(): Long?

    suspend fun setLastSyncAtMs(atMs: Long)

    /**
     * Deletes all synced data: epoch, model version, cursors, last sync time, harnesses, projects, threads,
     * turns, items, interactions, background tasks, queued inputs, operations, thread metadata and
     * view states.
     * **Keeps the outbox**: requests the user made are still sent after an epoch change
     * (protocol.md §2, §7.3); the server answers them definitively if they no longer apply.
     */
    suspend fun wipeSyncedData()

    // ----- workspace ----------------------------------------------------------------------------

    suspend fun harnesses(): List<Harness>

    /** Replaces the whole harness list (snapshot). */
    suspend fun replaceHarnesses(harnesses: List<Harness>)

    suspend fun upsertHarness(harness: Harness)

    suspend fun projects(): List<Project>

    suspend fun upsertProject(project: Project)

    suspend fun removeProject(id: ProjectId)

    suspend fun threads(): List<Thread>

    suspend fun thread(id: ThreadId): Thread?

    suspend fun upsertThread(thread: Thread)

    /**
     * Removes a thread and everything stored for it: turns, items, interactions, background tasks,
     * queued inputs, metadata, view state and the cursor of its stream.
     */
    suspend fun removeThread(id: ThreadId)

    suspend fun operations(): List<Operation>

    suspend fun operation(id: OperationId): Operation?

    suspend fun upsertOperation(operation: Operation)

    /** Interactions whose status is `pending`, of every thread. */
    suspend fun pendingInteractions(): List<Interaction>

    /** Local per-thread view state (unread marker) of every thread that has one. */
    suspend fun viewStates(): Map<ThreadId, ThreadViewState>

    suspend fun viewState(threadId: ThreadId): ThreadViewState?

    suspend fun setViewState(threadId: ThreadId, state: ThreadViewState)

    // ----- thread content -----------------------------------------------------------------------

    suspend fun turn(id: TurnId): Turn?

    suspend fun upsertTurn(turn: Turn)

    /** Turns of a thread, ascending by index. */
    suspend fun turnsOf(threadId: ThreadId): List<Turn>

    suspend fun item(id: ItemId): StoredItem?

    /** Inserts or replaces an item together with its position (see [ItemPosition]). */
    suspend fun upsertItem(item: StoredItem)

    /** Items of a thread, ascending by [ItemPosition]. */
    suspend fun itemsOf(threadId: ThreadId): List<StoredItem>

    suspend fun interaction(id: InteractionId): Interaction?

    suspend fun upsertInteraction(interaction: Interaction)

    /** Interactions of a thread (every status), ascending by `createdAt`, then id. */
    suspend fun interactionsOf(threadId: ThreadId): List<Interaction>

    suspend fun backgroundTask(id: BackgroundTaskId): BackgroundTask?

    /** Inserts or replaces a background task (`backgroundTask/updated` carries the whole task). */
    suspend fun upsertBackgroundTask(task: BackgroundTask)

    /** Background tasks of a thread, ascending by `startedAt`, then id. */
    suspend fun backgroundTasksOf(threadId: ThreadId): List<BackgroundTask>

    suspend fun queued(threadId: ThreadId): List<QueuedInput>

    /** Replaces the queue of a thread (`queue/updated` carries the whole queue). */
    suspend fun replaceQueued(threadId: ThreadId, queued: List<QueuedInput>)

    /**
     * Drops the cached turns, items, background tasks and queued inputs of a thread before a fresh
     * `thread/read` (which returns the tasks of its turns and every running one). Interactions,
     * the summary, metadata, view state and the cursor stay: interactions are kept current by the
     * workspace stream as well, and the caller sets the cursor itself.
     */
    suspend fun clearThreadContent(threadId: ThreadId)

    suspend fun threadMeta(threadId: ThreadId): ThreadMeta

    suspend fun setThreadMeta(threadId: ThreadId, meta: ThreadMeta)

    // ----- outbox -------------------------------------------------------------------------------

    /** Pending mutating requests in the order they were added. */
    suspend fun outbox(): List<OutboxEntry>

    /** Appends an entry (its `clientRequestId` is new). */
    suspend fun addOutbox(entry: OutboxEntry)

    /**
     * Replaces the entry with the same `clientRequestId` (retry bookkeeping), keeping its place.
     * Does nothing when no such entry exists (it was answered meanwhile).
     */
    suspend fun updateOutbox(entry: OutboxEntry)

    /** Removes an entry; returns whether it existed. */
    suspend fun removeOutbox(clientRequestId: String): Boolean

    /** Deletes every outbox entry (unpairing). */
    suspend fun clearOutbox()
}

/**
 * Where an item sorts within its thread: by the index of its turn, then by [seq]. Items that
 * arrived as events use the event's `seq`; items from `thread/read` use negative numbers in
 * the order the server returned them (all before any live event of the same turn).
 */
data class ItemPosition(val turnIndex: Int, val seq: Long) : Comparable<ItemPosition> {
    override fun compareTo(other: ItemPosition): Int =
        compareValuesBy(this, other, ItemPosition::turnIndex, ItemPosition::seq)

    companion object {
        /** Turn index of an item whose turn is not stored (sorts after every known turn). */
        const val UNKNOWN_TURN: Int = Int.MAX_VALUE
    }
}

/** An item with its sort position. */
data class StoredItem(val item: Item, val position: ItemPosition)

/** Per-thread data that is not part of the protocol's thread summary. */
data class ThreadMeta(
    /** `thread/read` reported older turns than the ones stored. */
    val hasMoreBefore: Boolean = false,
    /** Bumped by `commands/changed`: the app refetches `command/list` when it changes. */
    val commandsVersion: Int = 0,
)

/**
 * Local unread state of a thread (design.md §15: unread is per device and never written to
 * the server). A thread is unread when [markedUnread] or when its summary's `head` is beyond
 * [lastViewedHead].
 */
data class ThreadViewState(val lastViewedHead: Long = 0, val markedUnread: Boolean = false)

/** A mutating request waiting for its final answer (protocol.md §1.2). */
data class OutboxEntry(
    val clientRequestId: String,
    val method: String,
    /** Complete params, including `clientRequestId`. */
    val params: JsonObject,
    val createdAtMs: Long,
    /** Non-definitive failures so far. */
    val failures: Int = 0,
    /** Why the last attempt failed (for `harnessUnavailable`: the server's reason). */
    val lastError: String? = null,
    /** Earliest time (wall clock) of the next send after a non-definitive failure. */
    val nextAttemptAtMs: Long = 0,
    /**
     * The harness this request waits for: the server answered `harnessUnavailable` for it
     * (protocol.md §1.3). While the synced workspace does not list that harness as available,
     * the request is not resent (the server probes again on its own and publishes
     * `harness/updated`); once it does, the request is sent at once. `null` for any other state.
     */
    val waitingForHarness: String? = null,
    /**
     * The `clientRequestId` of the entry before this one in its chain ([SyncEngine.submitChain]),
     * while that entry waits for its answer: this one is not sent before it succeeded, and is
     * dropped (never sent) when it fails definitively or is discarded. `null` for an entry that
     * may be sent (not chained, or its predecessor succeeded). An entry chained after a
     * `thread/create` has no `threadId` until the creation succeeded.
     */
    val after: String? = null,
) {
    /** The thread the request targets, when it names one. */
    val threadId: ThreadId? get() = (params[JsonKeys.THREAD_ID] as? JsonPrimitive)?.contentOrNull

    /** The project the request targets, when it names one. */
    val projectId: ProjectId? get() = (params[JsonKeys.PROJECT_ID] as? JsonPrimitive)?.contentOrNull
}
