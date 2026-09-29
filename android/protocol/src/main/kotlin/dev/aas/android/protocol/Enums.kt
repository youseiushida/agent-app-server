package dev.aas.android.protocol

import kotlinx.serialization.Serializable

@Serializable(with = HarnessKind.Serializer::class)
enum class HarnessKind(override val wire: String) : WireEnum {
    Codex("codex"), Claude("claude"), Pi("pi"), Acp("acp"), Fake("fake"), Unknown("unknown");

    object Serializer : WireEnumSerializer<HarnessKind>("HarnessKind", HarnessKind.entries, Unknown)
}

@Serializable(with = ThreadStatus.Serializer::class)
enum class ThreadStatus(override val wire: String) : WireEnum {
    Idle("idle"), Queued("queued"), Starting("starting"), Ready("ready"), Running("running"), Stopping("stopping"),
    Unknown("unknown");

    object Serializer : WireEnumSerializer<ThreadStatus>("ThreadStatus", ThreadStatus.entries, Unknown)
}

@Serializable(with = TurnStatus.Serializer::class)
enum class TurnStatus(override val wire: String) : WireEnum {
    Running("running"), Completed("completed"), Interrupted("interrupted"), Failed("failed"), Unknown("unknown");

    val isTerminal: Boolean get() = this != Running

    object Serializer : WireEnumSerializer<TurnStatus>("TurnStatus", TurnStatus.entries, Unknown)
}

@Serializable(with = ItemStatus.Serializer::class)
enum class ItemStatus(override val wire: String) : WireEnum {
    InProgress("inProgress"), Completed("completed"), Failed("failed"), Declined("declined"),
    Interrupted("interrupted"),

    /**
     * The item launched work that goes on as a background task ([Item.backgroundTaskId]); the
     * task has its own lifecycle (protocol.md §3.1).
     */
    Backgrounded("backgrounded"),
    Unknown("unknown"),
    ;

    object Serializer : WireEnumSerializer<ItemStatus>("ItemStatus", ItemStatus.entries, Unknown)
}

@Serializable(with = UserMessageDelivery.Serializer::class)
enum class UserMessageDelivery(override val wire: String) : WireEnum {
    Normal("normal"), Steer("steer"), Unknown("unknown");

    object Serializer : WireEnumSerializer<UserMessageDelivery>("UserMessageDelivery", UserMessageDelivery.entries, Unknown)
}

@Serializable(with = FileChangeKind.Serializer::class)
enum class FileChangeKind(override val wire: String) : WireEnum {
    Add("add"), Delete("delete"), Update("update"), Move("move"), Unknown("unknown");

    object Serializer : WireEnumSerializer<FileChangeKind>("FileChangeKind", FileChangeKind.entries, Unknown)
}

@Serializable(with = ToolCategory.Serializer::class)
enum class ToolCategory(override val wire: String) : WireEnum {
    Read("read"), Search("search"), Fetch("fetch"), Mcp("mcp"), Subagent("subagent"), Edit("edit"),
    Execute("execute"), Think("think"), Other("other"), Unknown("unknown");

    object Serializer : WireEnumSerializer<ToolCategory>("ToolCategory", ToolCategory.entries, Unknown)
}

@Serializable(with = PlanEntryStatus.Serializer::class)
enum class PlanEntryStatus(override val wire: String) : WireEnum {
    Pending("pending"), InProgress("inProgress"), Completed("completed"), Unknown("unknown");

    object Serializer : WireEnumSerializer<PlanEntryStatus>("PlanEntryStatus", PlanEntryStatus.entries, Unknown)
}

@Serializable(with = NoticeLevel.Serializer::class)
enum class NoticeLevel(override val wire: String) : WireEnum {
    Info("info"), Warning("warning"), Error("error"), Unknown("unknown");

    object Serializer : WireEnumSerializer<NoticeLevel>("NoticeLevel", NoticeLevel.entries, Unknown)
}

@Serializable(with = DeltaField.Serializer::class)
enum class DeltaField(override val wire: String) : WireEnum {
    Text("text"), Output("output"), Unknown("unknown");

    object Serializer : WireEnumSerializer<DeltaField>("DeltaField", DeltaField.entries, Unknown)
}

@Serializable(with = InteractionStatus.Serializer::class)
enum class InteractionStatus(override val wire: String) : WireEnum {
    Pending("pending"), Resolved("resolved"), Expired("expired"), Unknown("unknown");

    object Serializer : WireEnumSerializer<InteractionStatus>("InteractionStatus", InteractionStatus.entries, Unknown)
}

