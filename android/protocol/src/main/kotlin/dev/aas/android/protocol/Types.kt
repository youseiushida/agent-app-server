package dev.aas.android.protocol

import kotlinx.serialization.KSerializer
import kotlinx.serialization.Serializable
import kotlinx.serialization.json.JsonElement
import kotlinx.serialization.json.JsonObject

// Identifiers are opaque strings (`prj_…`, `thr_…`, `trn_…`, `itm_…`, `int_…`, `que_…`, `op_…`,
// `dev_…`, `blb_…`). Clients never interpret them.
typealias ProjectId = String
typealias ThreadId = String
typealias TurnId = String
typealias ItemId = String
typealias InteractionId = String
typealias QueuedInputId = String
typealias OperationId = String
typealias DeviceId = String
typealias BlobId = String

/** A background task (`bgt_…`): work the harness runs outside the turn lifecycle. */
typealias BackgroundTaskId = String

/** Unix epoch milliseconds. */
typealias Millis = Long

// ----- harnesses -------------------------------------------------------------------------------

@Serializable
data class Harness(
    val id: String,
    val kind: HarnessKind,
    val displayName: String,
    val available: Boolean,
    val unavailableReason: String? = null,
    val version: String? = null,
    val executable: String? = null,
    val capabilities: HarnessCapabilities = HarnessCapabilities(),
    val models: List<Model> = emptyList(),
    val defaultModel: String? = null,
    val effortLevels: List<EffortLevel> = emptyList(),
    val permissionModes: List<PermissionMode> = emptyList(),
    val defaultPermissionMode: String? = null,
)

@Serializable
data class HarnessCapabilities(
    val interrupt: Boolean = false,
    val steer: Boolean = false,
    val approvals: Boolean = false,
    val questions: Boolean = false,
    val resume: Boolean = false,
    val fork: Boolean = false,
    val images: Boolean = false,
    val modelSwitchLive: Boolean = false,
    val nativeSessions: Boolean = false,
    /** The harness reports work that runs outside the turn lifecycle as background tasks. */
    val backgroundTasks: Boolean = false,
    /** A single background task can be stopped (`backgroundTask/stop`). */
    val backgroundStop: Boolean = false,
)

@Serializable
data class Model(
    val id: String,
    val displayName: String,
    val description: String? = null,
    val isDefault: Boolean = false,
    val effortLevels: List<String>? = null,
)

@Serializable
data class EffortLevel(val id: String, val label: String)

@Serializable
data class PermissionMode(
    val id: String,
    val label: String,
    val description: String? = null,
    val isDefault: Boolean = false,
)

// ----- projects --------------------------------------------------------------------------------

@Serializable
data class Project(
    val id: ProjectId,
    val name: String,
    val path: String,
    val createdAt: Millis,
    val updatedAt: Millis,
    val archived: Boolean = false,
    val defaults: ProjectDefaults = ProjectDefaults(),
    val git: GitInfo = GitInfo(),
)

@Serializable
data class ProjectDefaults(
    val harnessId: String? = null,
    val model: String? = null,
    val effort: String? = null,
    val permissionMode: String? = null,
)

@Serializable
data class GitInfo(
    val isRepo: Boolean = false,
    val branch: String? = null,
    val root: String? = null,
)

/** How a new project folder is initialised. */
@Serializable(with = ProjectInit.Serializer::class)
sealed interface ProjectInit {
    @Serializable
    data object Empty : ProjectInit

    @Serializable
    data object GitInit : ProjectInit

    @Serializable
    data class GitClone(val url: String) : ProjectInit

    data class Unknown(val kind: String, val raw: JsonObject) : ProjectInit

    object Serializer : TaggedUnionSerializer<ProjectInit>("ProjectInit", "kind") {
        override fun tagOf(value: ProjectInit) = when (value) {
            Empty -> "empty"
            GitInit -> "gitInit"
            is GitClone -> "gitClone"
            is Unknown -> null
        }

        override fun serializerFor(tag: String): KSerializer<out ProjectInit>? = when (tag) {
            "empty" -> Empty.serializer()
            "gitInit" -> GitInit.serializer()
            "gitClone" -> GitClone.serializer()
            else -> null
        }

