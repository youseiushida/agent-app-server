package dev.aas.android.data.db

import androidx.room.withTransaction
import dev.aas.android.protocol.AasJson
import dev.aas.android.protocol.BackgroundTask
import dev.aas.android.protocol.BackgroundTaskId
import dev.aas.android.protocol.Harness
import dev.aas.android.protocol.Interaction
import dev.aas.android.protocol.InteractionId
import dev.aas.android.protocol.InteractionStatus
import dev.aas.android.protocol.Item
import dev.aas.android.protocol.ItemId
import dev.aas.android.protocol.Operation
import dev.aas.android.protocol.OperationId
import dev.aas.android.protocol.Project
import dev.aas.android.protocol.ProjectId
import dev.aas.android.protocol.QueuedInput
import dev.aas.android.protocol.Thread
import dev.aas.android.protocol.ThreadId
import dev.aas.android.protocol.Turn
import dev.aas.android.protocol.TurnId
import dev.aas.android.protocol.threadStream
import dev.aas.android.sync.ItemPosition
import dev.aas.android.sync.OutboxEntry
import dev.aas.android.sync.StoredItem
import dev.aas.android.sync.SyncStore
import dev.aas.android.sync.SyncTx
import dev.aas.android.sync.ThreadMeta
import dev.aas.android.sync.ThreadViewState
import kotlinx.serialization.KSerializer
import kotlinx.serialization.json.JsonObject

/**
 * The durable [SyncStore]: one Room transaction per engine block.
 *
 * * **Atomic:** [RoomDatabase.withTransaction][androidx.room.withTransaction] commits when the
 *   block returns and rolls back when it throws, including `CancellationException`.
 * * **Serialized:** Room runs one transaction at a time on its transaction thread; the engine
 *   never nests blocks.
 * * **Durable on return:** the commit has been written to the WAL file when `withTransaction`
 *   returns, so it survives the death of the app's process (power loss follows the platform's
 *   SQLite WAL sync mode).
 *
 * The contract is checked by `RoomSyncStoreTest`, which runs `SyncStoreContract` from `:sync`.
 */
class RoomSyncStore(private val db: AasDatabase) : SyncStore {
    private val dao = db.syncDao()

    override suspend fun <T> transaction(block: suspend (SyncTx) -> T): T = db.withTransaction {
        val tx = Tx(dao)
        try {
            block(tx)
        } finally {
            tx.open = false
        }
    }

    private class Tx(private val dao: SyncDao) : SyncTx {
        @Volatile
        var open = true

        private fun check() {
            check(open) { "SyncTx used outside its transaction" }
        }

        // ----- sync metadata --------------------------------------------------------------------

        override suspend fun epoch(): String? {
            check()
            return dao.meta(MetaKeys.EPOCH)
        }

        override suspend fun setEpoch(epoch: String) {
            check()
            dao.putMeta(MetaEntity(MetaKeys.EPOCH, epoch))
        }

        override suspend fun cursor(stream: String): Long? {
            check()
            return dao.cursor(stream)
        }

        override suspend fun cursors(): Map<String, Long> {
            check()
            return dao.cursors().associate { it.stream to it.seq }
        }

        override suspend fun setCursor(stream: String, seq: Long) {
            check()
            dao.putCursor(CursorEntity(stream, seq))
        }

        override suspend fun lastSyncAtMs(): Long? {
            check()
            val text = dao.meta(MetaKeys.LAST_SYNC_AT) ?: return null
            return text.toLongOrNull() ?: throw IllegalStateException("corrupt ${MetaKeys.LAST_SYNC_AT} value: $text")
        }

        override suspend fun setLastSyncAtMs(atMs: Long) {
            check()
            dao.putMeta(MetaEntity(MetaKeys.LAST_SYNC_AT, atMs.toString()))
        }

        override suspend fun wipeSyncedData() {
            check()
            dao.clearMeta()
            dao.clearCursors()
            dao.clearHarnesses()
            dao.clearProjects()
            dao.clearThreads()
            dao.clearTurns()
            dao.clearItems()
            dao.clearInteractions()
            dao.clearBackgroundTasks()
            dao.clearQueued()
            dao.clearOperations()
            dao.clearThreadMeta()
            dao.clearViewStates()
            // The outbox stays (SyncTx.wipeSyncedData).
        }

        // ----- workspace ------------------------------------------------------------------------

        override suspend fun harnesses(): List<Harness> {
            check()
            return dao.harnesses().map { decode(Harness.serializer(), it.json) }
        }