@Serializable(with = ExpireReason.Serializer::class)
enum class ExpireReason(override val wire: String) : WireEnum {
    ProcessExited("processExited"), TurnEnded("turnEnded"), HarnessCancelled("harnessCancelled"),
    DaemonRestarted("daemonRestarted"),

    /** The background task that asked ended before an answer. */
    TaskEnded("taskEnded"),
    Unknown("unknown"),
    ;

    object Serializer : WireEnumSerializer<ExpireReason>("ExpireReason", ExpireReason.entries, Unknown)
}

@Serializable(with = ApprovalOptionKind.Serializer::class)
enum class ApprovalOptionKind(override val wire: String) : WireEnum {
    AllowOnce("allowOnce"), AllowForSession("allowForSession"), AllowAlways("allowAlways"), Deny("deny"),
    DenyWithFeedback("denyWithFeedback"), Abort("abort"), Unknown("unknown");

    /** Options that grant something (as opposed to refusing). */
    val isAllow: Boolean get() = this == AllowOnce || this == AllowForSession || this == AllowAlways

    object Serializer : WireEnumSerializer<ApprovalOptionKind>("ApprovalOptionKind", ApprovalOptionKind.entries, Unknown)
}

@Serializable(with = Delivery.Serializer::class)
enum class Delivery(override val wire: String) : WireEnum {
    Auto("auto"), Steer("steer"), Queue("queue"), Unknown("unknown");

    object Serializer : WireEnumSerializer<Delivery>("Delivery", Delivery.entries, Unknown)
}

@Serializable(with = Disposition.Serializer::class)
enum class Disposition(override val wire: String) : WireEnum {
    Started("started"), Steered("steered"), Queued("queued"), Unknown("unknown");

    object Serializer : WireEnumSerializer<Disposition>("Disposition", Disposition.entries, Unknown)
}

@Serializable(with = CommandSource.Serializer::class)
enum class CommandSource(override val wire: String) : WireEnum {
    App("app"), Harness("harness"), Unknown("unknown");

    object Serializer : WireEnumSerializer<CommandSource>("CommandSource", CommandSource.entries, Unknown)
}

@Serializable(with = PickerKind.Serializer::class)
enum class PickerKind(override val wire: String) : WireEnum {
    Model("model"), Effort("effort"), PermissionMode("permissionMode"), Unknown("unknown");

    object Serializer : WireEnumSerializer<PickerKind>("PickerKind", PickerKind.entries, Unknown)
}

@Serializable(with = OperationKind.Serializer::class)
enum class OperationKind(override val wire: String) : WireEnum {
    GitClone("gitClone"), Unknown("unknown");

    object Serializer : WireEnumSerializer<OperationKind>("OperationKind", OperationKind.entries, Unknown)
}

@Serializable(with = OperationStatus.Serializer::class)
enum class OperationStatus(override val wire: String) : WireEnum {
    Running("running"), Succeeded("succeeded"), Failed("failed"), Cancelled("cancelled"), Unknown("unknown");

    /** The operation ended (a status this client does not know counts as ended, not running). */
    val isTerminal: Boolean get() = this != Running

    object Serializer : WireEnumSerializer<OperationStatus>("OperationStatus", OperationStatus.entries, Unknown)
}

@Serializable(with = SettingsOutcome.Serializer::class)
enum class SettingsOutcome(override val wire: String) : WireEnum {
    AppliedLive("appliedLive"), AppliesNextTurn("appliesNextTurn"), Unknown("unknown");

    object Serializer : WireEnumSerializer<SettingsOutcome>("SettingsOutcome", SettingsOutcome.entries, Unknown)
}

/** What happened to the native session's name (`thread/update` result `nativeRename.status`). */
@Serializable(with = NativeRenameStatus.Serializer::class)
enum class NativeRenameStatus(override val wire: String) : WireEnum {
    /** The running agent took the name. */
    Applied("applied"),

    /** No agent runs now: the name is given when the next one starts. */
    Pending("pending"),

    /** The harness refused the name; the thread keeps its new title. */
    Failed("failed"),
    Unknown("unknown"),
    ;

    object Serializer : WireEnumSerializer<NativeRenameStatus>("NativeRenameStatus", NativeRenameStatus.entries, Unknown)
}

@Serializable(with = SubscriptionState.Serializer::class)
enum class SubscriptionState(override val wire: String) : WireEnum {
    Ok("ok"), NotFound("notFound"), Unknown("unknown");

    object Serializer : WireEnumSerializer<SubscriptionState>("SubscriptionState", SubscriptionState.entries, Unknown)
}

