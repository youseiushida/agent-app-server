package dev.aas.android.data

import dev.aas.android.domain.InboxModel
import dev.aas.android.protocol.BackgroundTask
import dev.aas.android.protocol.BackgroundTaskId
import dev.aas.android.protocol.BackgroundTaskStopParams
import dev.aas.android.protocol.Command
import dev.aas.android.protocol.CommandAction
import dev.aas.android.protocol.CommandListParams
import dev.aas.android.protocol.Delivery
import dev.aas.android.protocol.Device
import dev.aas.android.protocol.DeviceId
import dev.aas.android.protocol.DeviceRevokeParams
import dev.aas.android.protocol.DiffScope
import dev.aas.android.protocol.Empty
import dev.aas.android.protocol.FsEntry
import dev.aas.android.protocol.FsListParams
import dev.aas.android.protocol.FsListResult
import dev.aas.android.protocol.FsMkdirParams
import dev.aas.android.protocol.FsMkdirResult
import dev.aas.android.protocol.FsRoot
import dev.aas.android.protocol.FsSearchParams
import dev.aas.android.protocol.Harness
import dev.aas.android.protocol.InputPart
import dev.aas.android.protocol.Interaction
import dev.aas.android.protocol.InteractionResolution
import dev.aas.android.protocol.InteractionRespondParams
import dev.aas.android.protocol.Methods
import dev.aas.android.protocol.NativeImportParams
import dev.aas.android.protocol.NativeListParams
import dev.aas.android.protocol.NativeSession
import dev.aas.android.protocol.OperationCancelParams
import dev.aas.android.protocol.OperationId
import dev.aas.android.protocol.ProjectArchiveParams
import dev.aas.android.protocol.ProjectCreateParams
import dev.aas.android.protocol.ProjectCreateResult
import dev.aas.android.protocol.ProjectDefaults
import dev.aas.android.protocol.ProjectId
import dev.aas.android.protocol.ProjectInit
import dev.aas.android.protocol.ProjectOpenParams
import dev.aas.android.protocol.ProjectRemoveParams
import dev.aas.android.protocol.ProjectResult
import dev.aas.android.protocol.ProjectUpdateParams
import dev.aas.android.protocol.QueueRemoveParams
import dev.aas.android.protocol.QueueResumeParams
import dev.aas.android.protocol.QueueSteerParams
import dev.aas.android.protocol.QueueUpdateParams
import dev.aas.android.protocol.QueuedInputId
import dev.aas.android.protocol.SearchResult
import dev.aas.android.protocol.ServerStatusResult
import dev.aas.android.protocol.Thread
import dev.aas.android.protocol.ThreadArchiveParams
import dev.aas.android.protocol.ThreadCreateParams
import dev.aas.android.protocol.ThreadCreateResult
import dev.aas.android.protocol.ThreadCursor
import dev.aas.android.protocol.ThreadDiffParams
import dev.aas.android.protocol.ThreadDiffResult
import dev.aas.android.protocol.ThreadForkParams
import dev.aas.android.protocol.ThreadId
import dev.aas.android.protocol.ThreadListParams
import dev.aas.android.protocol.ThreadListResult
import dev.aas.android.protocol.ThreadResult
import dev.aas.android.protocol.ThreadSettings
import dev.aas.android.protocol.ThreadStopParams
import dev.aas.android.protocol.ThreadUpdateParams
import dev.aas.android.protocol.ThreadUpdateResult
import dev.aas.android.protocol.TurnInterruptParams
import dev.aas.android.protocol.TurnStartParams
import dev.aas.android.protocol.TurnStartResult
import dev.aas.android.protocol.WorkspaceSpec
import dev.aas.android.sync.OutboxDiscard
import dev.aas.android.sync.PendingMutation
import dev.aas.android.sync.SyncEngine
import dev.aas.android.sync.SyncSignal
import dev.aas.android.sync.ThreadState
import dev.aas.android.sync.WorkspaceState
import kotlinx.coroutines.NonCancellable
import kotlinx.coroutines.flow.Flow
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.flow.combine
import kotlinx.coroutines.flow.distinctUntilChanged
import kotlinx.coroutines.flow.emitAll
import kotlinx.coroutines.flow.filterIsInstance
import kotlinx.coroutines.flow.flow
import kotlinx.coroutines.flow.map
import kotlinx.coroutines.withContext
import kotlinx.serialization.json.JsonElement

