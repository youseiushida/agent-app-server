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
import java.util.concurrent.ConcurrentHashMap

/**
 * In-memory mirror of the store rows the UI shows: the workspace, the outbox, and the content
 * of open threads. Loaded from the store once, then kept current by applying the recorded
 * changes of every committed transaction (in commit order: the engine calls [apply] under its
 * write lock). Not thread-safe on its own.
 */
internal class Views {
    private val harnesses = LinkedHashMap<String, Harness>()
    private val projects = HashMap<ProjectId, Project>()
    private val threads = HashMap<ThreadId, Thread>()
    private val viewStates = HashMap<ThreadId, ThreadViewState>()
    private val pending = HashMap<InteractionId, Interaction>()
    private val operations = HashMap<OperationId, Operation>()
    private var epoch: String? = null
    private val outboxEntries = ArrayList<OutboxEntry>()
    /** Concurrent: [thread] is read without the engine's write lock. */
    private val open = ConcurrentHashMap<ThreadId, OpenThread>()

    private val _workspace = MutableStateFlow(WorkspaceState.Empty)
    val workspace: StateFlow<WorkspaceState> = _workspace.asStateFlow()

    private val _outbox = MutableStateFlow<List<OutboxEntry>>(emptyList())
    val outbox: StateFlow<List<OutboxEntry>> = _outbox.asStateFlow()

    /** Cursors as stored (for stall detection and diagnostics; read without the write lock). */
    val cursors = ConcurrentHashMap<String, Long>()

    var lastSyncAtMs: Long? = null
        private set

    private class OpenThread(val flow: MutableStateFlow<ThreadState>) {
        var sync = ThreadSync.Cached
        var loadError: SyncError? = null
        val turns = HashMap<TurnId, Turn>()
        val items = HashMap<ItemId, StoredItem>()
        val interactions = HashMap<InteractionId, Interaction>()
        val tasks = HashMap<BackgroundTaskId, BackgroundTask>()
        var queued: List<QueuedInput> = emptyList()
        var meta = ThreadMeta()
        var dirty = false
    }

    /** Replaces the workspace mirror with the store's content. */
    suspend fun loadWorkspace(tx: SyncTx) {
        epoch = tx.epoch()
        harnesses.clear()
        tx.harnesses().forEach { harnesses[it.id] = it }
        projects.clear()
        tx.projects().forEach { projects[it.id] = it }
        threads.clear()
        tx.threads().forEach { threads[it.id] = it }
        viewStates.clear()
        viewStates.putAll(tx.viewStates())
        pending.clear()
        tx.pendingInteractions().forEach { pending[it.id] = it }
        operations.clear()
        tx.operations().forEach { operations[it.id] = it }
        outboxEntries.clear()
        outboxEntries.addAll(tx.outbox())
        cursors.clear()
        cursors.putAll(tx.cursors())
        lastSyncAtMs = tx.lastSyncAtMs()
        publishWorkspace()
        _outbox.value = outboxEntries.toList()
        for ((id, view) in open) {
            loadContent(tx, id, view)
            publish(id, view)
        }
    }

    /** Registers an open thread and loads its stored content; returns its flow. */
    suspend fun openThread(tx: SyncTx, id: ThreadId): StateFlow<ThreadState> {
        val view = open.getOrPut(id) { OpenThread(MutableStateFlow(ThreadState.empty(id))) }
        loadContent(tx, id, view)
        publish(id, view)
        return view.flow.asStateFlow()
    }

    fun closeThread(id: ThreadId) {
        open.remove(id)
    }

    fun thread(id: ThreadId): StateFlow<ThreadState>? = open[id]?.flow?.asStateFlow()

    fun isOpen(id: ThreadId): Boolean = open.containsKey(id)

    fun setSync(id: ThreadId, sync: ThreadSync) {
        val view = open[id] ?: return
        if (view.sync == ThreadSync.Removed || view.sync == sync) return
        view.sync = sync
        view.loadError = null
        publish(id, view)
    }

