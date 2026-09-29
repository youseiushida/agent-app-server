package dev.aas.android.sync

import dev.aas.android.protocol.AasJson
import dev.aas.android.protocol.CommandAction
import dev.aas.android.protocol.Disposition
import dev.aas.android.protocol.Empty
import dev.aas.android.protocol.ErrorKind
import dev.aas.android.protocol.FsListParams
import dev.aas.android.protocol.Methods
import dev.aas.android.protocol.RpcException
import dev.aas.android.protocol.TurnStartResult
import dev.aas.android.protocol.WorkspaceSnapshotResult
import kotlinx.coroutines.async
import kotlinx.coroutines.delay
import kotlinx.coroutines.withTimeout
import kotlinx.serialization.json.JsonObject
import kotlinx.serialization.json.JsonPrimitive
import kotlinx.serialization.json.jsonArray
import kotlinx.serialization.json.jsonObject
import kotlinx.serialization.json.jsonPrimitive
import java.util.concurrent.CopyOnWriteArrayList
import java.util.concurrent.atomic.AtomicInteger
import kotlin.test.Test
import kotlin.test.assertEquals
import kotlin.test.assertFailsWith
import kotlin.test.assertIs
import kotlin.test.assertTrue

/** The outbox (protocol.md §1.2, §7.4) and the read-only call path. */
class SyncEngineOutboxTest {
    private val emptySnapshot = WorkspaceSnapshotResult(emptyList(), emptyList(), emptyList(), emptyList(), emptyList(), 0)

    private fun started(turnId: String) = AasJson.encodeToJsonElement(TurnStartResult.serializer(), TurnStartResult(Disposition.Started, turnId = turnId))

    private fun textOf(msg: dev.aas.android.protocol.RpcMessage) =
        msg.params!!.jsonObject["input"]!!.jsonArray[0].jsonObject["text"]!!.jsonPrimitive.content

    @Test
    fun anEntryIsCommittedBeforeItsFrameIsSent() = withFixture { f ->
        f.server.snapshot = emptySnapshot
        val persistedWhenReceived = CopyOnWriteArrayList<Boolean>()
        f.server.onRequest = { _, msg ->
            val crid = msg.params!!.jsonObject["clientRequestId"]!!.jsonPrimitive.content
            persistedWhenReceived += f.store.state.value.outbox.any { it.clientRequestId == crid && it.method == "turn/start" }
            started("trn_1")
        }
        f.connect()
        f.awaitOnline()
        val result = f.engine.mutate(Methods.TurnStart, turnStart("thr_1", "hi"))
        assertEquals("trn_1", result.turnId)
        assertEquals(listOf(true), persistedWhenReceived.toList())
        val crid = crid(f.server.requestsFor("turn/start").single())
        assertTrue(Regex("[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}").matches(crid), "UUID: $crid")
        eventually(what = "outbox empty") { f.store.state.value.outbox.takeIf { it.isEmpty() } }
        assertEquals(0, f.engine.status.value.pendingOutbox)
        eventually(what = "result") { f.results.filterIsInstance<OutboxResult.Succeeded>().firstOrNull() }
    }

    @Test
    fun aLostResponseIsResentWithTheSameRequestIdOnTheNextConnection() = withFixture { f ->
        f.server.snapshot = emptySnapshot
        val calls = AtomicInteger(0)
        f.server.onRequest = { conn, _ ->
            if (calls.incrementAndGet() == 1) {
                conn.kill() // the response is lost
                null
            } else {
                started("trn_9")
            }
        }
        f.connect()
        f.awaitOnline()
        assertEquals("trn_9", f.engine.mutate(Methods.TurnStart, turnStart("thr_1", "hi")).turnId)
        val sent = f.server.requestsFor("turn/start")
        assertEquals(2, sent.size)
        assertEquals(1, sent.map(::crid).toSet().size, "the resend reuses the clientRequestId")
        assertTrue(sent[0].first != sent[1].first, "resent on a new connection")
        // The resend waited for the setup of the new connection (protocol.md §2, step 5).
        val secondSubscribe = f.server.requests.indexOfFirst { it.first == 1 && it.second.method == "subscribe" }
        assertTrue(f.server.requests.indexOf(sent[1]) > secondSubscribe)
        assertTrue(f.store.state.value.outbox.isEmpty())
    }

