package dev.aas.android.sync

import dev.aas.android.protocol.BackgroundTask
import dev.aas.android.protocol.Harness
import dev.aas.android.protocol.Interaction
import dev.aas.android.protocol.Operation
import dev.aas.android.protocol.Project
import dev.aas.android.protocol.ProjectId
import dev.aas.android.protocol.QueuedInput
import dev.aas.android.protocol.Thread
import dev.aas.android.protocol.ThreadId
import dev.aas.android.protocol.Turn

/**
 * A write made inside a transaction. The engine records the writes of a transaction and,
 * once it committed, applies the same row changes to the in-memory views the UI collects, so
 * the views mirror the store without re-reading it after every batch.
 */
internal sealed interface StoreChange {
    data object Wiped : StoreChange

    data class EpochSet(val epoch: String) : StoreChange

    data class CursorSet(val stream: String, val seq: Long) : StoreChange

    data class LastSyncSet(val atMs: Long) : StoreChange

    data class HarnessesReplaced(val harnesses: List<Harness>) : StoreChange

    data class HarnessUpserted(val harness: Harness) : StoreChange

    data class ProjectUpserted(val project: Project) : StoreChange

    data class ProjectRemoved(val id: ProjectId) : StoreChange

    data class ThreadUpserted(val thread: Thread) : StoreChange

    data class ThreadRemoved(val id: ThreadId) : StoreChange

    data class OperationUpserted(val operation: Operation) : StoreChange

    data class ViewStateSet(val threadId: ThreadId, val state: ThreadViewState) : StoreChange

    data class TurnUpserted(val turn: Turn) : StoreChange

    data class ItemUpserted(val item: StoredItem) : StoreChange

    data class InteractionUpserted(val interaction: Interaction) : StoreChange

    data class BackgroundTaskUpserted(val task: BackgroundTask) : StoreChange

    data class QueueReplaced(val threadId: ThreadId, val queued: List<QueuedInput>) : StoreChange

    data class ThreadContentCleared(val threadId: ThreadId) : StoreChange

    data class ThreadMetaSet(val threadId: ThreadId, val meta: ThreadMeta) : StoreChange

    data class OutboxAdded(val entry: OutboxEntry) : StoreChange

    data class OutboxUpdated(val entry: OutboxEntry) : StoreChange

    data class OutboxRemoved(val clientRequestId: String) : StoreChange

    data object OutboxCleared : StoreChange
}

/** Forwards to [tx] and records every write. */
internal class RecordingTx(private val tx: SyncTx) : SyncTx by tx {
    val changes = ArrayList<StoreChange>()

    override suspend fun setEpoch(epoch: String) {
        tx.setEpoch(epoch)
        changes += StoreChange.EpochSet(epoch)
    }

    override suspend fun setCursor(stream: String, seq: Long) {
        tx.setCursor(stream, seq)
        changes += StoreChange.CursorSet(stream, seq)
    }

    override suspend fun setLastSyncAtMs(atMs: Long) {
        tx.setLastSyncAtMs(atMs)
        changes += StoreChange.LastSyncSet(atMs)
    }

    override suspend fun wipeSyncedData() {
        tx.wipeSyncedData()
        changes += StoreChange.Wiped
    }

    override suspend fun replaceHarnesses(harnesses: List<Harness>) {
        tx.replaceHarnesses(harnesses)
        changes += StoreChange.HarnessesReplaced(harnesses)
    }

    override suspend fun upsertHarness(harness: Harness) {
        tx.upsertHarness(harness)
        changes += StoreChange.HarnessUpserted(harness)
    }

    override suspend fun upsertProject(project: Project) {
        tx.upsertProject(project)
        changes += StoreChange.ProjectUpserted(project)
    }

    override suspend fun removeProject(id: ProjectId) {
        tx.removeProject(id)
        changes += StoreChange.ProjectRemoved(id)
    }

    override suspend fun upsertThread(thread: Thread) {
        tx.upsertThread(thread)
        changes += StoreChange.ThreadUpserted(thread)
    }

    override suspend fun removeThread(id: ThreadId) {
        tx.removeThread(id)
        changes += StoreChange.ThreadRemoved(id)
    }

    override suspend fun upsertOperation(operation: Operation) {
        tx.upsertOperation(operation)
        changes += StoreChange.OperationUpserted(operation)
    }

    override suspend fun setViewState(threadId: ThreadId, state: ThreadViewState) {
        tx.setViewState(threadId, state)
        changes += StoreChange.ViewStateSet(threadId, state)
    }

    override suspend fun upsertTurn(turn: Turn) {
        tx.upsertTurn(turn)
        changes += StoreChange.TurnUpserted(turn)
    }

    override suspend fun upsertItem(item: StoredItem) {
        tx.upsertItem(item)
        changes += StoreChange.ItemUpserted(item)
    }

    override suspend fun upsertInteraction(interaction: Interaction) {
        tx.upsertInteraction(interaction)
        changes += StoreChange.InteractionUpserted(interaction)
    }

    override suspend fun upsertBackgroundTask(task: BackgroundTask) {
        tx.upsertBackgroundTask(task)
        changes += StoreChange.BackgroundTaskUpserted(task)
    }

    override suspend fun replaceQueued(threadId: ThreadId, queued: List<QueuedInput>) {
        tx.replaceQueued(threadId, queued)
        changes += StoreChange.QueueReplaced(threadId, queued)
    }

    override suspend fun clearThreadContent(threadId: ThreadId) {
        tx.clearThreadContent(threadId)
        changes += StoreChange.ThreadContentCleared(threadId)
    }

    override suspend fun setThreadMeta(threadId: ThreadId, meta: ThreadMeta) {
        tx.setThreadMeta(threadId, meta)
        changes += StoreChange.ThreadMetaSet(threadId, meta)
    }

    override suspend fun addOutbox(entry: OutboxEntry) {
        tx.addOutbox(entry)
        changes += StoreChange.OutboxAdded(entry)
    }

    override suspend fun updateOutbox(entry: OutboxEntry) {
        tx.updateOutbox(entry)
        changes += StoreChange.OutboxUpdated(entry)
    }

    override suspend fun removeOutbox(clientRequestId: String): Boolean {
        val existed = tx.removeOutbox(clientRequestId)
        if (existed) changes += StoreChange.OutboxRemoved(clientRequestId)
        return existed
    }

    override suspend fun clearOutbox() {
        tx.clearOutbox()
        changes += StoreChange.OutboxCleared
    }
}