/*
 * Repositories: the only way view models reach the sync engine.
 *
 * Reading: the engine's StateFlows (committed store content mirrored in memory). The UI never
 * reads Room directly.
 * Changing state: `engine.enqueue` (fire and forget; the result arrives on `engine.results`,
 * shown by the app shell) or `engine.mutate` (awaits the result). Both commit the request to the
 * durable outbox first, so it is sent even if the app is offline or killed.
 * Read-only calls: `engine.query`, which throws `NotConnectedException` when offline.
 */

/**
 * The harnesses the daemon offers and `harness/refresh` (probe again, e.g. after logging in to a
 * CLI on the PC). The list itself follows the workspace stream (`harness/updated`).
 */
class HarnessRepository(private val engine: SyncEngine) {
    val harnesses: Flow<List<Harness>> = engine.workspace.map { it.harnesses }.distinctUntilChanged()

    /** Harnesses this app is probing right now (its own refresh in flight). */
    val refreshing: StateFlow<Set<String>> get() = engine.refreshingHarnesses

    /**
     * Probes [harnessId] (every harness when `null`) again and returns the result.
     *
     * @throws NotConnectedException offline.
     */
    suspend fun refresh(harnessId: String? = null): List<Harness> = engine.refreshHarnesses(harnessId)
}

/** Workspace-level state: projects, threads, pending interactions, the 要対応 inbox. */
class WorkspaceRepository(private val engine: SyncEngine) {
    val workspace: StateFlow<WorkspaceState> get() = engine.workspace

    /** The 要対応 inbox, recomputed when the workspace or the outbox changes. */
    val inbox: Flow<InboxModel> = combine(engine.workspace, engine.outbox) { workspace, outbox -> InboxModel.build(workspace, outbox) }

    suspend fun markViewed(threadId: ThreadId) = engine.markViewed(threadId)

    suspend fun markUnread(threadId: ThreadId) = engine.markUnread(threadId)

    /**
     * Ticks when the background task of a pending interaction became known after the interaction
     * (`SyncSignal.InteractionTaskKnown`): lists that name the task look it up again.
     */
    val interactionTasksKnown: Flow<Unit> = engine.signals.filterIsInstance<SyncSignal.InteractionTaskKnown>().map { }

    /** The background tasks stored on this device among [ids] (see `SyncEngine.storedBackgroundTasks`). */
    suspend fun backgroundTasks(ids: Collection<BackgroundTaskId>): Map<BackgroundTaskId, BackgroundTask> = engine.storedBackgroundTasks(ids)

    /** Drops a request from the outbox without an answer (a request on the wire stays). */
    suspend fun discard(clientRequestId: String): OutboxDiscard = engine.discardOutbox(clientRequestId)
}

/**
 * Projects, folders on the PC and operations (clone). Reads go through [reads] (sent again after
 * a reconnect); lists the screens key by id pass [lists] (each id once).
 */
class ProjectRepository(private val engine: SyncEngine, private val reads: Reads, private val lists: ServerLists) {
    /** `fs/roots`: the folders projects may live in (each path once). */
    suspend fun roots(): List<FsRoot> = lists.unique(reads.query(Methods.FsRoots, Empty).roots, "fs/roots", FsRoot::path)

    /** `fs/list`: the sub-folders of [path] (directories only, each path once). */
    suspend fun list(path: String): FsListResult {
        val result = reads.query(Methods.FsList, FsListParams(path))
        return result.copy(entries = lists.unique(result.entries, "fs/list", FsEntry::path))
    }

    /*
     * The new-project flow's changes are committed to the outbox and their answer is awaited on
     * the returned handle, whose `clientRequestId` lets the flow take a waiting request back
     * ([discard]); offline or while the daemon cannot answer, they wait in the outbox.
     */

