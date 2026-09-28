package dev.aas.android.protocol

import kotlinx.serialization.KSerializer
import kotlinx.serialization.Serializable
import kotlinx.serialization.descriptors.SerialDescriptor
import kotlinx.serialization.encoding.Decoder
import kotlinx.serialization.encoding.Encoder
import kotlinx.serialization.json.JsonDecoder
import kotlinx.serialization.json.JsonElement
import kotlinx.serialization.json.JsonEncoder
import kotlinx.serialization.json.JsonObject
import kotlinx.serialization.json.JsonPrimitive
import kotlinx.serialization.json.buildJsonObject
import kotlinx.serialization.json.contentOrNull
import kotlinx.serialization.json.jsonObject
import kotlinx.serialization.json.longOrNull
import kotlinx.serialization.json.put

/** Every event type of both streams (`stream/batch` payload). */
sealed interface Event {
    // ----- workspace -----
    @Serializable
    data class ProjectUpserted(val project: Project) : Event

    @Serializable
    data class ProjectRemoved(val projectId: ProjectId) : Event

    @Serializable
    data class ThreadUpserted(val thread: Thread) : Event

    @Serializable
    data class ThreadRemoved(val threadId: ThreadId) : Event

    @Serializable
    data class InteractionPending(val interaction: Interaction) : Event

    @Serializable
    data class InteractionClosed(val interactionId: InteractionId, val threadId: ThreadId, val status: InteractionStatus) : Event

    @Serializable
    data class HarnessUpdated(val harness: Harness) : Event

    @Serializable
    data class OperationUpdated(val operation: Operation) : Event

    // ----- thread -----
    @Serializable
    data class ThreadUpdated(val thread: Thread) : Event

    @Serializable
    data class TurnStarted(val turn: Turn) : Event

    @Serializable
    data class TurnCompleted(val turn: Turn) : Event

    @Serializable
    data class TurnDiffUpdated(val turnId: TurnId, val diff: DiffSummary) : Event

    /** Usage of a running turn so far (cumulative for the turn; `usage.context` is current). */
    @Serializable
    data class TurnUsageUpdated(val turnId: TurnId, val usage: Usage) : Event

    @Serializable
    data class ItemStarted(val item: Item) : Event

    @Serializable
    data class ItemDelta(val itemId: ItemId, val field: DeltaField, val text: String) : Event

    @Serializable
    data class ItemUpdated(val item: Item) : Event

    @Serializable
    data class ItemCompleted(val item: Item) : Event

    @Serializable
    data class InteractionRequested(val interaction: Interaction) : Event

    @Serializable
    data class InteractionResolved(val interaction: Interaction) : Event

    @Serializable
    data class InteractionExpired(val interaction: Interaction) : Event

    @Serializable
    data class QueueUpdated(val queued: List<QueuedInput>) : Event

    @Serializable
    data object CommandsChanged : Event

    /** A background task started, progressed or ended (always the whole task; protocol.md §3.1). */
    @Serializable
    data class BackgroundTaskUpdated(val task: BackgroundTask) : Event

    @Serializable
    data class Native(val harnessId: String, val payload: JsonElement) : Event

    /** An event type this client does not know (ignored by the sync engine). */
    data class Unknown(val type: String, val data: JsonElement) : Event

