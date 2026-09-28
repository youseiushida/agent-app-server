package dev.aas.android.sync

import dev.aas.android.protocol.AasJson
import dev.aas.android.protocol.ClientPolicy
import dev.aas.android.protocol.DeviceInfo
import dev.aas.android.protocol.ErrorKind
import dev.aas.android.protocol.Event
import dev.aas.android.protocol.EventEnvelope
import dev.aas.android.protocol.InitializeParams
import dev.aas.android.protocol.InitializeResult
import dev.aas.android.protocol.Methods
import dev.aas.android.protocol.RpcError
import dev.aas.android.protocol.RpcMessage
import dev.aas.android.protocol.ServerInfo
import dev.aas.android.protocol.StreamBatch
import dev.aas.android.protocol.SubscribeParams
import dev.aas.android.protocol.SubscribeResult
import dev.aas.android.protocol.SubscriptionState
import dev.aas.android.protocol.SubscriptionStatus
import dev.aas.android.protocol.ThreadReadParams
import dev.aas.android.protocol.ThreadReadResult
import dev.aas.android.protocol.WorkspaceSnapshotResult
import kotlinx.serialization.json.JsonElement
import kotlinx.serialization.json.JsonObject
import kotlinx.serialization.json.JsonPrimitive
import kotlinx.serialization.json.buildJsonObject
import kotlinx.serialization.json.put
import mockwebserver3.Dispatcher
import mockwebserver3.MockResponse
import mockwebserver3.MockWebServer
import mockwebserver3.RecordedRequest
import okhttp3.Response
import okhttp3.WebSocket
import okhttp3.WebSocketListener
import okhttp3.internal.ws.RealWebSocket
import okio.utf8Size
import java.io.IOException
import java.util.concurrent.ConcurrentHashMap
import java.util.concurrent.CopyOnWriteArrayList
import java.util.concurrent.atomic.AtomicInteger

/**
 * A scripted agent-app-server on real loopback sockets (MockWebServer): per-stream event logs,
 * `initialize`, `workspace/snapshot`, `subscribe` (replaying from `after`), `unsubscribe`,
 * `thread/read`, frame limits (`payloadTooLarge` answers and 1009 closes), and hooks that let a
 * test decide how every other request is answered.
 */
class FakeServer(private val token: String = "tok") : AutoCloseable {
    private val server = MockWebServer()
    private val connIndex = AtomicInteger(0)

    @Volatile
    var epoch: String = "epoch-1"

    @Volatile
    var clientTimeoutMs: Long = 45_000

    @Volatile
    var maxClientFrameBytes: Long = 1L shl 20

    /** Frames larger than this close the connection with 1009 (the server's transport limit). */
    @Volatile
    var maxTransportFrameBytes: Long = 16L shl 20

    @Volatile
    var snapshot: WorkspaceSnapshotResult = WorkspaceSnapshotResult(emptyList(), emptyList(), emptyList(), emptyList(), emptyList(), 0)

    /** Answers `initialize` with an error while it returns one (argument: 0-based initialize count). */
    @Volatile
    var initializeError: (Int) -> ErrorKind? = { null }

    /** Answers `initialize` with this protocol version. */
    @Volatile
    var protocolVersion: Int = 1

    val threadReads = ConcurrentHashMap<String, ThreadReadResult>()
    val logs = ConcurrentHashMap<String, MutableList<EventEnvelope>>()
    val connections = CopyOnWriteArrayList<Conn>()

    /** Streams `subscribe` reports as `notFound`. */
    val notFoundStreams: MutableSet<String> = ConcurrentHashMap.newKeySet()

    /**
     * Heads reported instead of the log's: below the log's, a server that lost events; above
     * it, one whose retention removed the stream's last events (the log then ends before the
     * head, and a subscription from before the head gets one empty batch, like the server's).
     */
    val reportedHeads = ConcurrentHashMap<String, Long>()

    /** Sizes of frames that exceeded [maxTransportFrameBytes] (each closed its connection with 1009). */
    val oversizedFrames = CopyOnWriteArrayList<Long>()

    /** Every request received: connection index to message. */
    val requests = CopyOnWriteArrayList<Pair<Int, RpcMessage>>()
    private val initializes = AtomicInteger(0)

    /**
     * Answers requests the server does not handle itself (mutations and other methods): return
     * a result [JsonElement], an [RpcError], or `null` to not answer (e.g. after dropping the
     * connection).
     */
    @Volatile
    var onRequest: (Conn, RpcMessage) -> Any? = { _, _ -> JsonObject(emptyMap()) }

