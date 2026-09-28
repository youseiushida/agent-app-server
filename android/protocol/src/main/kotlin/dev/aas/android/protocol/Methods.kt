package dev.aas.android.protocol

import kotlinx.serialization.KSerializer
import kotlinx.serialization.Serializable

/** Static description of a method: name, whether it mutates, param and result types. */
class RpcMethod<P, R>(
    val name: String,
    /** Mutating methods carry `clientRequestId` and go through the outbox. */
    val mutating: Boolean,
    val params: KSerializer<P>,
    val result: KSerializer<R>,
)

/** `{}` params or result. */
@Serializable
data object Empty

// ----- connection & server -------------------------------------------------------------------

@Serializable
data class ClientInfo(val name: String, val version: String, val platform: String)

@Serializable
data class InitializeParams(val protocolVersion: Int, val client: ClientInfo, val lastKnownEpoch: String? = null)

@Serializable
data class ServerInfo(val name: String, val version: String, val hostname: String, val epoch: String)

@Serializable
data class DeviceInfo(val id: DeviceId, val name: String)

@Serializable
data class InitializeResult(
    val protocolVersion: Int,
    val server: ServerInfo,
    val device: DeviceInfo,
    val epochChanged: Boolean,
    val policy: ClientPolicy,
)

@Serializable
data class Subscription(val stream: String, val after: Long)

@Serializable
data class SubscribeParams(val subscriptions: List<Subscription>)

@Serializable
data class SubscriptionStatus(val stream: String, val head: Long, val status: SubscriptionState)

@Serializable
data class SubscribeResult(val subscriptions: List<SubscriptionStatus>)

@Serializable
data class UnsubscribeParams(val streams: List<String>)

@Serializable
data class WorkspaceSnapshotResult(
    val harnesses: List<Harness>,
    val projects: List<Project>,
    val threads: List<Thread>,
    val pendingInteractions: List<Interaction>,
    val operations: List<Operation>,
    val head: Long,
)

@Serializable
data class ServerStatusResult(
    val uptimeMs: Long,
    val runningProcesses: Int,
    val runningTurns: Int,
    val draining: Boolean,
    /**
     * The daemon keeps the PC awake while a turn runs, and while background work keeps an agent
     * busy (`policy.prevent_sleep_while_running`).
     */
    val preventSleepWhileRunning: Boolean = false,
    /** Background tasks that keep an agent's process alive, over all threads. */
    val runningBackgroundTasks: Int = 0,
)

@Serializable
data class DeviceListResult(val devices: List<Device>)

@Serializable
data class DeviceRevokeParams(val clientRequestId: String, val deviceId: DeviceId)

// ----- harnesses -----------------------------------------------------------------------------

@Serializable
data class HarnessListResult(val harnesses: List<Harness>)

@Serializable
data class HarnessRefreshParams(val harnessId: String? = null)

// ----- projects & filesystem -----------------------------------------------------------------

@Serializable
data class ProjectListParams(val includeArchived: Boolean = false)

@Serializable
data class ProjectListResult(val projects: List<Project>)

@Serializable
data class ProjectGetParams(val projectId: ProjectId)

@Serializable
data class ProjectResult(val project: Project)

@Serializable
data class ProjectCreateParams(val clientRequestId: String, val parentPath: String, val name: String, val init: ProjectInit)

@Serializable
data class ProjectCreateResult(val project: Project? = null, val operation: Operation? = null)

@Serializable
data class ProjectOpenParams(val clientRequestId: String, val path: String, val name: String? = null)

@Serializable
data class ProjectUpdateParams(
    val clientRequestId: String,
    val projectId: ProjectId,
    val name: String? = null,
    val defaults: ProjectDefaults? = null,
)

@Serializable
data class ProjectArchiveParams(val clientRequestId: String, val projectId: ProjectId, val archived: Boolean)

@Serializable
data class ProjectRemoveParams(val clientRequestId: String, val projectId: ProjectId)

@Serializable
data class FsRootsResult(val roots: List<FsRoot>)

@Serializable
data class FsListParams(val path: String, val includeFiles: Boolean = false)

@Serializable
data class FsListResult(val path: String, val entries: List<FsEntry>)

@Serializable
data class FsMkdirParams(val clientRequestId: String, val path: String)

@Serializable
data class FsMkdirResult(val path: String)