    /** Loading the thread failed: [ThreadSync.Failed] with [error] until the next load starts. */
    fun setLoadFailed(id: ThreadId, error: SyncError) {
        val view = open[id] ?: return
        if (view.sync == ThreadSync.Removed) return
        view.sync = ThreadSync.Failed
        view.loadError = error
        publish(id, view)
    }

    /** Every open thread that is not removed goes back to [ThreadSync.Cached] (connection lost). */
    fun allCached() {
        for ((id, view) in open) {
            if (view.sync != ThreadSync.Removed && view.sync != ThreadSync.Cached) {
                view.sync = ThreadSync.Cached
                view.loadError = null
                publish(id, view)
            }
        }
    }

    private suspend fun loadContent(tx: SyncTx, id: ThreadId, view: OpenThread) {
        view.turns.clear()
        tx.turnsOf(id).forEach { view.turns[it.id] = it }
        view.items.clear()
        tx.itemsOf(id).forEach { view.items[it.item.id] = it }
        view.interactions.clear()
        tx.interactionsOf(id).forEach { view.interactions[it.id] = it }
        view.tasks.clear()
        tx.backgroundTasksOf(id).forEach { view.tasks[it.id] = it }
        view.queued = tx.queued(id)
        view.meta = tx.threadMeta(id)
    }

    /** Applies the changes of one committed transaction. */
    fun apply(changes: List<StoreChange>) {
        if (changes.isEmpty()) return
        var workspaceDirty = false
        var outboxDirty = false
        for (change in changes) {
            when (change) {
                StoreChange.Wiped -> {
                    epoch = null
                    harnesses.clear()
                    projects.clear()
                    threads.clear()
                    viewStates.clear()
                    pending.clear()
                    operations.clear()
                    cursors.clear()
                    lastSyncAtMs = null
                    for (view in open.values) {
                        view.turns.clear()
                        view.items.clear()
                        view.interactions.clear()
                        view.tasks.clear()
                        view.queued = emptyList()
                        view.meta = ThreadMeta()
                        view.dirty = true
                    }
                    workspaceDirty = true
                }
                is StoreChange.EpochSet -> {
                    epoch = change.epoch
                    workspaceDirty = true
                }
                is StoreChange.CursorSet -> cursors[change.stream] = change.seq
                is StoreChange.LastSyncSet -> lastSyncAtMs = change.atMs
                is StoreChange.HarnessesReplaced -> {
                    harnesses.clear()
                    change.harnesses.forEach { harnesses[it.id] = it }
                    workspaceDirty = true
                }
                is StoreChange.HarnessUpserted -> {
                    harnesses[change.harness.id] = change.harness
                    workspaceDirty = true
                }
                is StoreChange.ProjectUpserted -> {
                    projects[change.project.id] = change.project
                    workspaceDirty = true
                }
                is StoreChange.ProjectRemoved -> {
                    projects.remove(change.id)
                    workspaceDirty = true
                }
                is StoreChange.ThreadUpserted -> {
                    threads[change.thread.id] = change.thread
                    open[change.thread.id]?.dirty = true
                    workspaceDirty = true
                }
                is StoreChange.ThreadRemoved -> {
                    threads.remove(change.id)
                    viewStates.remove(change.id)
                    pending.values.removeAll { it.threadId == change.id }
                    cursors.remove(threadStream(change.id))
                    open[change.id]?.let { view ->
                        view.turns.clear()
                        view.items.clear()
                        view.interactions.clear()
                        view.tasks.clear()
                        view.queued = emptyList()
                        view.meta = ThreadMeta()
                        view.sync = ThreadSync.Removed
                        view.loadError = null
                        view.dirty = true
                    }
                    workspaceDirty = true
                }
                is StoreChange.OperationUpserted -> {
                    operations[change.operation.id] = change.operation
                    workspaceDirty = true
                }
                is StoreChange.ViewStateSet -> {
                    viewStates[change.threadId] = change.state
                    workspaceDirty = true
                }
                is StoreChange.TurnUpserted -> open[change.turn.threadId]?.let {
                    it.turns[change.turn.id] = change.turn
                    it.dirty = true
                }
                is StoreChange.ItemUpserted -> open[change.item.item.threadId]?.let {
                    it.items[change.item.item.id] = change.item
                    it.dirty = true
                }
                is StoreChange.InteractionUpserted -> {
                    val i = change.interaction
                    if (i.status == InteractionStatus.Pending) pending[i.id] = i else pending.remove(i.id)
                    workspaceDirty = true
                    open[i.threadId]?.let {
                        it.interactions[i.id] = i
                        it.dirty = true
                    }
                }
                is StoreChange.BackgroundTaskUpserted -> open[change.task.threadId]?.let {
                    it.tasks[change.task.id] = change.task
                    it.dirty = true
                }
                is StoreChange.QueueReplaced -> open[change.threadId]?.let {
                    it.queued = change.queued
                    it.dirty = true
                }
                is StoreChange.ThreadContentCleared -> open[change.threadId]?.let {
                    it.turns.clear()
                    it.items.clear()
                    it.tasks.clear()
                    it.queued = emptyList()
                    it.dirty = true
                }
                is StoreChange.ThreadMetaSet -> open[change.threadId]?.let {
                    it.meta = change.meta
                    it.dirty = true
                }
                is StoreChange.OutboxAdded -> {
                    outboxEntries += change.entry
                    outboxDirty = true
                }
                is StoreChange.OutboxUpdated -> {
                    val i = outboxEntries.indexOfFirst { it.clientRequestId == change.entry.clientRequestId }
                    if (i >= 0) outboxEntries[i] = change.entry
                    outboxDirty = true
                }
                is StoreChange.OutboxRemoved -> {
                    outboxEntries.removeAll { it.clientRequestId == change.clientRequestId }
                    outboxDirty = true
                }
                StoreChange.OutboxCleared -> {
                    outboxEntries.clear()
                    outboxDirty = true
                }
            }
        }
        if (workspaceDirty) publishWorkspace()
        if (outboxDirty) {
            _outbox.value = outboxEntries.toList()
            open.values.forEach { it.dirty = true }
        }
        for ((id, view) in open) {
            if (view.dirty) publish(id, view)
        }
    }