        override fun unknown(tag: String, raw: JsonObject) = Unknown(tag, raw)
        override fun rawOf(value: ProjectInit) = (value as? Unknown)?.raw
    }
}

// ----- threads, turns --------------------------------------------------------------------------

@Serializable
data class ThreadSettings(
    val model: String? = null,
    val effort: String? = null,
    val permissionMode: String? = null,
)

@Serializable(with = Workspace.Serializer::class)
sealed interface Workspace {
    @Serializable
    data object Local : Workspace

    @Serializable
    data class Worktree(val path: String, val branch: String, val baseRef: String) : Workspace

    data class Unknown(val kind: String, val raw: JsonObject) : Workspace

    object Serializer : TaggedUnionSerializer<Workspace>("Workspace", "kind") {
        override fun tagOf(value: Workspace) = when (value) {
            Local -> "local"
            is Worktree -> "worktree"
            is Unknown -> null
        }

        override fun serializerFor(tag: String): KSerializer<out Workspace>? = when (tag) {
            "local" -> Local.serializer()
            "worktree" -> Worktree.serializer()
            else -> null
        }

        override fun unknown(tag: String, raw: JsonObject) = Unknown(tag, raw)
        override fun rawOf(value: Workspace) = (value as? Unknown)?.raw
    }
}

/** Requested workspace for `thread/create`. */
@Serializable(with = WorkspaceSpec.Serializer::class)
sealed interface WorkspaceSpec {
    @Serializable
    data object Local : WorkspaceSpec

    @Serializable
    data class Worktree(val baseRef: String? = null, val branch: String? = null) : WorkspaceSpec

    data class Unknown(val kind: String, val raw: JsonObject) : WorkspaceSpec

    object Serializer : TaggedUnionSerializer<WorkspaceSpec>("WorkspaceSpec", "kind") {
        override fun tagOf(value: WorkspaceSpec) = when (value) {
            Local -> "local"
            is Worktree -> "worktree"
            is Unknown -> null
        }

        override fun serializerFor(tag: String): KSerializer<out WorkspaceSpec>? = when (tag) {
            "local" -> Local.serializer()
            "worktree" -> Worktree.serializer()
            else -> null
        }

        override fun unknown(tag: String, raw: JsonObject) = Unknown(tag, raw)
        override fun rawOf(value: WorkspaceSpec) = (value as? Unknown)?.raw
    }
}

@Serializable
data class Thread(
    val id: ThreadId,
    val projectId: ProjectId,
    val harnessId: String,
    val title: String,
    val cwd: String,
    val workspace: Workspace,
    val settings: ThreadSettings = ThreadSettings(),
    val status: ThreadStatus,
    val pendingInteractions: Int = 0,
    val queuedInputs: Int = 0,
    val queuePaused: Boolean = false,
    val lastTurn: TurnSummary? = null,
    val lastError: ThreadError? = null,
    val nativeSessionId: String? = null,
    val forkedFrom: ForkOrigin? = null,
    val usage: Usage = Usage(),
    val diffAvailable: Boolean = false,
    val createdAt: Millis,
    val updatedAt: Millis,
    val lastActivityAt: Millis,
    val archived: Boolean = false,
    /** Pinned by the user (`thread/update { pinned }`); clients group pinned threads first. */
    val pinned: Boolean = false,
    /**
     * Head of the thread stream when this summary was produced (excluding the `thread/updated`
     * event that carries it). It grows with every summary change, so of two copies of the same
     * thread the one with the larger head is the newer one (protocol.md §3.1).
     */
    val head: Long = 0,
    /** The thread's background work: how many tasks run, and the one that ended last (protocol.md §3.1). */
    val background: ThreadBackground = ThreadBackground(),
)

/**
 * Summary of a thread's background tasks. It changes when a task starts, ends or changes its
 * `ambient` flag; progress alone does not change it.
 */