    @Test
    fun entriesAreResentInTheOrderTheyWereMade() = withFixture { f ->
        f.server.snapshot = emptySnapshot
        val order = CopyOnWriteArrayList<String>()
        val answering = AtomicInteger(0)
        f.server.onRequest = { _, msg ->
            order += textOf(msg)
            // Nothing is answered on the first connection.
            if (answering.get() == 0) null else started("trn_${textOf(msg)}")
        }
        // Queued while offline (no credentials yet).
        f.engine.start()
        for (text in listOf("one", "two", "three")) f.engine.enqueue(Methods.TurnStart, turnStart("thr_1", text))
        assertEquals(3, f.engine.status.value.pendingOutbox)
        f.connect()
        f.awaitOnline()
        eventually(what = "first sent") { order.takeIf { it.isNotEmpty() } }
        delay(200)
        assertEquals(listOf("one"), order.toList(), "one at a time per thread: the second waits for the first's answer")
        answering.set(1)
        f.server.lastConnection.kill()
        eventually(what = "all answered") { f.store.state.value.outbox.takeIf { it.isEmpty() } }
        assertEquals(listOf("one", "one", "two", "three"), order.toList())
        val byText = f.server.requestsFor("turn/start").groupBy({ textOf(it.second) }, ::crid)
        assertEquals(1, byText.getValue("one").toSet().size, "same clientRequestId on the resend")
    }

    @Test
    fun definitiveErrorsAreFinalOthersAreRetried() = withFixture(EngineFixture(config = TEST_CONFIG.copy(outboxRetryBaseMs = 30, outboxRetryCapMs = 60))) { f ->
        f.server.snapshot = emptySnapshot
        val draining = AtomicInteger(2)
        f.server.onRequest = { _, msg ->
            when {
                msg.params.toString().contains("thr_gone") -> FakeServer.rpcError(ErrorKind.NotFound)
                draining.getAndDecrement() > 0 -> FakeServer.rpcError(ErrorKind.Draining)
                else -> AasJson.encodeToJsonElement(TurnStartResult.serializer(), TurnStartResult(Disposition.Queued, queuedId = "que_1"))
            }
        }
        f.connect()
        f.awaitOnline()
        val failure = assertFailsWith<RpcException> { f.engine.mutate(Methods.TurnStart, turnStart("thr_gone", "x")) }
        assertEquals(ErrorKind.NotFound, failure.kind)
        assertEquals(1, f.server.requestsFor("turn/start").size, "a definitive error is not retried")
        val ok = f.engine.mutate(Methods.TurnStart, turnStart("thr_1", "y"))
        assertEquals(Disposition.Queued, ok.disposition)
        val attempts = f.server.requestsFor("turn/start").filter { it.second.params.toString().contains("thr_1") }
        assertEquals(3, attempts.size, "two draining errors, then success")
        assertEquals(1, attempts.map(::crid).toSet().size)
        assertTrue(f.store.state.value.outbox.isEmpty())
        assertEquals(1, f.server.connections.size, "non-definitive errors do not reconnect")
    }

    @Test
    fun everyErrorKindIsClassifiedLikeTheProtocolTable() {
        val definitive = ErrorKind.entries.filter { it.definitive && it != ErrorKind.Unknown }.map { it.wire }.toSet()
        assertEquals(
            setOf(
                "parseError", "invalidRequest", "methodNotFound", "invalidParams", "notFound", "invalidState",
                "capabilityUnsupported", "idempotencyKeyReused", "pathNotAllowed", "protocolVersionUnsupported",
                "alreadyExists", "payloadTooLarge", "sessionSwitchingCommand",
            ),
            definitive,
        )
    }