    companion object {
        /** Wire name of an event, `null` for [Unknown]. */
        fun typeOf(event: Event): String? = when (event) {
            is ProjectUpserted -> "project/upserted"
            is ProjectRemoved -> "project/removed"
            is ThreadUpserted -> "thread/upserted"
            is ThreadRemoved -> "thread/removed"
            is InteractionPending -> "interaction/pending"
            is InteractionClosed -> "interaction/closed"
            is HarnessUpdated -> "harness/updated"
            is OperationUpdated -> "operation/updated"
            is ThreadUpdated -> "thread/updated"
            is TurnStarted -> "turn/started"
            is TurnCompleted -> "turn/completed"
            is TurnDiffUpdated -> "turn/diffUpdated"
            is TurnUsageUpdated -> "turn/usageUpdated"
            is ItemStarted -> "item/started"
            is ItemDelta -> "item/delta"
            is ItemUpdated -> "item/updated"
            is ItemCompleted -> "item/completed"
            is InteractionRequested -> "interaction/requested"
            is InteractionResolved -> "interaction/resolved"
            is InteractionExpired -> "interaction/expired"
            is QueueUpdated -> "queue/updated"
            CommandsChanged -> "commands/changed"
            is BackgroundTaskUpdated -> "backgroundTask/updated"
            is Native -> "native"
            is Unknown -> null
        }

        /** Wire names of every event type this client knows. */
        val knownTypes: List<String> = listOf(
            "project/upserted", "project/removed", "thread/upserted", "thread/removed", "interaction/pending",
            "interaction/closed", "harness/updated", "operation/updated", "thread/updated", "turn/started",
            "turn/completed", "turn/diffUpdated", "turn/usageUpdated", "item/started", "item/delta", "item/updated",
            "item/completed", "interaction/requested", "interaction/resolved", "interaction/expired", "queue/updated",
            "commands/changed", "backgroundTask/updated", "native",
        )

        fun serializerFor(type: String): KSerializer<out Event>? = when (type) {
            "project/upserted" -> ProjectUpserted.serializer()
            "project/removed" -> ProjectRemoved.serializer()
            "thread/upserted" -> ThreadUpserted.serializer()
            "thread/removed" -> ThreadRemoved.serializer()
            "interaction/pending" -> InteractionPending.serializer()
            "interaction/closed" -> InteractionClosed.serializer()
            "harness/updated" -> HarnessUpdated.serializer()
            "operation/updated" -> OperationUpdated.serializer()
            "thread/updated" -> ThreadUpdated.serializer()
            "turn/started" -> TurnStarted.serializer()
            "turn/completed" -> TurnCompleted.serializer()
            "turn/diffUpdated" -> TurnDiffUpdated.serializer()
            "turn/usageUpdated" -> TurnUsageUpdated.serializer()
            "item/started" -> ItemStarted.serializer()
            "item/delta" -> ItemDelta.serializer()
            "item/updated" -> ItemUpdated.serializer()
            "item/completed" -> ItemCompleted.serializer()
            "interaction/requested" -> InteractionRequested.serializer()
            "interaction/resolved" -> InteractionResolved.serializer()
            "interaction/expired" -> InteractionExpired.serializer()
            "queue/updated" -> QueueUpdated.serializer()
            "commands/changed" -> CommandsChanged.serializer()
            "backgroundTask/updated" -> BackgroundTaskUpdated.serializer()
            "native" -> Native.serializer()
            else -> null
        }
    }
}

/**
 * One event of a stream. `seqFrom` is present when the event is the concatenation of the
 * deltas `seqFrom..=seq`. Sequence numbers are cursors, not a dense range.
 */
@Serializable(with = EventEnvelope.Serializer::class)
data class EventEnvelope(
    val seq: Long,
    val seqFrom: Long? = null,
    val ts: Millis,
    val event: Event,
) {
    object Serializer : KSerializer<EventEnvelope> {
        override val descriptor: SerialDescriptor = JsonObject.serializer().descriptor

        @Suppress("UNCHECKED_CAST")
        override fun serialize(encoder: Encoder, value: EventEnvelope) {
            val json = encoder as JsonEncoder
            val (type, data) = when (val e = value.event) {
                is Event.Unknown -> e.type to e.data
                else -> {
                    val type = Event.typeOf(e)!!
                    val serializer = Event.serializerFor(type) as KSerializer<Event>
                    type to json.json.encodeToJsonElement(serializer, e)
                }
            }
            json.encodeJsonElement(
                buildJsonObject {
                    put("seq", value.seq)
                    value.seqFrom?.let { put("seqFrom", it) }
                    put("ts", value.ts)
                    put("type", type)
                    put("data", data)
                },
            )
        }

        override fun deserialize(decoder: Decoder): EventEnvelope {
            val json = decoder as JsonDecoder
            val obj = json.decodeJsonElement().jsonObject
            val type = (obj["type"] as? JsonPrimitive)?.contentOrNull.orEmpty()
            val data = obj["data"] ?: JsonObject(emptyMap())
            val event = Event.serializerFor(type)?.let { json.json.decodeFromJsonElement(it, data) } ?: Event.Unknown(type, data)
            return EventEnvelope(
                seq = (obj["seq"] as? JsonPrimitive)?.longOrNull ?: error("event without seq"),
                seqFrom = (obj["seqFrom"] as? JsonPrimitive)?.longOrNull,
                ts = (obj["ts"] as? JsonPrimitive)?.longOrNull ?: 0L,
                event = event,
            )
        }
    }
}

const val WORKSPACE_STREAM = "workspace"

fun threadStream(threadId: ThreadId): String = "thread:$threadId"

/** The thread id of a `thread:<id>` stream name, or `null`. */
fun threadIdOfStream(stream: String): ThreadId? = stream.removePrefix("thread:").takeIf { stream.startsWith("thread:") && it.isNotEmpty() }