        override suspend fun replaceHarnesses(harnesses: List<Harness>) {
            check()
            dao.clearHarnesses()
            harnesses.forEachIndexed { i, h -> dao.upsertHarness(HarnessEntity(h.id, i, encode(Harness.serializer(), h))) }
        }

        override suspend fun upsertHarness(harness: Harness) {
            check()
            val position = dao.harnessPosition(harness.id) ?: ((dao.maxHarnessPosition() ?: -1) + 1)
            dao.upsertHarness(HarnessEntity(harness.id, position, encode(Harness.serializer(), harness)))
        }

        override suspend fun projects(): List<Project> {
            check()
            return dao.projects().map { decode(Project.serializer(), it.json) }
        }

        override suspend fun upsertProject(project: Project) {
            check()
            dao.upsertProject(ProjectEntity(project.id, encode(Project.serializer(), project)))
        }

        override suspend fun removeProject(id: ProjectId) {
            check()
            dao.deleteProject(id)
        }

        override suspend fun threads(): List<Thread> {
            check()
            return dao.threads().map { decode(Thread.serializer(), it.json) }
        }

        override suspend fun thread(id: ThreadId): Thread? {
            check()
            return dao.thread(id)?.let { decode(Thread.serializer(), it.json) }
        }

        override suspend fun upsertThread(thread: Thread) {
            check()
            dao.upsertThread(ThreadEntity(thread.id, encode(Thread.serializer(), thread)))
        }

        override suspend fun removeThread(id: ThreadId) {
            check()
            dao.deleteThread(id)
            dao.deleteTurnsOf(id)
            dao.deleteItemsOf(id)
            dao.deleteInteractionsOf(id)
            dao.deleteBackgroundTasksOf(id)
            dao.deleteQueuedOf(id)
            dao.deleteThreadMeta(id)
            dao.deleteViewState(id)
            dao.deleteCursor(threadStream(id))
        }

        override suspend fun operations(): List<Operation> {
            check()
            return dao.operations().map { decode(Operation.serializer(), it.json) }
        }

        override suspend fun operation(id: OperationId): Operation? {
            check()
            return dao.operation(id)?.let { decode(Operation.serializer(), it.json) }
        }

        override suspend fun upsertOperation(operation: Operation) {
            check()
            dao.upsertOperation(OperationEntity(operation.id, encode(Operation.serializer(), operation)))
        }

        override suspend fun pendingInteractions(): List<Interaction> {
            check()
            return dao.pendingInteractions().map { decode(Interaction.serializer(), it.json) }
        }

        override suspend fun viewStates(): Map<ThreadId, ThreadViewState> {
            check()
            return dao.viewStates().associate { it.threadId to ThreadViewState(it.lastViewedHead, it.markedUnread) }
        }

        override suspend fun viewState(threadId: ThreadId): ThreadViewState? {
            check()
            return dao.viewState(threadId)?.let { ThreadViewState(it.lastViewedHead, it.markedUnread) }
        }

        override suspend fun setViewState(threadId: ThreadId, state: ThreadViewState) {
            check()
            dao.putViewState(ViewStateEntity(threadId, state.lastViewedHead, state.markedUnread))
        }

        // ----- thread content -------------------------------------------------------------------

        override suspend fun turn(id: TurnId): Turn? {
            check()
            return dao.turn(id)?.let { decode(Turn.serializer(), it.json) }
        }

        override suspend fun upsertTurn(turn: Turn) {
            check()
            dao.upsertTurn(TurnEntity(turn.id, turn.threadId, turn.index, encode(Turn.serializer(), turn)))
        }

        override suspend fun turnsOf(threadId: ThreadId): List<Turn> {
            check()
            return dao.turnsOf(threadId).map { decode(Turn.serializer(), it.json) }
        }

        override suspend fun item(id: ItemId): StoredItem? {
            check()
            return dao.item(id)?.toStored()
        }

        override suspend fun upsertItem(item: StoredItem) {
            check()
            dao.upsertItem(
                ItemEntity(
                    id = item.item.id,
                    threadId = item.item.threadId,
                    turnIndex = item.position.turnIndex,
                    sortSeq = item.position.seq,
                    json = encode(Item.Serializer, item.item),
                ),
            )
        }

        override suspend fun itemsOf(threadId: ThreadId): List<StoredItem> {
            check()
            return dao.itemsOf(threadId).map { it.toStored() }
        }

        override suspend fun interaction(id: InteractionId): Interaction? {
            check()
            return dao.interaction(id)?.let { decode(Interaction.serializer(), it.json) }
        }

        override suspend fun upsertInteraction(interaction: Interaction) {
            check()
            dao.upsertInteraction(
                InteractionEntity(
                    id = interaction.id,
                    threadId = interaction.threadId,
                    pending = interaction.status == InteractionStatus.Pending,
                    createdAt = interaction.createdAt,
                    json = encode(Interaction.serializer(), interaction),
                ),
            )
        }