/**
 * Why the server stops (`server/shuttingDown.reason`): one value per stop, the same for every
 * connection. [StorageFailure] is the daemon stopping itself because it can no longer persist
 * its event log (the watchdog restarts it; the client reconnects and resends its outbox as
 * after any restart).
 */
@Serializable(with = ShutdownReason.Serializer::class)
enum class ShutdownReason(override val wire: String) : WireEnum {
    Drain("drain"), Shutdown("shutdown"), StorageFailure("storageFailure"), Unknown("unknown");

    object Serializer : WireEnumSerializer<ShutdownReason>("ShutdownReason", ShutdownReason.entries, Unknown)
}

/** What made the harness start a run by itself, as it reported it explicitly (`Turn.trigger`). */
@Serializable(with = TurnTrigger.Serializer::class)
enum class TurnTrigger(override val wire: String) : WireEnum {
    /** A background task ended (or reported something) and the harness took it up. */
    BackgroundTask("backgroundTask"),

    /** A wakeup the harness had scheduled for itself came due. */
    Scheduled("scheduled"),
    Unknown("unknown"),
    ;

    object Serializer : WireEnumSerializer<TurnTrigger>("TurnTrigger", TurnTrigger.entries, Unknown)
}

/** What kind of work a background task is, as the harness reports it. */
@Serializable(with = BackgroundTaskKind.Serializer::class)
enum class BackgroundTaskKind(override val wire: String) : WireEnum {
    /** A background sub-agent. */
    Agent("agent"),

    /** A command left running (a background shell or terminal). */
    Shell("shell"),

    /** A multi-agent workflow. */
    Workflow("workflow"),

    /** A watcher that reports what it observes. */
    Monitor("monitor"),

    /** Work that runs elsewhere (in the cloud) and that the harness tracks. */
    Remote("remote"),

    /** A wakeup the harness scheduled for itself (`nextRunAt`). */
    Scheduled("scheduled"),
    Other("other"),
    Unknown("unknown"),
    ;

    object Serializer : WireEnumSerializer<BackgroundTaskKind>("BackgroundTaskKind", BackgroundTaskKind.entries, Unknown)
}

@Serializable(with = BackgroundTaskStatus.Serializer::class)
enum class BackgroundTaskStatus(override val wire: String) : WireEnum {
    Running("running"),

    /** The harness reported that it finished. */
    Completed("completed"),

    /** The harness reported that it failed. */
    Failed("failed"),

    /** Stopped: by the harness (e.g. after `backgroundTask/stop`) or with the agent's process (`endReason`). */
    Stopped("stopped"),

    /** The agent's process ended unexpectedly, or the daemon restarted: how it ended is unknown. */
    Lost("lost"),
    Unknown("unknown"),
    ;

    /** The task ended (a status this client does not know counts as ended, not as running forever). */
    val isTerminal: Boolean get() = this != Running

    object Serializer : WireEnumSerializer<BackgroundTaskStatus>("BackgroundTaskStatus", BackgroundTaskStatus.entries, Unknown)
}

/** Why a background task ended (`BackgroundTask.endReason`). */
@Serializable(with = BackgroundEndReason.Serializer::class)
enum class BackgroundEndReason(override val wire: String) : WireEnum {
    /** The harness reported the end. */
    Harness("harness"),

    /** `thread/stop` or an archive stopped the agent's process. */
    ThreadStopped("threadStopped"),

    /** The idle agent's process was stopped. */
    IdleStop("idleStop"),

    /** The daemon was stopped. */
    DaemonShutdown("daemonShutdown"),

    /** Windows ended the session (sign-out, shutdown, restart). */
    SystemShutdown("systemShutdown"),

    /** The agent's process was terminated because it did not honour an interrupt. */
    ForcedStop("forcedStop"),

    /** The agent's process was replaced to apply the thread's settings. */
    ProcessReplaced("processReplaced"),

    /** The agent's process ended by itself. */
    ProcessExited("processExited"),

    /** The daemon restarted while the task ran. */
    DaemonRestarted("daemonRestarted"),
    Unknown("unknown"),
    ;

    object Serializer : WireEnumSerializer<BackgroundEndReason>("BackgroundEndReason", BackgroundEndReason.entries, Unknown)
}

/** State of one agent of a workflow (`WorkflowAgent.state`). */
@Serializable(with = WorkflowAgentState.Serializer::class)
enum class WorkflowAgentState(override val wire: String) : WireEnum {
    Start("start"), Progress("progress"), Done("done"), Error("error"), Unknown("unknown");

    object Serializer : WireEnumSerializer<WorkflowAgentState>("WorkflowAgentState", WorkflowAgentState.entries, Unknown)
}
