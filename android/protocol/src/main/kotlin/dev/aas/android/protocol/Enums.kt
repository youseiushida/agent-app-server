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
    Interrupted("interrupted"), Unknown("unknown");

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
    DaemonRestarted("daemonRestarted"), Unknown("unknown");

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
