package dev.aas.android.protocol

import kotlinx.serialization.Serializable
import kotlinx.serialization.json.JsonElement

@Serializable
data class StreamBatch(val stream: String, val head: Long, val events: List<EventEnvelope>)

@Serializable
data class Heartbeat(val serverTime: Millis, val heads: Map<String, Long> = emptyMap())

@Serializable
data object ConnectionReplaced

/**
 * The server announces its stop (close code 1001 follows). [restartExpected] is `true` only when
 * the server will be started again by itself (a daemon under the watchdog stopping for a
 * failure, today [ShutdownReason.StorageFailure]); `false` for `stop`, drain, the end of the
 * Windows session and a server without the watchdog: it may stay down until someone starts it.
 * The client reconnects with backoff in both cases (protocol.md §5).
 */
@Serializable
data class ServerShuttingDown(val reason: ShutdownReason, val restartExpected: Boolean)

/** A parsed server notification. */
sealed interface ServerNotification {
    data class Batch(val batch: StreamBatch) : ServerNotification

    data class Beat(val heartbeat: Heartbeat) : ServerNotification

    data object Replaced : ServerNotification

    data class ShuttingDown(val info: ServerShuttingDown) : ServerNotification

    data class Unknown(val method: String, val params: JsonElement?) : ServerNotification

    companion object {
        const val STREAM_BATCH = "stream/batch"
        const val HEARTBEAT = "heartbeat"
        const val CONNECTION_REPLACED = "connection/replaced"
        const val SERVER_SHUTTING_DOWN = "server/shuttingDown"

        /** Every notification method this client knows. */
        val knownMethods: List<String> = listOf(STREAM_BATCH, HEARTBEAT, CONNECTION_REPLACED, SERVER_SHUTTING_DOWN)

        /** Decodes a notification; unknown methods become [Unknown]. */
        fun parse(method: String, params: JsonElement?): ServerNotification {
            val p = params ?: kotlinx.serialization.json.JsonObject(emptyMap())
            return when (method) {
                STREAM_BATCH -> Batch(AasJson.decodeFromJsonElement(StreamBatch.serializer(), p))
                HEARTBEAT -> Beat(AasJson.decodeFromJsonElement(Heartbeat.serializer(), p))
                CONNECTION_REPLACED -> Replaced
                SERVER_SHUTTING_DOWN -> ShuttingDown(AasJson.decodeFromJsonElement(ServerShuttingDown.serializer(), p))
                else -> Unknown(method, params)
            }
        }

        /** Params of a notification (for re-encoding in tests and tools). */
        fun paramsOf(n: ServerNotification): JsonElement? = when (n) {
            is Batch -> AasJson.encodeToJsonElement(StreamBatch.serializer(), n.batch)
            is Beat -> AasJson.encodeToJsonElement(Heartbeat.serializer(), n.heartbeat)
            Replaced -> AasJson.encodeToJsonElement(ConnectionReplaced.serializer(), ConnectionReplaced)
            is ShuttingDown -> AasJson.encodeToJsonElement(ServerShuttingDown.serializer(), n.info)
            is Unknown -> n.params
        }
    }
}