@Serializable
data class FsSearchParams(
    val projectId: ProjectId? = null,
    val threadId: ThreadId? = null,
    val query: String,
    val limit: Int? = null,
)

@Serializable
data class FsSearchResult(val results: List<SearchResult>, val ranking: String)

// ----- threads & turns -----------------------------------------------------------------------

@Serializable
data class ThreadCursor(val lastActivityAt: Millis, val id: ThreadId)

@Serializable
data class ThreadListParams(
    val projectId: ProjectId? = null,
    val includeArchived: Boolean = false,
    val limit: Int? = null,
    val before: ThreadCursor? = null,
)

@Serializable
data class ThreadListResult(val threads: List<Thread>, val hasMore: Boolean)

@Serializable
data class ThreadGetParams(val threadId: ThreadId)

@Serializable
data class ThreadResult(val thread: Thread)

@Serializable
data class ThreadCreateParams(
    val clientRequestId: String,
    val projectId: ProjectId,
    val harnessId: String,
    val settings: ThreadSettings? = null,
    val workspace: WorkspaceSpec? = null,
    val title: String? = null,
    val input: List<InputPart>? = null,
)

@Serializable
data class ThreadCreateResult(val thread: Thread, val turnId: TurnId? = null, val disposition: Disposition? = null)

@Serializable
data class ThreadReadParams(val threadId: ThreadId, val beforeTurnIndex: Int? = null, val limitTurns: Int? = null)

@Serializable
data class ThreadReadResult(
    val thread: Thread,
    val turns: List<Turn>,
    val items: List<Item>,
    val interactions: List<Interaction>,
    val queued: List<QueuedInput>,
    val head: Long,
    val hasMoreBefore: Boolean,
    /** The background tasks first reported during the returned turns, and every running task (oldest first). */
    val backgroundTasks: List<BackgroundTask> = emptyList(),
)

@Serializable
data class ThreadUpdateParams(
    val clientRequestId: String,
    val threadId: ThreadId,
    val title: String? = null,
    val settings: ThreadSettings? = null,
    /** Pins (`true`) or unpins (`false`) the thread. */
    val pinned: Boolean? = null,
)

@Serializable
data class ThreadUpdateResult(val thread: Thread, val settingsOutcome: SettingsOutcome? = null)

@Serializable
data class ThreadArchiveParams(
    val clientRequestId: String,
    val threadId: ThreadId,
    val archived: Boolean,
    val removeWorktree: Boolean = false,
    val force: Boolean = false,
)

@Serializable
data class ThreadForkParams(val clientRequestId: String, val threadId: ThreadId, val atTurnId: TurnId? = null)

@Serializable
data class ThreadStopParams(val clientRequestId: String, val threadId: ThreadId)

@Serializable
data class ThreadDiffParams(val threadId: ThreadId, val scope: DiffScope)

@Serializable
data class ThreadDiffResult(
    val summary: DiffSummary,
    val files: List<DiffFile>,
    val patch: String? = null,
    val patchBlobId: BlobId? = null,
)

@Serializable
data class TurnStartParams(
    val clientRequestId: String,
    val threadId: ThreadId,
    val input: List<InputPart>,
    val delivery: Delivery = Delivery.Auto,
)

@Serializable
data class TurnStartResult(val disposition: Disposition, val turnId: TurnId? = null, val queuedId: QueuedInputId? = null)

@Serializable
data class TurnInterruptParams(val clientRequestId: String, val threadId: ThreadId)

@Serializable
data class TurnInterruptResult(val interrupted: Boolean)

@Serializable
data class QueueRemoveParams(val clientRequestId: String, val threadId: ThreadId, val queuedId: QueuedInputId)

@Serializable
data class QueueRemoveResult(val removed: Boolean)

@Serializable
data class QueueResumeParams(val clientRequestId: String, val threadId: ThreadId)

@Serializable
data class QueueResumeResult(val turnId: TurnId? = null)

/** Replaces a queued input in place (validated like `turn/start` input). */
@Serializable
data class QueueUpdateParams(
    val clientRequestId: String,
    val threadId: ThreadId,
    val queuedId: QueuedInputId,
    val input: List<InputPart>,
)

/** `updated` is `false` when the entry had already left the queue. */
@Serializable
data class QueueUpdateResult(val updated: Boolean)