    /**
     * Like the real server: a new connection of the device replaces the older ones still open
     * here (`connection/replaced`, then close code 4000), also those the client already gave up
     * but whose end the server has not noticed yet.
     */
    @Volatile
    var replaceOlderConnections = false

    /** Called for every request before the built-in handling; return true to consume it. */
    @Volatile
    var intercept: (Conn, RpcMessage) -> Boolean = { _, _ -> false }

    init {
        server.dispatcher = object : Dispatcher() {
            override fun dispatch(request: RecordedRequest): MockResponse {
                if (request.headers["Authorization"] != "Bearer $token") {
                    return MockResponse.Builder().code(401).body("""{"kind":"unauthorized","message":"bad token"}""").build()
                }
                val conn = Conn(connIndex.getAndIncrement())
                return MockResponse.Builder().webSocketUpgrade(conn.listener).build()
            }
        }
        server.start()
    }

    val wsUrl: String get() = server.url("/v1/ws").toString().replaceFirst("http", "ws")

    fun log(stream: String): MutableList<EventEnvelope> = logs.getOrPut(stream) { CopyOnWriteArrayList() }

    fun head(stream: String): Long = logs[stream]?.maxOfOrNull { it.seq } ?: 0

    /** Appends an event to a stream's log (sent on subscription or [Conn.pushNew]). */
    fun append(stream: String, event: Event, seq: Long = head(stream) + 1, seqFrom: Long? = null): EventEnvelope {
        val env = EventEnvelope(seq, seqFrom, 1_000L + seq, event)
        log(stream) += env
        return env
    }

    fun requestsFor(method: String): List<Pair<Int, RpcMessage>> = requests.filter { it.second.method == method }

    val lastConnection: Conn get() = connections.last()

    override fun close() {
        connections.forEach { it.kill() }
        server.close()
    }

