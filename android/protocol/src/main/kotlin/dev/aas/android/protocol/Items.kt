package dev.aas.android.protocol

import kotlinx.serialization.KSerializer
import kotlinx.serialization.Serializable
import kotlinx.serialization.json.JsonElement
import kotlinx.serialization.json.JsonObject
import kotlinx.serialization.json.JsonPrimitive

/**
 * One element of a turn. Every variant carries the common fields; `kind` selects the variant
 * on the wire. Unknown kinds decode to [Unknown] (rendered as a generic row).
 */
@Serializable(with = Item.Serializer::class)
sealed interface Item {
    val id: ItemId
    val threadId: ThreadId
    val turnId: TurnId
    val status: ItemStatus
    val startedAt: Millis
    val completedAt: Millis?

    @Serializable
    data class UserMessage(
        override val id: ItemId,
        override val threadId: ThreadId,
        override val turnId: TurnId,
        override val status: ItemStatus,
        override val startedAt: Millis,
        override val completedAt: Millis? = null,
        val text: String,
        val attachments: List<Attachment> = emptyList(),
        val mentions: List<Mention> = emptyList(),
        val delivery: UserMessageDelivery = UserMessageDelivery.Normal,
    ) : Item

    /** Markdown text. */
    @Serializable
    data class AgentMessage(
        override val id: ItemId,
        override val threadId: ThreadId,
        override val turnId: TurnId,
        override val status: ItemStatus,
        override val startedAt: Millis,
        override val completedAt: Millis? = null,
        val text: String,
    ) : Item

    @Serializable
    data class Reasoning(
        override val id: ItemId,
        override val threadId: ThreadId,
        override val turnId: TurnId,
        override val status: ItemStatus,
        override val startedAt: Millis,
        override val completedAt: Millis? = null,
        val text: String,
    ) : Item

    @Serializable
    data class CommandExecution(
        override val id: ItemId,
        override val threadId: ThreadId,
        override val turnId: TurnId,
        override val status: ItemStatus,
        override val startedAt: Millis,
        override val completedAt: Millis? = null,
        val command: String,
        val cwd: String? = null,
        val output: String,
        val outputTruncated: Boolean = false,
        val outputBlobId: BlobId? = null,
        val exitCode: Int? = null,
        val durationMs: Long? = null,
    ) : Item

    @Serializable
    data class FileChangeItem(
        override val id: ItemId,
        override val threadId: ThreadId,
        override val turnId: TurnId,
        override val status: ItemStatus,
        override val startedAt: Millis,
        override val completedAt: Millis? = null,
        val changes: List<FileChange>,
    ) : Item

    @Serializable
    data class ToolCall(
        override val id: ItemId,
        override val threadId: ThreadId,
        override val turnId: TurnId,
        override val status: ItemStatus,
        override val startedAt: Millis,
        override val completedAt: Millis? = null,
        val category: ToolCategory,
        val name: String,
        val title: String,
        val server: String? = null,
        val input: JsonElement? = null,
        val output: String? = null,
        val outputTruncated: Boolean = false,
        val outputBlobId: BlobId? = null,
    ) : Item

    @Serializable
    data class Plan(
        override val id: ItemId,
        override val threadId: ThreadId,
        override val turnId: TurnId,
        override val status: ItemStatus,
        override val startedAt: Millis,
        override val completedAt: Millis? = null,
        val entries: List<PlanEntry>,
    ) : Item

    @Serializable
    data class Notice(
        override val id: ItemId,
        override val threadId: ThreadId,
        override val turnId: TurnId,
        override val status: ItemStatus,
        override val startedAt: Millis,
        override val completedAt: Millis? = null,
        val level: NoticeLevel,
        val message: String,
        val code: String? = null,
    ) : Item

    /** An item kind this client does not know; the raw object is kept verbatim. */
    data class Unknown(val kind: String, val raw: JsonObject) : Item {
        override val id: ItemId get() = raw.str("id")
        override val threadId: ThreadId get() = raw.str("threadId")
        override val turnId: TurnId get() = raw.str("turnId")
        override val status: ItemStatus
            get() = ItemStatus.entries.firstOrNull { it.wire == raw.strOrNull("status") } ?: ItemStatus.Unknown
        override val startedAt: Millis get() = raw.long("startedAt")
        override val completedAt: Millis? get() = raw.longOrNull("completedAt")
    }

    object Serializer : TaggedUnionSerializer<Item>("Item", "kind") {
        override fun tagOf(value: Item) = when (value) {
            is UserMessage -> "userMessage"
            is AgentMessage -> "agentMessage"
            is Reasoning -> "reasoning"
            is CommandExecution -> "commandExecution"
            is FileChangeItem -> "fileChange"
            is ToolCall -> "toolCall"
            is Plan -> "plan"
            is Notice -> "notice"
            is Unknown -> null
        }

        override fun serializerFor(tag: String): KSerializer<out Item>? = when (tag) {
            "userMessage" -> UserMessage.serializer()
            "agentMessage" -> AgentMessage.serializer()
            "reasoning" -> Reasoning.serializer()
            "commandExecution" -> CommandExecution.serializer()
            "fileChange" -> FileChangeItem.serializer()
            "toolCall" -> ToolCall.serializer()
            "plan" -> Plan.serializer()
            "notice" -> Notice.serializer()
            else -> null
        }

        override fun unknown(tag: String, raw: JsonObject) = Unknown(tag, raw)
        override fun rawOf(value: Item) = (value as? Unknown)?.raw
    }
}

/**
 * Appends streamed text to the field a delta targets (`item/delta`). Returns the item
 * unchanged when its kind has no such field.
 */