/** Sends a queued input now: steered into the running turn, or starting a new one. */
@Serializable
data class QueueSteerParams(val clientRequestId: String, val threadId: ThreadId, val queuedId: QueuedInputId)

/** `disposition` is absent when the entry had already left the queue. */
@Serializable
data class QueueSteerResult(val disposition: Disposition? = null, val turnId: TurnId? = null)

// ----- interactions, commands, native sessions, operations -----------------------------------

@Serializable
data class InteractionRespondParams(
    val clientRequestId: String,
    val interactionId: InteractionId,
    val resolution: InteractionResolution,
)

@Serializable
data class InteractionRespondResult(val interaction: Interaction, val alreadyResolved: Boolean)

@Serializable
data class InteractionListParams(val status: InteractionStatus? = null)

@Serializable
data class InteractionListResult(val interactions: List<Interaction>)

@Serializable
data class CommandListParams(val threadId: ThreadId? = null, val projectId: ProjectId? = null, val harnessId: String? = null)

@Serializable
data class CommandListResult(val commands: List<Command>)

@Serializable
data class NativeListParams(val projectId: ProjectId, val harnessId: String)

@Serializable
data class NativeListResult(val sessions: List<NativeSession>)

@Serializable
data class NativeImportParams(
    val clientRequestId: String,
    val projectId: ProjectId,
    val harnessId: String,
    val nativeSessionId: String,
)

@Serializable
data class OperationListResult(val operations: List<Operation>)

@Serializable
data class OperationCancelParams(val clientRequestId: String, val operationId: OperationId)

@Serializable
data class OperationResult(val operation: Operation)

// ----- background tasks ------------------------------------------------------------------------

/** Asks the harness to stop one background task (protocol.md §4 `backgroundTask/stop`). */
@Serializable
data class BackgroundTaskStopParams(val clientRequestId: String, val threadId: ThreadId, val taskId: BackgroundTaskId)

/**
 * The task after the stop was requested (`stopRequestedAt` set). It did not stop yet: the end
 * arrives with `backgroundTask/updated`.
 */
@Serializable
data class BackgroundTaskResult(val task: BackgroundTask)