    @Test
    fun aRetryingThreadDoesNotHoldBackOthers() = withFixture(EngineFixture(config = TEST_CONFIG.copy(outboxRetryBaseMs = 60_000, outboxRetryCapMs = 60_000))) { f ->
        f.server.snapshot = emptySnapshot
        f.server.onRequest = { _, msg ->
            if (msg.params.toString().contains("thr_busy")) FakeServer.rpcError(ErrorKind.HarnessUnavailable) else started("trn_ok")
        }
        f.connect()
        f.awaitOnline()
        f.engine.enqueue(Methods.TurnStart, turnStart("thr_busy", "a"))
        f.engine.enqueue(Methods.TurnStart, turnStart("thr_busy", "b"))
        assertEquals("trn_ok", f.engine.mutate(Methods.TurnStart, turnStart("thr_free", "c")).turnId)
        val left = eventually(what = "retry bookkeeping") { f.store.state.value.outbox.takeIf { it.size == 2 && it[0].failures == 1 } }
        assertTrue(left[0].lastError!!.contains("harnessUnavailable"), left[0].lastError)
        assertTrue(left[0].nextAttemptAtMs > System.currentTimeMillis())
        assertEquals(0, left[1].failures, "the second entry of the thread waits behind the first")
        assertEquals(1, f.server.requestsFor("turn/start").count { it.second.params.toString().contains("thr_busy") })
        assertEquals(2, f.engine.status.value.pendingOutbox)
        assertEquals(2, f.engine.outbox.value.size)
    }

    @Test
    fun aMutationMadeOfflineIsSentOnceConnected() = withFixture { f ->
        f.server.snapshot = emptySnapshot
        f.server.onRequest = { _, _ -> started("trn_1") }
        f.engine.start()
        val pending = f.scope.async { f.engine.mutate(Methods.TurnStart, turnStart("thr_1", "x")) }
        eventually(what = "persisted") { f.store.state.value.outbox.firstOrNull() }
        eventually(what = "visible to the thread") { f.engine.outbox.value.firstOrNull()?.takeIf { it.threadId == "thr_1" } }
        assertEquals(0, f.server.requestsFor("turn/start").size)
        f.connect()
        assertEquals("trn_1", withTimeout(10_000) { pending.await() }.turnId)
    }

    @Test
    fun resettingLocalDataDiscardsTheOutboxAndReleasesWaiters() = withFixture { f ->
        f.engine.start()
        val pending = f.scope.async { runCatching { f.engine.mutate(Methods.TurnStart, turnStart("thr_1", "x")) } }
        eventually(what = "persisted") { f.store.state.value.outbox.firstOrNull() }
        f.store.transaction { it.upsertProject(Samples.project("prj_1")); it.setEpoch("e") }
        f.engine.resetLocalData()
        assertIs<OutboxClearedException>(withTimeout(5_000) { pending.await() }.exceptionOrNull())
        assertEquals(InMemorySyncStore.State(), f.store.state.value)
        eventually(what = "discarded result") { f.results.filterIsInstance<OutboxResult.Discarded>().firstOrNull() }
        assertTrue(f.engine.outbox.value.isEmpty())
        assertEquals(0, f.engine.status.value.pendingOutbox)
    }