fun Item.appendDelta(field: DeltaField, text: String): Item = when {
    this is Item.AgentMessage && field == DeltaField.Text -> copy(text = this.text + text)
    this is Item.Reasoning && field == DeltaField.Text -> copy(text = this.text + text)
    this is Item.CommandExecution && field == DeltaField.Output -> copy(output = output + text)
    this is Item.ToolCall && field == DeltaField.Output -> copy(output = (output ?: "") + text)
    this is Item.Unknown -> {
        val key = field.wire
        val current = (raw[key] as? JsonPrimitive)?.content ?: ""
        Item.Unknown(kind, JsonObject(raw + (key to JsonPrimitive(current + text))))
    }
    else -> this
}

// ----- interactions ----------------------------------------------------------------------------

@Serializable
data class Interaction(
    val id: InteractionId,
    val threadId: ThreadId,
    val turnId: TurnId? = null,
    val itemId: ItemId? = null,
    val status: InteractionStatus,
    val createdAt: Millis,
    val resolvedAt: Millis? = null,
    /** Device id that answered, or `"system"`. */
    val resolvedBy: String? = null,
    val request: InteractionRequest,
    val resolution: InteractionResolution? = null,
    val expireReason: ExpireReason? = null,
)

@Serializable(with = InteractionRequest.Serializer::class)
sealed interface InteractionRequest {
    val title: String

    @Serializable
    data class Approval(
        override val title: String,
        val detail: String? = null,
        val subject: Subject,
        val options: List<ApprovalOption>,
    ) : InteractionRequest

    @Serializable
    data class Question(override val title: String, val questions: List<dev.aas.android.protocol.Question>) : InteractionRequest

    data class Unknown(val kind: String, val raw: JsonObject) : InteractionRequest {
        override val title: String get() = raw.str("title")
    }

    object Serializer : TaggedUnionSerializer<InteractionRequest>("InteractionRequest", "kind") {
        override fun tagOf(value: InteractionRequest) = when (value) {
            is Approval -> "approval"
            is Question -> "question"
            is Unknown -> null
        }

        override fun serializerFor(tag: String): KSerializer<out InteractionRequest>? = when (tag) {
            "approval" -> Approval.serializer()
            "question" -> Question.serializer()
            else -> null
        }

        override fun unknown(tag: String, raw: JsonObject) = Unknown(tag, raw)
        override fun rawOf(value: InteractionRequest) = (value as? Unknown)?.raw
    }
}

@Serializable(with = Subject.Serializer::class)
sealed interface Subject {
    @Serializable
    data class Command(val command: String, val cwd: String? = null) : Subject

    @Serializable
    data class FileChanges(val changes: List<FileChange>) : Subject

    @Serializable
    data class Tool(val name: String, val input: JsonElement? = null) : Subject

    @Serializable
    data class Plan(val text: String) : Subject

    @Serializable
    data class Permissions(val description: String) : Subject

    @Serializable
    data class Other(val description: String) : Subject

    data class Unknown(val type: String, val raw: JsonObject) : Subject

    object Serializer : TaggedUnionSerializer<Subject>("Subject", "type") {
        override fun tagOf(value: Subject) = when (value) {
            is Command -> "command"
            is FileChanges -> "fileChange"
            is Tool -> "tool"
            is Plan -> "plan"
            is Permissions -> "permissions"
            is Other -> "other"
            is Unknown -> null
        }

        override fun serializerFor(tag: String): KSerializer<out Subject>? = when (tag) {
            "command" -> Command.serializer()
            "fileChange" -> FileChanges.serializer()
            "tool" -> Tool.serializer()
            "plan" -> Plan.serializer()
            "permissions" -> Permissions.serializer()
            "other" -> Other.serializer()
            else -> null
        }

        override fun unknown(tag: String, raw: JsonObject) = Unknown(tag, raw)
        override fun rawOf(value: Subject) = (value as? Unknown)?.raw
    }
}

@Serializable
data class ApprovalOption(val id: String, val label: String, val kind: ApprovalOptionKind)

@Serializable
data class Question(
    val id: String,
    val header: String? = null,
    val prompt: String,
    val choices: List<QuestionChoice> = emptyList(),
    val multiSelect: Boolean = false,
    val allowFreeText: Boolean = false,
    val placeholder: String? = null,
)

@Serializable
data class QuestionChoice(val id: String, val label: String, val description: String? = null)

@Serializable(with = InteractionResolution.Serializer::class)
sealed interface InteractionResolution {
    @Serializable
    data class Approval(val optionId: String, val feedback: String? = null) : InteractionResolution

    @Serializable
    data class Question(val answers: List<QuestionAnswer>) : InteractionResolution

    @Serializable
    data object Dismissed : InteractionResolution

    data class Unknown(val kind: String, val raw: JsonObject) : InteractionResolution

    object Serializer : TaggedUnionSerializer<InteractionResolution>("InteractionResolution", "kind") {
        override fun tagOf(value: InteractionResolution) = when (value) {
            is Approval -> "approval"
            is Question -> "question"
            Dismissed -> "dismissed"
            is Unknown -> null
        }

        override fun serializerFor(tag: String): KSerializer<out InteractionResolution>? = when (tag) {
            "approval" -> Approval.serializer()
            "question" -> Question.serializer()
            "dismissed" -> Dismissed.serializer()
            else -> null
        }

        override fun unknown(tag: String, raw: JsonObject) = Unknown(tag, raw)
        override fun rawOf(value: InteractionResolution) = (value as? Unknown)?.raw
    }
}

@Serializable
data class QuestionAnswer(val questionId: String, val choiceIds: List<String> = emptyList(), val text: String? = null)
