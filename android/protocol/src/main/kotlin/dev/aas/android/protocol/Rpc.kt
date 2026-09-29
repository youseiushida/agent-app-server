package dev.aas.android.protocol

import kotlinx.serialization.Serializable
import kotlinx.serialization.json.JsonElement
import kotlinx.serialization.json.JsonObject
import kotlinx.serialization.json.JsonPrimitive
import kotlinx.serialization.json.contentOrNull

/** Any JSON-RPC 2.0 message (both directions). Classify with [kind]. */
@Serializable
data class RpcMessage(
    val jsonrpc: String = "2.0",
    val id: JsonElement? = null,
    val method: String? = null,
    val params: JsonElement? = null,
    val result: JsonElement? = null,
    val error: RpcError? = null,
) {
    enum class Kind { Request, Notification, Response, Invalid }

    val kind: Kind
        get() = when {
            jsonrpc != "2.0" -> Kind.Invalid
            id != null && method != null && result == null && error == null -> Kind.Request
            id == null && method != null && result == null && error == null -> Kind.Notification
            method == null && (result != null) != (error != null) -> Kind.Response
            else -> Kind.Invalid
        }

    companion object {
        fun request(id: Long, method: String, params: JsonElement) = RpcMessage(id = JsonPrimitive(id), method = method, params = params)
    }
}

/**
 * JSON-RPC error; `data.kind` carries the [ErrorKind] name, other `data` fields the details
 * (protocol.md §1.3). The detail accessors are `null` when the server did not send the field
 * (an older server, or a kind that does not carry it).
 */
@Serializable
data class RpcError(val code: Int, val message: String, val data: JsonElement? = null) {
    /** The error kind from `data.kind`, falling back to the code. */
    val kind: ErrorKind
        get() {
            val name = dataString("kind")
            return ErrorKind.entries.firstOrNull { it.wire == name } ?: ErrorKind.fromCode(code)
        }

    /** The harness the error is about: `data.harnessId` of `harnessUnavailable` and `adapterError`. */
    val harnessId: String? get() = dataString(DATA_HARNESS_ID)

    /**
     * Why the harness cannot be used (`data.reason` of `harnessUnavailable`), in the server's
     * words: shown to the user as it is, never interpreted (it is the probe's own text).
     */
    val reason: String? get() = dataString(DATA_REASON)

    /** The missing capability (`data.capability` of `capabilityUnsupported`). */
    val capability: String? get() = dataString(DATA_CAPABILITY)

    /**
     * The harness's own text of an `adapterError` (`data.detail`): without the daemon's English
     * lead-in and terminal escape sequences, shown verbatim after the app's own lead-in.
     */
    val detail: String? get() = dataString(DATA_DETAIL)

    /** The session-switching command a `sessionSwitchingCommand` refused (`data.command`, without the `/`). */
    val command: String? get() = dataString(DATA_COMMAND)

    /** A string field of `data`, or `null` when absent or not a string. */
    fun dataString(key: String): String? =
        ((data as? JsonObject)?.get(key) as? JsonPrimitive)?.takeIf { it.isString }?.contentOrNull

    companion object {
        const val DATA_HARNESS_ID = "harnessId"
        const val DATA_REASON = "reason"
        const val DATA_CAPABILITY = "capability"
        const val DATA_DETAIL = "detail"
        const val DATA_COMMAND = "command"
    }
}

/**
 * Error kinds of protocol v1. [definitive] errors never change on resend: the client drops the
 * request from its outbox. Others are kept and resent later (protocol.md §1.3).
 */
enum class ErrorKind(val wire: String, val code: Int, val definitive: Boolean) {
    ParseError("parseError", -32700, true),
    InvalidRequest("invalidRequest", -32600, true),
    MethodNotFound("methodNotFound", -32601, true),
    InvalidParams("invalidParams", -32602, true),
    Internal("internal", -32603, false),
    NotInitialized("notInitialized", -32000, false),
    Unauthorized("unauthorized", -32001, false),
    NotFound("notFound", -32002, true),
    InvalidState("invalidState", -32003, true),
    CapabilityUnsupported("capabilityUnsupported", -32004, true),

    /**
     * Not definitive: the server probes an unavailable harness again on its own and before every
     * request that needs it, so the same request can succeed later ([RpcError.harnessId],
     * [RpcError.reason]). The client does not resend it on a timer: it shows the reason and
     * resends when `harness/updated` reports the harness available (protocol.md §1.3).
     */
    HarnessUnavailable("harnessUnavailable", -32005, false),
    IdempotencyKeyReused("idempotencyKeyReused", -32006, true),
    PathNotAllowed("pathNotAllowed", -32007, true),
    RateLimited("rateLimited", -32008, false),
    ProtocolVersionUnsupported("protocolVersionUnsupported", -32009, true),
    AlreadyExists("alreadyExists", -32010, true),
    AdapterError("adapterError", -32011, false),
    PayloadTooLarge("payloadTooLarge", -32012, true),
    Draining("draining", -32013, false),

    /**
     * The input starts with a harness command that would move the thread's agent to another
     * native session ([RpcError.command], [RpcError.harnessId]): one thread is one native session.
     */
    SessionSwitchingCommand("sessionSwitchingCommand", -32014, true),

    /**
     * A kind this client does not know (a newer server). Treated as definitive: the client
     * cannot tell whether the server stored it for this `clientRequestId`, and if it did, every
     * resend would get the same answer while the request blocks the requests behind it in its
     * lane. The request leaves the outbox and the failure is shown, so the user can send again.
     */
    Unknown("unknown", 0, true),
    ;

    companion object {
        fun fromCode(code: Int): ErrorKind = entries.firstOrNull { it.code == code && it != Unknown } ?: Unknown
    }
}

/** A failed call, as seen by callers. */
class RpcException(val error: RpcError) : Exception("${error.kind.wire}: ${error.message}") {
    val kind: ErrorKind get() = error.kind
}

const val PROTOCOL_VERSION = 1

/** Param names the client fills in itself (idempotency keys, command action targets). */
object JsonKeys {
    const val CLIENT_REQUEST_ID = "clientRequestId"
    const val THREAD_ID = "threadId"
    const val PROJECT_ID = "projectId"
    const val INTERACTION_ID = "interactionId"
    const val HARNESS_ID = "harnessId"
    const val TASK_ID = "taskId"
    const val ITEM_ID = "itemId"
}