@Serializable
data class ThreadBackground(
    /** Tasks with status `running` that are not `ambient` (they keep the agent's process alive). */
    val running: Int = 0,
    /**
     * The last end of a task that is not `ambient` (by `endedAt`, then task id). It only moves
     * on to later ends: when that task starts a new run, it stays the previous run's end until
     * the new run ends. Notifications follow it moving on to a later end.
     */
    val lastEnded: BackgroundTaskEnded? = null,
)

/** The task a thread's background work ended with last (`ThreadBackground.lastEnded`). */
@Serializable
data class BackgroundTaskEnded(
    val taskId: BackgroundTaskId,
    val title: String,
    val kind: BackgroundTaskKind,
    val status: BackgroundTaskStatus,
    val endedAt: Millis,
)

@Serializable
data class TurnSummary(
    val id: TurnId,
    val index: Int,
    val status: TurnStatus,
    val startedAt: Millis,
    val completedAt: Millis? = null,
)

@Serializable
data class ThreadError(val message: String, val kind: String, val at: Millis)

@Serializable
data class ForkOrigin(val threadId: ThreadId, val turnId: TurnId? = null)

@Serializable
data class Usage(
    val inputTokens: Long = 0,
    val outputTokens: Long = 0,
    val cachedInputTokens: Long = 0,
    val reasoningTokens: Long = 0,
    val costUsd: Double? = null,
    /** Context-window occupancy, present only when the harness reported both numbers. */
    val context: ContextUsage? = null,
)

/** Context-window occupancy as reported by the harness (never estimated). */
@Serializable
data class ContextUsage(val usedTokens: Long, val windowTokens: Long)

@Serializable
data class Turn(
    val id: TurnId,
    val threadId: ThreadId,
    val index: Int,
    val status: TurnStatus,
    val startedAt: Millis,
    val completedAt: Millis? = null,
    val model: String? = null,
    val error: TurnError? = null,
    val usage: Usage? = null,
    val diff: DiffSummary? = null,
    /**
     * Why the harness started this run by itself, when it said so explicitly (only on turns the
     * agent started, reported with `turn/completed`).
     */
    val trigger: TurnTrigger? = null,
)

@Serializable
data class TurnError(val message: String, val kind: String)

@Serializable
data class DiffSummary(val files: Int, val insertions: Long, val deletions: Long)

// ----- background tasks ------------------------------------------------------------------------

/**
 * Work the harness runs outside the turn lifecycle: a background agent, a shell left running, a
 * workflow, a scheduled wakeup… Reported only from explicit signals of the harness; it can
 * outlive turns and start again under the same [nativeId] ([runs]). Every
 * `backgroundTask/updated` carries the whole object (protocol.md §3.1).
 */
@Serializable
data class BackgroundTask(
    val id: BackgroundTaskId,
    val threadId: ThreadId,
    /** The harness's own id of the task (Claude's `task_id`, a Codex process id). */
    val nativeId: String,
    val kind: BackgroundTaskKind,
    /** The harness's description, verbatim. */
    val title: String,
    val status: BackgroundTaskStatus,
    /** The harness says it is not activity: shown, but neither counted as running nor keeping the process. */
    val ambient: Boolean = false,
    /** How many times it started under the same [nativeId] (1 for the first run). */
    val runs: Int = 1,
    /** The turn that ran when it was first reported (or the thread's last turn). */
    val turnId: TurnId? = null,
    /** The item that launched it (its status is `backgrounded`). */
    val originItemId: ItemId? = null,
    /** The background task that launched this one. */
    val parentTaskId: BackgroundTaskId? = null,
    /** When the current run started. */
    val startedAt: Millis,
    val endedAt: Millis? = null,
    /** Present once it ended. */
    val endReason: BackgroundEndReason? = null,
    val progress: BackgroundProgress? = null,
    /** What it produced, only from explicit fields of the harness. */
    val result: BackgroundResult? = null,
    val usage: BackgroundUsage? = null,
    /** `backgroundTask/stop` can stop it on its own. */
    val stoppable: Boolean = false,
    /** Set while a `backgroundTask/stop` waits for the harness to report the end. */
    val stopRequestedAt: Millis? = null,
    /** The harness did not report the end within the daemon's confirmation time after the last stop request. */
    val stopUnconfirmedAt: Millis? = null,
    /** When the harness says it runs next (scheduled wakeups). */
    val nextRunAt: Millis? = null,
)

