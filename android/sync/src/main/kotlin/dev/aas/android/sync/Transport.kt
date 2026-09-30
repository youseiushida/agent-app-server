package dev.aas.android.sync

import dev.aas.android.protocol.AasJson
import dev.aas.android.protocol.ErrorKind
import dev.aas.android.protocol.RpcError
import dev.aas.android.protocol.RpcException
import dev.aas.android.protocol.RpcMessage
import kotlinx.coroutines.CompletableDeferred
import kotlinx.coroutines.Deferred
import kotlinx.coroutines.channels.Channel
import kotlinx.coroutines.channels.ReceiveChannel
import kotlinx.coroutines.withTimeoutOrNull
import kotlinx.serialization.json.JsonElement
import kotlinx.serialization.json.JsonNull
import kotlinx.serialization.json.JsonPrimitive
import kotlinx.serialization.json.buildJsonObject
import kotlinx.serialization.json.longOrNull
import kotlinx.serialization.json.put
import okhttp3.OkHttpClient
import okhttp3.Request
import okhttp3.Response
import okhttp3.WebSocket
import okhttp3.WebSocketListener
import okio.utf8Size
import java.util.concurrent.ConcurrentHashMap
import java.util.concurrent.atomic.AtomicLong

/** Something the socket reported, in order. */
internal sealed interface WsEvent {
    data object Open : WsEvent

    data class Text(val text: String) : WsEvent

    /** The server sent a close frame. */
    data class Closing(val code: Int, val reason: String) : WsEvent

    /** The socket failed (network error, refused upgrade with [httpStatus], or cancelled). */
    data class Failure(val error: Throwable, val httpStatus: Int?) : WsEvent
}

/**
 * One OkHttp WebSocket, exposed as a channel of [WsEvent]s. Only the session that opened it
 * reads [events]: once that session ended, nothing this socket reports (a close code sent to an
 * old socket, say) reaches the engine.
 */
internal class WsConnection private constructor(private val tap: WireTap?) {
    private val channel = Channel<WsEvent>(Channel.UNLIMITED)
    private val _released = CompletableDeferred<Unit>()
    private lateinit var ws: WebSocket

    val events: ReceiveChannel<WsEvent> get() = channel

    /**
     * Completes when OkHttp reported the socket's end (`onClosed` or `onFailure`, which also
     * follows [cancel]) and the TCP socket is closed: the socket holds no connection any more.
     */
    val released: Deferred<Unit> get() = _released

    private val listener = object : WebSocketListener() {
        override fun onOpen(webSocket: WebSocket, response: Response) {
            channel.trySend(WsEvent.Open)
        }

        override fun onMessage(webSocket: WebSocket, text: String) {
            tap?.received(text)
            channel.trySend(WsEvent.Text(text))
        }

        override fun onClosing(webSocket: WebSocket, code: Int, reason: String) {
            channel.trySend(WsEvent.Closing(code, reason))
            webSocket.close(NORMAL_CLOSURE, null)
        }

        override fun onClosed(webSocket: WebSocket, code: Int, reason: String) {
            channel.close()
            release(webSocket)
        }

        override fun onFailure(webSocket: WebSocket, t: Throwable, response: Response?) {
            channel.trySend(WsEvent.Failure(t, response?.code))
            response?.close()
            channel.close()
            release(webSocket)
        }
    }

    /**
     * Reports the end once the TCP socket is closed. OkHttp calls `onFailure` (and `onClosed`)
     * before it closes the socket itself, so a socket the peer ended (EOF, a reset) would still be
     * open for a moment after the report, and the engine could open the next one meanwhile.
     * Cancelling closes it now (after the engine's own [cancel] it is already closed; cancelling
     * twice does nothing).
     */
    private fun release(webSocket: WebSocket) {
        webSocket.cancel()
        _released.complete(Unit)
    }

    /** Queues a text frame; `false` when the socket is closing or gone. */
    fun send(text: String): Boolean {
        val queued = ws.send(text)
        if (queued) tap?.sent(text)
        return queued
    }

    /** Drops the socket at once (no close handshake, which a dead path never completes). */
    fun cancel() = ws.cancel()