    private fun publishWorkspace() {
        _workspace.value = WorkspaceState(
            synced = epoch != null,
            harnesses = harnesses.values.toList(),
            projects = projects.values.sortedWith(compareBy<Project, String>(String.CASE_INSENSITIVE_ORDER) { it.name }.thenBy { it.id }),
            threads = threads.values
                .sortedWith(compareByDescending<Thread> { it.lastActivityAt }.thenByDescending { it.id })
                .map { ThreadEntry(it, isUnread(it)) },
            pendingInteractions = pending.values.sortedWith(compareBy({ it.createdAt }, { it.id })),
            operations = operations.values.sortedWith(compareByDescending<Operation> { it.startedAt }.thenByDescending { it.id }),
        )
    }

    private fun isUnread(thread: Thread): Boolean {
        val state = viewStates[thread.id] ?: return true
        return state.markedUnread || thread.head > state.lastViewedHead
    }

    private fun publish(id: ThreadId, view: OpenThread) {
        view.dirty = false
        view.flow.value = ThreadState(
            threadId = id,
            sync = view.sync,
            thread = threads[id],
            turns = view.turns.values.sortedBy { it.index },
            items = view.items.values.sortedBy { it.position }.map { it.item },
            interactions = view.interactions.values.sortedWith(compareBy({ it.createdAt }, { it.id })),
            queued = view.queued,
            hasMoreBefore = view.meta.hasMoreBefore,
            commandsVersion = view.meta.commandsVersion,
            pending = outboxEntries.filter { it.threadId == id },
            loadError = view.loadError,
            backgroundTasks = view.tasks.values.sortedWith(compareBy({ it.startedAt }, { it.id })),
        )
    }
}