    /** `fs/mkdir` (the flow then browses the new folder: the answer's `path`). */
    suspend fun mkdir(path: String): PendingMutation<FsMkdirResult> = engine.submit(Methods.FsMkdir) { crid -> FsMkdirParams(crid, path) }

    /** `project/create`: the answer holds the project, or the clone operation to follow. */
    suspend fun create(parentPath: String, name: String, init: ProjectInit): PendingMutation<ProjectCreateResult> =
        engine.submit(Methods.ProjectCreate) { crid -> ProjectCreateParams(crid, parentPath, name, init) }

    /** `project/open`: registers an existing folder (the same project when it was registered before). */
    suspend fun open(path: String, name: String? = null): PendingMutation<ProjectResult> =
        engine.submit(Methods.ProjectOpen) { crid -> ProjectOpenParams(crid, path, name) }

    /** Takes back a request of the flow that waits in the outbox (never sent; its waiter fails with `OutboxClearedException`). */
    suspend fun discard(clientRequestId: String): OutboxDiscard = engine.discardOutbox(clientRequestId)

    suspend fun rename(projectId: ProjectId, name: String) {
        engine.enqueue(Methods.ProjectUpdate) { crid -> ProjectUpdateParams(crid, projectId, name = name) }
    }

    /**
     * The settings a new thread starts with ("前回値を引き継ぐ", UX §8.2). Committed to the
     * outbox at once (the project's lane keeps it after a `thread/create` committed before it).
     */
    suspend fun setDefaults(projectId: ProjectId, defaults: ProjectDefaults) {
        engine.enqueue(Methods.ProjectUpdate) { crid -> ProjectUpdateParams(crid, projectId, defaults = defaults) }
    }

    suspend fun archive(projectId: ProjectId, archived: Boolean) {
        engine.enqueue(Methods.ProjectArchive) { crid -> ProjectArchiveParams(crid, projectId, archived) }
    }

    /** Unregisters the project (its files stay on the PC). */
    suspend fun remove(projectId: ProjectId) {
        engine.enqueue(Methods.ProjectRemove) { crid -> ProjectRemoveParams(crid, projectId) }
    }

    /** `operation/cancel` (a running clone). */
    suspend fun cancelOperation(operationId: OperationId) {
        engine.enqueue(Methods.OperationCancel) { crid -> OperationCancelParams(crid, operationId) }
    }

    /**
     * `thread/list` of one project (the archived ones are only here, not in the workspace): one
     * page as the server sent it (its last thread is the cursor of the next page). Pages are
     * joined with [joinThreadPages].
     */
    suspend fun threads(projectId: ProjectId, includeArchived: Boolean, before: ThreadCursor? = null): ThreadListResult =
        reads.query(Methods.ThreadList, ThreadListParams(projectId = projectId, includeArchived = includeArchived, before = before))

    /**
     * [shown] followed by the threads of the next [page], each thread once: a thread whose
     * activity moved it between pages while they were read comes in both. The newer summary
     * (the larger `head`, protocol.md §3.1) is kept, at the thread's first position.
     */
    fun joinThreadPages(shown: List<Thread>, page: List<Thread>): List<Thread> =
        lists.unique(shown + page, "thread/list", Thread::id, compareBy(Thread::head))

    /**
     * `native/list`: the harness's own sessions in the project's folder, each session once. A
     * harness may list a session several times (Codex: once per rollout of a thread resumed
     * elsewhere, same id, different `updatedAt`); the latest is kept.
     */
    suspend fun nativeSessions(projectId: ProjectId, harnessId: String): List<NativeSession> = lists.unique(
        reads.query(Methods.NativeList, NativeListParams(projectId, harnessId)).sessions,
        "native/list of $harnessId",
        NativeSession::nativeSessionId,
        compareBy(nullsFirst()) { it.updatedAt },
    )

    /**
     * `native/import`: the thread continuing a native session. Committed to the outbox; the
     * answer (the thread) is awaited on the returned handle.
     */
    suspend fun importSession(projectId: ProjectId, harnessId: String, nativeSessionId: String): PendingMutation<ThreadResult> =
        engine.submit(Methods.NativeImport) { crid -> NativeImportParams(crid, projectId, harnessId, nativeSessionId) }