    inner class Conn(val index: Int) {
        @Volatile
        var ws: WebSocket? = null

        @Volatile
        var closed = false

        /** Streams subscribed on this connection with the last seq sent. */
        val subscribed = ConcurrentHashMap<String, Long>()

        val listener = object : WebSocketListener() {
            override fun onOpen(webSocket: WebSocket, response: Response) {
                ws = webSocket
                if (replaceOlderConnections) {
                    for (older in connections) {
                        if (older.closed) continue
                        older.notify("connection/replaced", JsonObject(emptyMap()))
                        older.closeWith(4000, "replaced by a newer connection")
                    }
                }
                connections += this@Conn
            }

            override fun onMessage(webSocket: WebSocket, text: String) {
                val size = text.utf8Size()
                if (size > maxTransportFrameBytes) {
                    oversizedFrames += size
                    webSocket.close(1009, "message too big")
                    return
                }
                val msg = AasJson.decodeFromString(RpcMessage.serializer(), text)
                requests += index to msg
                if (size > maxClientFrameBytes) {
                    error(msg, ErrorKind.PayloadTooLarge)
                    return
                }
                handle(msg)
            }

            override fun onClosing(webSocket: WebSocket, code: Int, reason: String) {
                closed = true
                webSocket.close(1000, null)
            }

            override fun onClosed(webSocket: WebSocket, code: Int, reason: String) {
                closed = true
            }

            override fun onFailure(webSocket: WebSocket, t: Throwable, response: Response?) {
                closed = true
            }
        }

        private fun handle(msg: RpcMessage) {
            if (intercept(this, msg)) return
            when (msg.method) {
                Methods.Initialize.name -> {
                    val p = AasJson.decodeFromJsonElement(Methods.Initialize.params, msg.params!!)
                    val failure = initializeError(initializes.getAndIncrement())
                    if (failure != null) error(msg, failure) else respond(msg, AasJson.encodeToJsonElement(InitializeResult.serializer(), initResult(p)))
                }
                Methods.WorkspaceSnapshot.name -> respond(msg, AasJson.encodeToJsonElement(WorkspaceSnapshotResult.serializer(), snapshot))
                Methods.Subscribe.name -> {
                    val p = AasJson.decodeFromJsonElement(SubscribeParams.serializer(), msg.params!!)
                    val statuses = p.subscriptions.map { s ->
                        if (s.stream in notFoundStreams) {
                            SubscriptionStatus(s.stream, 0, SubscriptionState.NotFound)
                        } else {
                            SubscriptionStatus(s.stream, reportedHeads[s.stream] ?: head(s.stream), SubscriptionState.Ok)
                        }
                    }
                    respond(msg, AasJson.encodeToJsonElement(SubscribeResult.serializer(), SubscribeResult(statuses)))
                    for (s in p.subscriptions) {
                        if (s.stream in notFoundStreams) continue
                        subscribed[s.stream] = s.after
                        pushNew(s.stream)
                    }
                }
                Methods.Unsubscribe.name -> {
                    AasJson.decodeFromJsonElement(Methods.Unsubscribe.params, msg.params!!).streams.forEach { subscribed.remove(it) }
                    respond(msg, JsonObject(emptyMap()))
                }
                Methods.ThreadRead.name -> {
                    val p = AasJson.decodeFromJsonElement(ThreadReadParams.serializer(), msg.params!!)
                    val read = threadReads[p.threadId]
                    if (read == null) error(msg, ErrorKind.NotFound) else respond(msg, AasJson.encodeToJsonElement(ThreadReadResult.serializer(), read))
                }
                else -> when (val answer = onRequest(this, msg)) {
                    null -> Unit
                    is RpcError -> send(RpcMessage(id = msg.id, error = answer))
                    is JsonElement -> respond(msg, answer)
                    else -> throw IllegalArgumentException("unsupported answer $answer")
                }
            }
        }

        private fun initResult(p: InitializeParams) = InitializeResult(
            protocolVersion = protocolVersion,
            server = ServerInfo("fake", "0.0.0", "host", epoch),
            device = DeviceInfo("dev_1", "phone"),
            epochChanged = p.lastKnownEpoch != null && p.lastKnownEpoch != epoch,
            policy = ClientPolicy(
                heartbeatIntervalMs = 15_000,
                clientTimeoutMs = clientTimeoutMs,
                maxClientFrameBytes = maxClientFrameBytes,
                maxBlobBytes = 1L shl 20,
            ),
        )

        /**
         * Sends every logged event of [stream] after the subscription's position. When none is
         * left but the reported head is beyond the position (retention removed the last events),
         * sends one empty batch with that head instead (protocol.md §2.1).
         */
        fun pushNew(stream: String) {
            val after = subscribed[stream] ?: return
            val head = reportedHeads[stream] ?: head(stream)
            val events = log(stream).filter { it.seq > after }
            if (events.isEmpty()) {
                if (head > after) {
                    batch(StreamBatch(stream, head, emptyList()))
                    subscribed[stream] = head
                }
                return
            }
            batch(StreamBatch(stream, maxOf(head, events.maxOf { it.seq }), events))
            subscribed[stream] = events.maxOf { it.seq }
        }

        fun batch(batch: StreamBatch) {
            send(RpcMessage(method = "stream/batch", params = AasJson.encodeToJsonElement(StreamBatch.serializer(), batch)))
        }

        fun respond(msg: RpcMessage, result: JsonElement) = send(RpcMessage(id = msg.id, result = result))

        fun error(msg: RpcMessage, kind: ErrorKind) = send(RpcMessage(id = msg.id, error = rpcError(kind)))

        fun notify(method: String, params: JsonElement) = send(RpcMessage(method = method, params = params))

        fun send(msg: RpcMessage) {
            ws?.send(AasJson.encodeToString(RpcMessage.serializer(), msg))
        }

        /** Sends a frame verbatim (malformed payloads). */
        fun sendRaw(text: String) {
            ws?.send(text)
        }

        fun heartbeat(heads: Map<String, Long>) = notify("heartbeat", heartbeatParams(heads))

        /** Closes with a close frame, like the server does for `replaced`, `revoked`, 4003, 1009. */
        fun closeWith(code: Int, reason: String) {
            ws?.close(code, reason)
        }

        /**
         * Drops the TCP connection without a close handshake. (`WebSocket.cancel()` works only
         * on client sockets; a server-side socket is failed instead, which closes it at once.)
         */
        fun kill() {
            (ws as? RealWebSocket)?.failWebSocket(IOException("connection killed by the test"), null, false)
        }
    }

    companion object {
        fun rpcError(kind: ErrorKind, message: String = "error ${kind.wire}"): RpcError =
            RpcError(kind.code, message, buildJsonObject { put("kind", kind.wire) })

        fun heartbeatParams(heads: Map<String, Long> = emptyMap()): JsonElement = buildJsonObject {
            put("serverTime", JsonPrimitive(1L))
            put("heads", JsonObject(heads.mapValues { JsonPrimitive(it.value) }))
        }
    }
}