    companion object {
        const val NORMAL_CLOSURE = 1000

        fun open(http: OkHttpClient, request: Request, tap: WireTap?): WsConnection {
            val connection = WsConnection(tap)
            connection.ws = http.newWebSocket(request, connection.listener)
            return connection
        }
    }
}

/**
 * JSON-RPC requests over one [WsConnection]: ids increase per connection (protocol.md §1.1),
 * responses are matched by id, and every pending call fails with [ConnectionLostException]
 * when the connection ends.
 */
internal class RpcConnection(private val ws: WsConnection, private val callTimeoutMs: Long) {
    private val nextId = AtomicLong(1)
    private val pending = ConcurrentHashMap<Long, CompletableDeferred<JsonElement>>()

    @Volatile
    private var closedReason: String? = null

    /** `policy.maxClientFrameBytes` from `initialize`; `null` before it answered. */
    @Volatile
    var maxClientFrameBytes: Long? = null

    /**
     * Sends a request and waits for its result.
     *
     * With [enforceFrameLimit], a frame larger than the server's `maxClientFrameBytes` is not
     * sent and fails with a local `payloadTooLarge` — the answer the server would give (read-only
     * calls). Outbox requests pass `false`: the server answers them itself, so a frame that
     * makes it close the connection (1009) can be attributed to exactly that request.
     *
     * @throws RpcException the server (or the frame limit) answered with an error.
     * @throws ConnectionLostException the connection ended before the response arrived.
     * @throws CallTimeoutException no response within the call timeout.
     */
    suspend fun call(method: String, params: JsonElement, enforceFrameLimit: Boolean): JsonElement {
        closedReason?.let { throw ConnectionLostException(it) }
        val id = nextId.getAndIncrement()
        val text = AasJson.encodeToString(RpcMessage.serializer(), RpcMessage.request(id, method, params))
        val size = text.utf8Size()
        val limit = maxClientFrameBytes
        if (size > OKHTTP_MAX_FRAME_BYTES || (enforceFrameLimit && limit != null && size > limit)) {
            throw RpcException(localPayloadTooLarge(method, size, limit))
        }
        val deferred = CompletableDeferred<JsonElement>()
        pending[id] = deferred
        try {
            if (!ws.send(text)) throw ConnectionLostException("the connection is closing")
            // A close between the check above and registering must not leave the call waiting.
            closedReason?.let { throw ConnectionLostException(it) }
            return withTimeoutOrNull(callTimeoutMs) { deferred.await() } ?: throw CallTimeoutException(method, callTimeoutMs)
        } finally {
            pending.remove(id)
        }
    }

    /** Routes a response to its caller (responses to unknown ids are ignored). */
    fun onResponse(msg: RpcMessage) {
        val id = (msg.id as? JsonPrimitive)?.longOrNull ?: return
        val deferred = pending.remove(id) ?: return
        val error = msg.error
        if (error != null) deferred.completeExceptionally(RpcException(error)) else deferred.complete(msg.result ?: JsonNull)
    }

    /** Fails every pending call; later calls fail at once. */
    fun close(reason: String) {
        closedReason = reason
        val all = pending.values.toList()
        pending.clear()
        all.forEach { it.completeExceptionally(ConnectionLostException(reason)) }
    }

    companion object {
        /**
         * OkHttp refuses to queue a message that would take its outgoing queue past 16 MiB and
         * closes the socket instead (`RealWebSocket.MAX_QUEUE_SIZE`). Such a frame can never be
         * sent by this client, so it fails like the server's `payloadTooLarge`.
         */
        const val OKHTTP_MAX_FRAME_BYTES: Long = 16L * 1024 * 1024

        /** The error for a frame this client does not send because it is too large. */
        fun localPayloadTooLarge(method: String, size: Long, limit: Long?): RpcError = RpcError(
            code = ErrorKind.PayloadTooLarge.code,
            message = "$method: the request is $size bytes" +
                (limit?.let { ", the server accepts at most $it" } ?: ", more than this client can send"),
            data = buildJsonObject {
                put("kind", ErrorKind.PayloadTooLarge.wire)
                put("local", true)
            },
        )
    }
}