    /** `command/list` for a new thread of [harnessId] in [projectId]. */
    suspend fun commands(projectId: ProjectId, harnessId: String): List<Command> =
        reads.query(Methods.CommandList, CommandListParams(projectId = projectId, harnessId = harnessId)).commands

    /** `fs/search` in the project's folder (mentions in the new-thread composer; each path once). */
    suspend fun search(projectId: ProjectId, query: String): List<SearchResult> =
        lists.unique(reads.query(Methods.FsSearch, FsSearchParams(projectId = projectId, query = query)).results, "fs/search", SearchResult::path)
}

/**
 * An open thread and everything done to it. Reads go through [reads] (sent again after a
 * reconnect); lists the screens key by id pass [lists] (each id once).
 */
class ThreadRepository(private val engine: SyncEngine, private val reads: Reads, private val lists: ServerLists) {
    /**
     * The thread's state while collected: opens it (stored content at once, then `thread/read`
     * and a live subscription when online) and closes it when the collection ends. Use it with
     * `stateIn(viewModelScope, WhileSubscribed(uiStopTimeoutMs), ThreadState.empty(id))` so a
     * rotation does not resubscribe.
     */
    fun observe(threadId: ThreadId): Flow<ThreadState> = flow {
        val state = engine.openThread(threadId)
        try {
            emitAll(state)
        } finally {
            withContext(NonCancellable) { engine.closeThread(threadId) }
        }
    }

    /** Loads the page before the oldest loaded turn; returns whether older turns remain. */
    suspend fun loadOlder(threadId: ThreadId): Boolean = engine.loadOlder(threadId)

    /** Loads an open thread again after it failed to load (`ThreadSync.Failed`). */
    fun retryLoad(threadId: ThreadId) = engine.retryThread(threadId)

    /** Takes back a request of the thread that waits in the outbox (e.g. a message that keeps failing). */
    suspend fun discardPending(clientRequestId: String): OutboxDiscard = engine.discardOutbox(clientRequestId)

    suspend fun markViewed(threadId: ThreadId) = engine.markViewed(threadId)

    suspend fun markUnread(threadId: ThreadId) = engine.markUnread(threadId)

    /**
     * `thread/create`; with [input] the first turn starts at once. Returns once the request is in
     * the outbox; [PendingMutation.await] waits for the daemon (the new id).
     */
    suspend fun create(
        projectId: ProjectId,
        harnessId: String,
        settings: ThreadSettings?,
        workspace: WorkspaceSpec?,
        input: List<InputPart>?,
    ): PendingMutation<ThreadCreateResult> = engine.submit(Methods.ThreadCreate) { crid ->
        ThreadCreateParams(crid, projectId, harnessId, settings = settings, workspace = workspace, input = input)
    }

    /**
     * `turn/start`, committed to the outbox (it shows there until answered). The handle has the
     * daemon's final answer (a refusal gives the draft back: [SentDrafts]).
     */
    suspend fun send(threadId: ThreadId, input: List<InputPart>, delivery: Delivery): PendingMutation<TurnStartResult> =
        engine.submit(Methods.TurnStart) { crid -> TurnStartParams(crid, threadId, input, delivery) }

    suspend fun interrupt(threadId: ThreadId) {
        engine.enqueue(Methods.TurnInterrupt) { crid -> TurnInterruptParams(crid, threadId) }
    }

    /** `thread/stop`: stops the agent process tree (the next message resumes). */
    suspend fun stop(threadId: ThreadId) {
        engine.enqueue(Methods.ThreadStop) { crid -> ThreadStopParams(crid, threadId) }
    }

    /**
     * `backgroundTask/stop`: asks the harness to stop one background task. The answer only says
     * the request was taken (`stopRequestedAt`); the end arrives with `backgroundTask/updated`.
     */
    suspend fun stopBackgroundTask(threadId: ThreadId, taskId: BackgroundTaskId) {
        engine.enqueue(Methods.BackgroundTaskStop) { crid -> BackgroundTaskStopParams(crid, threadId, taskId) }
    }

    suspend fun rename(threadId: ThreadId, title: String) {
        engine.enqueue(Methods.ThreadUpdate) { crid -> ThreadUpdateParams(crid, threadId, title = title) }
    }

