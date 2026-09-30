package dev.aas.android.sync

import dev.aas.android.protocol.BackgroundTask
import dev.aas.android.protocol.BackgroundTaskId
import dev.aas.android.protocol.Harness
import dev.aas.android.protocol.Interaction
import dev.aas.android.protocol.InteractionId
import dev.aas.android.protocol.InteractionStatus
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
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.flow.asStateFlow
import kotlinx.coroutines.sync.Mutex
import kotlinx.coroutines.sync.withLock

/**
 * In-memory [SyncStore] for tests and previews. It keeps the whole contract except durability:
 * a transaction works on an immutable snapshot and publishes the result only when the block
 * returns normally, so a failing or cancelled block leaves no trace; a mutex serializes blocks.
 */
class InMemorySyncStore : SyncStore {
    /** Everything the store holds (immutable). */
    data class State(
        val epoch: String? = null,
        val modelVersion: Int? = null,
        val cursors: Map<String, Long> = emptyMap(),
        val lastSyncAtMs: Long? = null,
        val harnesses: Map<String, Harness> = emptyMap(),
        val projects: Map<ProjectId, Project> = emptyMap(),
        val threads: Map<ThreadId, Thread> = emptyMap(),
        val operations: Map<OperationId, Operation> = emptyMap(),
        val turns: Map<TurnId, Turn> = emptyMap(),
        val items: Map<ItemId, StoredItem> = emptyMap(),
        val interactions: Map<InteractionId, Interaction> = emptyMap(),
        val backgroundTasks: Map<BackgroundTaskId, BackgroundTask> = emptyMap(),
        val queued: Map<ThreadId, List<QueuedInput>> = emptyMap(),
        val meta: Map<ThreadId, ThreadMeta> = emptyMap(),
        val viewStates: Map<ThreadId, ThreadViewState> = emptyMap(),
        val outbox: List<OutboxEntry> = emptyList(),
    )

    private val mutex = Mutex()
    private val _state = MutableStateFlow(State())

    /** The committed state. */
    val state: StateFlow<State> = _state.asStateFlow()

    /** Number of committed transactions (tests check that work happened in one). */
    @Volatile
    var commits: Long = 0
        private set

    override suspend fun <T> transaction(block: suspend (SyncTx) -> T): T = mutex.withLock {
        val tx = Tx(_state.value)
        val result = try {
            block(tx)
        } finally {
            tx.open = false
        }
        _state.value = tx.s
        commits++
        result
    }

    /** Items of a thread in their stored order. */
    fun itemsOf(threadId: ThreadId): List<StoredItem> =
        state.value.items.values.filter { it.item.threadId == threadId }.sortedBy { it.position }

    private class Tx(var s: State) : SyncTx {
        var open = true

        private fun check() {
            check(open) { "SyncTx used outside its transaction" }
        }

        private inline fun <R> read(f: () -> R): R {
            check()
            return f()
        }

        private inline fun write(f: (State) -> State) {
            check()
            s = f(s)
        }

        override suspend fun epoch() = read { s.epoch }

        override suspend fun setEpoch(epoch: String) = write { it.copy(epoch = epoch) }

        override suspend fun modelVersion() = read { s.modelVersion }

        override suspend fun setModelVersion(version: Int) = write { it.copy(modelVersion = version) }

        override suspend fun cursor(stream: String) = read { s.cursors[stream] }

        override suspend fun cursors() = read { s.cursors }

        override suspend fun setCursor(stream: String, seq: Long) = write { it.copy(cursors = it.cursors + (stream to seq)) }

        override suspend fun lastSyncAtMs() = read { s.lastSyncAtMs }

        override suspend fun setLastSyncAtMs(atMs: Long) = write { it.copy(lastSyncAtMs = atMs) }

        override suspend fun wipeSyncedData() = write { State(outbox = it.outbox) }

        override suspend fun harnesses() = read { s.harnesses.values.toList() }

        override suspend fun replaceHarnesses(harnesses: List<Harness>) = write { it.copy(harnesses = harnesses.associateBy { h -> h.id }) }

        override suspend fun upsertHarness(harness: Harness) = write { it.copy(harnesses = it.harnesses + (harness.id to harness)) }

        override suspend fun projects() = read { s.projects.values.toList() }

        override suspend fun upsertProject(project: Project) = write { it.copy(projects = it.projects + (project.id to project)) }

        override suspend fun removeProject(id: ProjectId) = write { it.copy(projects = it.projects - id) }

        override suspend fun threads() = read { s.threads.values.toList() }

        override suspend fun thread(id: ThreadId) = read { s.threads[id] }

        override suspend fun upsertThread(thread: Thread) = write { it.copy(threads = it.threads + (thread.id to thread)) }