        override suspend fun interactionsOf(threadId: ThreadId): List<Interaction> {
            check()
            return dao.interactionsOf(threadId).map { decode(Interaction.serializer(), it.json) }
        }

        override suspend fun backgroundTask(id: BackgroundTaskId): BackgroundTask? {
            check()
            return dao.backgroundTask(id)?.let { decode(BackgroundTask.serializer(), it.json) }
        }

        override suspend fun upsertBackgroundTask(task: BackgroundTask) {
            check()
            dao.upsertBackgroundTask(BackgroundTaskEntity(task.id, task.threadId, task.startedAt, encode(BackgroundTask.serializer(), task)))
        }

        override suspend fun backgroundTasksOf(threadId: ThreadId): List<BackgroundTask> {
            check()
            return dao.backgroundTasksOf(threadId).map { decode(BackgroundTask.serializer(), it.json) }
        }

        override suspend fun queued(threadId: ThreadId): List<QueuedInput> {
            check()
            return dao.queuedOf(threadId).map { decode(QueuedInput.serializer(), it.json) }
        }

        override suspend fun replaceQueued(threadId: ThreadId, queued: List<QueuedInput>) {
            check()
            dao.deleteQueuedOf(threadId)
            if (queued.isNotEmpty()) {
                dao.insertQueued(queued.mapIndexed { i, q -> QueuedEntity(threadId, i, q.id, encode(QueuedInput.serializer(), q)) })
            }
        }

        override suspend fun clearThreadContent(threadId: ThreadId) {
            check()
            dao.deleteTurnsOf(threadId)
            dao.deleteItemsOf(threadId)
            dao.deleteBackgroundTasksOf(threadId)
            dao.deleteQueuedOf(threadId)
        }

        override suspend fun threadMeta(threadId: ThreadId): ThreadMeta {
            check()
            return dao.threadMeta(threadId)?.let { ThreadMeta(it.hasMoreBefore, it.commandsVersion) } ?: ThreadMeta()
        }

        override suspend fun setThreadMeta(threadId: ThreadId, meta: ThreadMeta) {
            check()
            dao.putThreadMeta(ThreadMetaEntity(threadId, meta.hasMoreBefore, meta.commandsVersion))
        }

        // ----- outbox ---------------------------------------------------------------------------

        override suspend fun outbox(): List<OutboxEntry> {
            check()
            return dao.outbox().map { it.toEntry() }
        }

        override suspend fun addOutbox(entry: OutboxEntry) {
            check()
            dao.insertOutbox(
                OutboxEntity(
                    clientRequestId = entry.clientRequestId,
                    method = entry.method,
                    params = encode(JsonObject.serializer(), entry.params),
                    createdAt = entry.createdAtMs,
                    failures = entry.failures,
                    lastError = entry.lastError,
                    nextAttemptAt = entry.nextAttemptAtMs,
                    waitingForHarness = entry.waitingForHarness,
                ),
            )
        }

        override suspend fun updateOutbox(entry: OutboxEntry) {
            check()
            // Zero rows updated means the entry was answered meanwhile: nothing to do.
            dao.updateOutbox(
                clientRequestId = entry.clientRequestId,
                method = entry.method,
                params = encode(JsonObject.serializer(), entry.params),
                createdAt = entry.createdAtMs,
                failures = entry.failures,
                lastError = entry.lastError,
                nextAttemptAt = entry.nextAttemptAtMs,
                waitingForHarness = entry.waitingForHarness,
            )
        }

        override suspend fun removeOutbox(clientRequestId: String): Boolean {
            check()
            return dao.deleteOutbox(clientRequestId) > 0
        }

        override suspend fun clearOutbox() {
            check()
            dao.clearOutbox()
        }

        private fun ItemEntity.toStored() = StoredItem(decode(Item.Serializer, json), ItemPosition(turnIndex, sortSeq))

        private fun OutboxEntity.toEntry() = OutboxEntry(
            clientRequestId = clientRequestId,
            method = method,
            params = decode(JsonObject.serializer(), params),
            createdAtMs = createdAt,
            failures = failures,
            lastError = lastError,
            nextAttemptAtMs = nextAttemptAt,
            waitingForHarness = waitingForHarness,
        )
    }

    private companion object {
        fun <T> encode(serializer: KSerializer<T>, value: T): String = AasJson.encodeToString(serializer, value)

        fun <T> decode(serializer: KSerializer<T>, json: String): T = AasJson.decodeFromString(serializer, json)
    }
}