/** Progress a harness reports for a running task; every value is the harness's own. */
@Serializable
data class BackgroundProgress(
    val lastToolName: String? = null,
    val toolUses: Long? = null,
    val tokens: Long? = null,
    val durationMs: Long? = null,
    /** What the task is doing, as the harness summarizes it (display only). */
    val summary: String? = null,
    /** The agents of a workflow in the harness's order (absent when there are none). */
    val workflow: List<WorkflowAgent>? = null,
)

/** One agent of a workflow ([BackgroundProgress.workflow]). */
@Serializable
data class WorkflowAgent(
    val label: String,
    val phase: String? = null,
    val state: WorkflowAgentState,
    val agentType: String? = null,
    val model: String? = null,
    val tokens: Long? = null,
)

/** What a finished task produced (never parsed out of text written for people). */
@Serializable
data class BackgroundResult(
    val summary: String? = null,
    val exitCode: Int? = null,
    /** The output up to the daemon's inline limit; the whole output is in [outputBlobId] when longer. */
    val output: String? = null,
    val outputTruncated: Boolean = false,
    val outputBlobId: BlobId? = null,
)

/** What a task used, as the harness reports it. */
@Serializable
data class BackgroundUsage(
    val totalTokens: Long? = null,
    val toolUses: Long? = null,
    val durationMs: Long? = null,
    val costUsd: Double? = null,
)

// ----- small value types -----------------------------------------------------------------------

@Serializable(with = Attachment.Serializer::class)
sealed interface Attachment {
    @Serializable
    data class Image(val blobId: BlobId, val mime: String) : Attachment

    data class Unknown(val type: String, val raw: JsonObject) : Attachment

    object Serializer : TaggedUnionSerializer<Attachment>("Attachment", "type") {
        override fun tagOf(value: Attachment) = when (value) {
            is Image -> "image"
            is Unknown -> null
        }

        override fun serializerFor(tag: String): KSerializer<out Attachment>? = when (tag) {
            "image" -> Image.serializer()
            else -> null
        }

        override fun unknown(tag: String, raw: JsonObject) = Unknown(tag, raw)
        override fun rawOf(value: Attachment) = (value as? Unknown)?.raw
    }
}

@Serializable
data class Mention(val path: String)

@Serializable
data class FileChange(
    val path: String,
    val kind: FileChangeKind,
    val movePath: String? = null,
    /** Unified diff of this file when the harness provides one. */
    val diff: String? = null,
    val added: Long? = null,
    val removed: Long? = null,
)

@Serializable
data class PlanEntry(val text: String, val status: PlanEntryStatus)

// ----- input, queue, commands ------------------------------------------------------------------

@Serializable(with = InputPart.Serializer::class)
sealed interface InputPart {
    @Serializable
    data class Text(val text: String) : InputPart

    @Serializable
    data class Image(val blobId: BlobId) : InputPart

    @Serializable
    data class Mention(val path: String) : InputPart

    data class Unknown(val type: String, val raw: JsonObject) : InputPart

    object Serializer : TaggedUnionSerializer<InputPart>("InputPart", "type") {
        override fun tagOf(value: InputPart) = when (value) {
            is Text -> "text"
            is Image -> "image"
            is Mention -> "mention"
            is Unknown -> null
        }

        override fun serializerFor(tag: String): KSerializer<out InputPart>? = when (tag) {
            "text" -> Text.serializer()
            "image" -> Image.serializer()
            "mention" -> Mention.serializer()
            else -> null
        }

        override fun unknown(tag: String, raw: JsonObject) = Unknown(tag, raw)
        override fun rawOf(value: InputPart) = (value as? Unknown)?.raw
    }
}

@Serializable
data class QueuedInput(
    val id: QueuedInputId,
    val threadId: ThreadId,
    val createdAt: Millis,
    val preview: String,
    val input: List<InputPart>,
)