        override suspend fun removeThread(id: ThreadId) = write { st ->
            st.copy(
                threads = st.threads - id,
                turns = st.turns.filterValues { it.threadId != id },
                items = st.items.filterValues { it.item.threadId != id },
                interactions = st.interactions.filterValues { it.threadId != id },
                backgroundTasks = st.backgroundTasks.filterValues { it.threadId != id },
                queued = st.queued - id,
                meta = st.meta - id,
                viewStates = st.viewStates - id,
                cursors = st.cursors - threadStream(id),
            )
        }

        override suspend fun operations() = read { s.operations.values.toList() }

        override suspend fun operation(id: OperationId) = read { s.operations[id] }

        override suspend fun upsertOperation(operation: Operation) =
            write { it.copy(operations = it.operations + (operation.id to operation)) }

        override suspend fun pendingInteractions() = read { s.interactions.values.filter { it.status == InteractionStatus.Pending } }

        override suspend fun viewStates() = read { s.viewStates }

        override suspend fun viewState(threadId: ThreadId) = read { s.viewStates[threadId] }

        override suspend fun setViewState(threadId: ThreadId, state: ThreadViewState) =
            write { it.copy(viewStates = it.viewStates + (threadId to state)) }

        override suspend fun turn(id: TurnId) = read { s.turns[id] }

        override suspend fun upsertTurn(turn: Turn) = write { it.copy(turns = it.turns + (turn.id to turn)) }

        override suspend fun turnsOf(threadId: ThreadId) = read { s.turns.values.filter { it.threadId == threadId }.sortedBy { it.index } }

        override suspend fun item(id: ItemId) = read { s.items[id] }

        override suspend fun upsertItem(item: StoredItem) = write { it.copy(items = it.items + (item.item.id to item)) }

        override suspend fun itemsOf(threadId: ThreadId) =
            read { s.items.values.filter { it.item.threadId == threadId }.sortedBy { it.position } }

        override suspend fun interaction(id: InteractionId) = read { s.interactions[id] }

        override suspend fun upsertInteraction(interaction: Interaction) =
            write { it.copy(interactions = it.interactions + (interaction.id to interaction)) }

        override suspend fun interactionsOf(threadId: ThreadId) = read {
            s.interactions.values.filter { it.threadId == threadId }.sortedWith(compareBy({ it.createdAt }, { it.id }))
        }

        override suspend fun backgroundTask(id: BackgroundTaskId) = read { s.backgroundTasks[id] }

        override suspend fun upsertBackgroundTask(task: BackgroundTask) =
            write { it.copy(backgroundTasks = it.backgroundTasks + (task.id to task)) }

        override suspend fun backgroundTasksOf(threadId: ThreadId) = read {
            s.backgroundTasks.values.filter { it.threadId == threadId }.sortedWith(compareBy({ it.startedAt }, { it.id }))
        }

        override suspend fun queued(threadId: ThreadId) = read { s.queued[threadId].orEmpty() }

        override suspend fun replaceQueued(threadId: ThreadId, queued: List<QueuedInput>) =
            write { it.copy(queued = it.queued + (threadId to queued)) }

        override suspend fun clearThreadContent(threadId: ThreadId) = write { st ->
            st.copy(
                turns = st.turns.filterValues { it.threadId != threadId },
                items = st.items.filterValues { it.item.threadId != threadId },
                backgroundTasks = st.backgroundTasks.filterValues { it.threadId != threadId },
                queued = st.queued - threadId,
            )
        }

        override suspend fun threadMeta(threadId: ThreadId) = read { s.meta[threadId] ?: ThreadMeta() }

        override suspend fun setThreadMeta(threadId: ThreadId, meta: ThreadMeta) = write { it.copy(meta = it.meta + (threadId to meta)) }

        override suspend fun outbox() = read { s.outbox }

        override suspend fun addOutbox(entry: OutboxEntry) = write { st ->
            require(st.outbox.none { it.clientRequestId == entry.clientRequestId }) { "duplicate clientRequestId ${entry.clientRequestId}" }
            st.copy(outbox = st.outbox + entry)
        }

        override suspend fun updateOutbox(entry: OutboxEntry) = write { st ->
            st.copy(outbox = st.outbox.map { if (it.clientRequestId == entry.clientRequestId) entry else it })
        }

        override suspend fun removeOutbox(clientRequestId: String): Boolean {
            check()
            val existed = s.outbox.any { it.clientRequestId == clientRequestId }
            if (existed) s = s.copy(outbox = s.outbox.filterNot { it.clientRequestId == clientRequestId })
            return existed
        }

        override suspend fun clearOutbox() = write { it.copy(outbox = emptyList()) }
    }
}