    @Test
    fun anEntryTooLargeForTheServerFailsWithoutEndlessResends() = withFixture { f ->
        f.server.snapshot = emptySnapshot
        f.server.maxClientFrameBytes = 2_000
        f.server.maxTransportFrameBytes = 8_000
        f.server.onRequest = { _, msg -> started("trn_${textOf(msg).length}") }
        f.connect()
        f.awaitOnline()
        // Between the frame limit and the transport limit: the server answers payloadTooLarge.
        val medium = assertFailsWith<RpcException> { f.engine.mutate(Methods.TurnStart, turnStart("thr_1", "m".repeat(4_000))) }
        assertEquals(ErrorKind.PayloadTooLarge, medium.kind)
        // Beyond the transport limit: the server closes with 1009. The entry fails definitively
        // and is never sent again; the connection comes back and other requests go through.
        val small = f.engine.enqueue(Methods.TurnStart, turnStart("thr_2", "small"))
        val huge = assertFailsWith<RpcException> { f.engine.mutate(Methods.TurnStart, turnStart("thr_1", "h".repeat(20_000))) }
        assertEquals(ErrorKind.PayloadTooLarge, huge.kind)
        f.awaitOnline()
        eventually(what = "the small entry answered") { f.results.firstOrNull { it.entry.clientRequestId == small && it is OutboxResult.Succeeded } }
        delay(300)
        assertEquals(1, f.server.oversizedFrames.size, "the frame that closed the connection was not resent")
        assertTrue(f.store.state.value.outbox.isEmpty())
        assertTrue(f.server.connections.size >= 2, "the 1009 close ended the first connection")
    }

    @Test
    fun readOnlyCallsNeedASessionAndRespectTheFrameLimit() = withFixture { f ->
        assertFailsWith<NotConnectedException> { f.engine.query(Methods.ServerStatus, Empty) }
        f.server.snapshot = emptySnapshot
        f.server.maxClientFrameBytes = 500
        f.server.onRequest = { _, msg ->
            if (msg.method == "server/status") {
                AasJson.parseToJsonElement("""{"uptimeMs":1,"runningProcesses":0,"runningTurns":0,"draining":false,"preventSleepWhileRunning":true}""")
            } else {
                JsonObject(emptyMap())
            }
        }
        f.connect()
        f.awaitOnline()
        assertEquals(true, f.engine.query(Methods.ServerStatus, Empty).preventSleepWhileRunning)
        val tooBig = assertFailsWith<RpcException> { f.engine.query(Methods.FsList, FsListParams("x".repeat(1_000))) }
        assertEquals(ErrorKind.PayloadTooLarge, tooBig.kind)
        assertEquals(0, f.server.requestsFor("fs/list").size, "not sent")
        assertFailsWith<IllegalArgumentException> { f.engine.query(Methods.TurnStart, turnStart("thr_1", "x")("c")) }
        assertEquals(1, f.server.connections.size)
    }

    @Test
    fun commandActionsAddThreadIdAndGoThroughTheOutboxWhenTheyChangeState() = withFixture { f ->
        f.server.snapshot = emptySnapshot
        f.server.onRequest = { _, _ -> JsonObject(mapOf("ok" to JsonPrimitive(true))) }
        f.connect()
        f.awaitOnline()
        f.engine.runCommand(CommandAction.Method("thread/stop"), "thr_1")
        val stop = f.server.requestsFor("thread/stop").single().second.params!!.jsonObject
        assertEquals("thr_1", stop["threadId"]!!.jsonPrimitive.content)
        assertTrue(stop.containsKey("clientRequestId"))
        f.engine.runCommand(CommandAction.Method("thread/diff", JsonObject(mapOf("scope" to JsonObject(mapOf("kind" to JsonPrimitive("thread")))))), "thr_1")
        val diff = f.server.requestsFor("thread/diff").single().second.params!!.jsonObject
        assertEquals("thr_1", diff["threadId"]!!.jsonPrimitive.content)
        assertTrue(!diff.containsKey("clientRequestId"), "read-only methods are plain calls")
        f.engine.runCommand(CommandAction.Method("thread/somethingNew"), "thr_1")
        assertTrue(f.server.requestsFor("thread/somethingNew").single().second.params!!.jsonObject.containsKey("clientRequestId"))
    }
}