/** Every method of protocol v1, typed. */
object Methods {
    val Initialize = RpcMethod("initialize", false, InitializeParams.serializer(), InitializeResult.serializer())
    val Subscribe = RpcMethod("subscribe", false, SubscribeParams.serializer(), SubscribeResult.serializer())
    val Unsubscribe = RpcMethod("unsubscribe", false, UnsubscribeParams.serializer(), Empty.serializer())
    val WorkspaceSnapshot = RpcMethod("workspace/snapshot", false, Empty.serializer(), WorkspaceSnapshotResult.serializer())
    val ServerStatus = RpcMethod("server/status", false, Empty.serializer(), ServerStatusResult.serializer())
    val DeviceList = RpcMethod("device/list", false, Empty.serializer(), DeviceListResult.serializer())
    val DeviceRevoke = RpcMethod("device/revoke", true, DeviceRevokeParams.serializer(), Empty.serializer())
    val HarnessList = RpcMethod("harness/list", false, Empty.serializer(), HarnessListResult.serializer())
    val HarnessRefresh = RpcMethod("harness/refresh", false, HarnessRefreshParams.serializer(), HarnessListResult.serializer())
    val ProjectList = RpcMethod("project/list", false, ProjectListParams.serializer(), ProjectListResult.serializer())
    val ProjectGet = RpcMethod("project/get", false, ProjectGetParams.serializer(), ProjectResult.serializer())
    val ProjectCreate = RpcMethod("project/create", true, ProjectCreateParams.serializer(), ProjectCreateResult.serializer())
    val ProjectOpen = RpcMethod("project/open", true, ProjectOpenParams.serializer(), ProjectResult.serializer())
    val ProjectUpdate = RpcMethod("project/update", true, ProjectUpdateParams.serializer(), ProjectResult.serializer())
    val ProjectArchive = RpcMethod("project/archive", true, ProjectArchiveParams.serializer(), ProjectResult.serializer())
    val ProjectRemove = RpcMethod("project/remove", true, ProjectRemoveParams.serializer(), Empty.serializer())
    val FsRoots = RpcMethod("fs/roots", false, Empty.serializer(), FsRootsResult.serializer())
    val FsList = RpcMethod("fs/list", false, FsListParams.serializer(), FsListResult.serializer())
    val FsMkdir = RpcMethod("fs/mkdir", true, FsMkdirParams.serializer(), FsMkdirResult.serializer())
    val FsSearch = RpcMethod("fs/search", false, FsSearchParams.serializer(), FsSearchResult.serializer())
    val ThreadList = RpcMethod("thread/list", false, ThreadListParams.serializer(), ThreadListResult.serializer())
    val ThreadGet = RpcMethod("thread/get", false, ThreadGetParams.serializer(), ThreadResult.serializer())
    val ThreadCreate = RpcMethod("thread/create", true, ThreadCreateParams.serializer(), ThreadCreateResult.serializer())
    val ThreadRead = RpcMethod("thread/read", false, ThreadReadParams.serializer(), ThreadReadResult.serializer())
    val ThreadUpdate = RpcMethod("thread/update", true, ThreadUpdateParams.serializer(), ThreadUpdateResult.serializer())
    val ThreadArchive = RpcMethod("thread/archive", true, ThreadArchiveParams.serializer(), ThreadResult.serializer())
    val ThreadFork = RpcMethod("thread/fork", true, ThreadForkParams.serializer(), ThreadResult.serializer())
    val ThreadStop = RpcMethod("thread/stop", true, ThreadStopParams.serializer(), ThreadResult.serializer())
    val ThreadDiff = RpcMethod("thread/diff", false, ThreadDiffParams.serializer(), ThreadDiffResult.serializer())
    val TurnStart = RpcMethod("turn/start", true, TurnStartParams.serializer(), TurnStartResult.serializer())
    val TurnInterrupt = RpcMethod("turn/interrupt", true, TurnInterruptParams.serializer(), TurnInterruptResult.serializer())
    val QueueRemove = RpcMethod("queue/remove", true, QueueRemoveParams.serializer(), QueueRemoveResult.serializer())
    val QueueResume = RpcMethod("queue/resume", true, QueueResumeParams.serializer(), QueueResumeResult.serializer())
    val QueueUpdate = RpcMethod("queue/update", true, QueueUpdateParams.serializer(), QueueUpdateResult.serializer())
    val QueueSteer = RpcMethod("queue/steer", true, QueueSteerParams.serializer(), QueueSteerResult.serializer())
    val InteractionRespond =
        RpcMethod("interaction/respond", true, InteractionRespondParams.serializer(), InteractionRespondResult.serializer())
    val InteractionList = RpcMethod("interaction/list", false, InteractionListParams.serializer(), InteractionListResult.serializer())
    val CommandList = RpcMethod("command/list", false, CommandListParams.serializer(), CommandListResult.serializer())
    val NativeList = RpcMethod("native/list", false, NativeListParams.serializer(), NativeListResult.serializer())
    val NativeImport = RpcMethod("native/import", true, NativeImportParams.serializer(), ThreadResult.serializer())
    val OperationList = RpcMethod("operation/list", false, Empty.serializer(), OperationListResult.serializer())
    val OperationCancel = RpcMethod("operation/cancel", true, OperationCancelParams.serializer(), OperationResult.serializer())
    val BackgroundTaskStop =
        RpcMethod("backgroundTask/stop", true, BackgroundTaskStopParams.serializer(), BackgroundTaskResult.serializer())

    val all: List<RpcMethod<*, *>> = listOf(
        Initialize, Subscribe, Unsubscribe, WorkspaceSnapshot, ServerStatus, DeviceList, DeviceRevoke, HarnessList,
        HarnessRefresh, ProjectList, ProjectGet, ProjectCreate, ProjectOpen, ProjectUpdate, ProjectArchive, ProjectRemove,
        FsRoots, FsList, FsMkdir, FsSearch, ThreadList, ThreadGet, ThreadCreate, ThreadRead, ThreadUpdate, ThreadArchive,
        ThreadFork, ThreadStop, ThreadDiff, TurnStart, TurnInterrupt, QueueRemove, QueueResume, QueueUpdate, QueueSteer,
        InteractionRespond, InteractionList, CommandList, NativeList, NativeImport, OperationList, OperationCancel,
        BackgroundTaskStop,
    )

    private val byName = all.associateBy { it.name }

    fun byName(name: String): RpcMethod<*, *>? = byName[name]
}