@Serializable
data class Command(
    val name: String,
    val description: String? = null,
    val source: CommandSource,
    val argumentHint: String? = null,
    val action: CommandAction,
)

@Serializable(with = CommandAction.Serializer::class)
sealed interface CommandAction {
    /** Insert text into the composer (harness-native commands such as `/compact`). */
    @Serializable
    data class InsertText(val text: String) : CommandAction

    /** Call a protocol method; the client adds `clientRequestId` and `threadId`. */
    @Serializable
    data class Method(val method: String, val params: JsonElement? = null) : CommandAction

    /** Open a picker. */
    @Serializable
    data class Picker(val picker: PickerKind) : CommandAction

    data class Unknown(val type: String, val raw: JsonObject) : CommandAction

    object Serializer : TaggedUnionSerializer<CommandAction>("CommandAction", "type") {
        override fun tagOf(value: CommandAction) = when (value) {
            is InsertText -> "insertText"
            is Method -> "method"
            is Picker -> "picker"
            is Unknown -> null
        }

        override fun serializerFor(tag: String): KSerializer<out CommandAction>? = when (tag) {
            "insertText" -> InsertText.serializer()
            "method" -> Method.serializer()
            "picker" -> Picker.serializer()
            else -> null
        }

        override fun unknown(tag: String, raw: JsonObject) = Unknown(tag, raw)
        override fun rawOf(value: CommandAction) = (value as? Unknown)?.raw
    }
}

// ----- operations, sessions, devices, filesystem -----------------------------------------------

@Serializable
data class Operation(
    val id: OperationId,
    val kind: OperationKind,
    val status: OperationStatus,
    val projectId: ProjectId? = null,
    val message: String? = null,
    /** The tool's latest progress line, verbatim and uninterpreted; only while running. */
    val progress: String? = null,
    val startedAt: Millis,
    val finishedAt: Millis? = null,
)

@Serializable
data class NativeSession(
    val nativeSessionId: String,
    val title: String? = null,
    val updatedAt: Millis? = null,
    val cwd: String? = null,
    val importedThreadId: ThreadId? = null,
)

@Serializable
data class Device(
    val id: DeviceId,
    val name: String,
    val platform: String? = null,
    val createdAt: Millis,
    val lastSeenAt: Millis? = null,
    val current: Boolean = false,
)

@Serializable
data class FsRoot(val path: String, val name: String)

@Serializable
data class FsEntry(
    val name: String,
    val path: String,
    val isDir: Boolean,
    val isGitRepo: Boolean? = null,
    val size: Long? = null,
    val modifiedAt: Millis? = null,
)

@Serializable
data class SearchResult(val path: String, val isDir: Boolean)

@Serializable
data class DiffFile(
    val path: String,
    val kind: FileChangeKind,
    val added: Long,
    val removed: Long,
    val binary: Boolean,
)

@Serializable(with = DiffScope.Serializer::class)
sealed interface DiffScope {
    @Serializable
    data class Turn(val turnId: TurnId) : DiffScope

    @Serializable
    data object Thread : DiffScope

    data class Unknown(val kind: String, val raw: JsonObject) : DiffScope

    object Serializer : TaggedUnionSerializer<DiffScope>("DiffScope", "kind") {
        override fun tagOf(value: DiffScope) = when (value) {
            is Turn -> "turn"
            Thread -> "thread"
            is Unknown -> null
        }

        override fun serializerFor(tag: String): KSerializer<out DiffScope>? = when (tag) {
            "turn" -> Turn.serializer()
            "thread" -> Thread.serializer()
            else -> null
        }

        override fun unknown(tag: String, raw: JsonObject) = Unknown(tag, raw)
        override fun rawOf(value: DiffScope) = (value as? Unknown)?.raw
    }
}

/** Policy values the client must honour (from `initialize`). */
@Serializable
data class ClientPolicy(
    val heartbeatIntervalMs: Long,
    val clientTimeoutMs: Long,
    val maxClientFrameBytes: Long,
    val maxBlobBytes: Long,
)