    suspend fun setPinned(threadId: ThreadId, pinned: Boolean) {
        engine.enqueue(Methods.ThreadUpdate) { crid -> ThreadUpdateParams(crid, threadId, pinned = pinned) }
    }

    /** Model / effort / permission mode; waits for `settingsOutcome` (applied now or next turn). */
    suspend fun updateSettings(threadId: ThreadId, settings: ThreadSettings): ThreadUpdateResult =
        engine.mutate(Methods.ThreadUpdate) { crid -> ThreadUpdateParams(crid, threadId, settings = settings) }

    suspend fun archive(threadId: ThreadId, archived: Boolean) {
        engine.enqueue(Methods.ThreadArchive) { crid -> ThreadArchiveParams(crid, threadId, archived) }
    }

    /** `thread/fork`; waits for the new thread. */
    suspend fun fork(threadId: ThreadId): Thread = engine.mutate(Methods.ThreadFork) { crid -> ThreadForkParams(crid, threadId) }.thread

    suspend fun removeQueued(threadId: ThreadId, queuedId: QueuedInputId) {
        engine.enqueue(Methods.QueueRemove) { crid -> QueueRemoveParams(crid, threadId, queuedId) }
    }

    suspend fun resumeQueue(threadId: ThreadId) {
        engine.enqueue(Methods.QueueResume) { crid -> QueueResumeParams(crid, threadId) }
    }

    suspend fun updateQueued(threadId: ThreadId, queuedId: QueuedInputId, input: List<InputPart>) {
        engine.enqueue(Methods.QueueUpdate) { crid -> QueueUpdateParams(crid, threadId, queuedId, input) }
    }

    /** "今すぐ反映": steered into the running turn, or a new turn when none runs. */
    suspend fun steerQueued(threadId: ThreadId, queuedId: QueuedInputId) {
        engine.enqueue(Methods.QueueSteer) { crid -> QueueSteerParams(crid, threadId, queuedId) }
    }

    /** `thread/diff` (read-only; needs a connection). */
    suspend fun diff(threadId: ThreadId, scope: DiffScope): ThreadDiffResult = reads.query(Methods.ThreadDiff, ThreadDiffParams(threadId, scope))

    /** `command/list` of the thread (app commands, then the harness's). */
    suspend fun commands(threadId: ThreadId): List<Command> = reads.query(Methods.CommandList, CommandListParams(threadId = threadId)).commands

    /** `fs/search` in the thread's working folder (its worktree, if any; each path once). */
    suspend fun search(threadId: ThreadId, query: String): List<SearchResult> =
        lists.unique(reads.query(Methods.FsSearch, FsSearchParams(threadId = threadId, query = query)).results, "fs/search", SearchResult::path)

    /** A command's `method` action for a method this app has no dedicated call for. */
    suspend fun runCommand(action: CommandAction.Method, threadId: ThreadId): JsonElement = engine.runCommand(action, threadId)
}

/** Answers approvals and questions (in the app; notification actions use InteractionResponder). */
class InteractionRepository(private val engine: SyncEngine) {
    /** Queues the answer; returns its `clientRequestId`. The outcome arrives on `engine.results`. */
    suspend fun respond(interaction: Interaction, resolution: InteractionResolution): String =
        engine.enqueue(Methods.InteractionRespond) { crid -> InteractionRespondParams(crid, interaction.id, resolution) }
}

/**
 * Server information and devices (設定). Read-only calls need a connection; they go through
 * [reads], and the device list passes [lists] (each id once).
 */
class ServerRepository(private val engine: SyncEngine, private val reads: Reads, private val lists: ServerLists) {
    suspend fun status(): ServerStatusResult = reads.query(Methods.ServerStatus, Empty)

    suspend fun devices(): List<Device> = lists.unique(reads.query(Methods.DeviceList, Empty).devices, "device/list", Device::id)

    /** Revokes another device; waits for the server's answer. */
    suspend fun revoke(deviceId: DeviceId) {
        engine.mutate(Methods.DeviceRevoke) { crid -> DeviceRevokeParams(crid, deviceId) }
    }
}
