package dev.aas.android.sync

import dev.aas.android.protocol.AasJson
import dev.aas.android.protocol.Disposition
import dev.aas.android.protocol.ErrorKind
import dev.aas.android.protocol.Methods
import dev.aas.android.protocol.RpcError
import dev.aas.android.protocol.RpcException
import dev.aas.android.protocol.ThreadStopParams
import dev.aas.android.protocol.TurnInterruptParams
import dev.aas.android.protocol.TurnStartResult
import dev.aas.android.protocol.WorkspaceSnapshotResult
import kotlinx.coroutines.CompletableDeferred
import kotlinx.coroutines.async
import kotlinx.coroutines.delay
import kotlinx.coroutines.withTimeout
import kotlinx.serialization.json.JsonObject
import kotlinx.serialization.json.JsonPrimitive
import kotlinx.serialization.json.buildJsonObject
import kotlinx.serialization.json.put
import java.util.concurrent.atomic.AtomicBoolean
import kotlin.test.Test
import kotlin.test.assertEquals
import kotlin.test.assertFailsWith
import kotlin.test.assertIs
import kotlin.test.assertTrue

/** Getting out of a stuck outbox lane: stop controls, discarding an entry, unknown error kinds. */
class SyncEngineOutboxControlTest {
    private val emptySnapshot = WorkspaceSnapshotResult(emptyList(), emptyList(), emptyList(), emptyList(), emptyList(), 0)

    private fun started(turnId: String) = AasJson.encodeToJsonElement(TurnStartResult.serializer(), TurnStartResult(Disposition.Started, turnId = turnId))

    /** Retries far in the future: an entry that failed once stays waiting for the whole test. */
    private val slowRetries = TEST_CONFIG.copy(outboxRetryBaseMs = 60_000, outboxRetryCapMs = 60_000)

    @Test
    fun stopControlsPassAnInputOfTheirThreadThatWaitsForItsRetry() = withFixture(EngineFixture(config = slowRetries)) { f ->
        f.server.snapshot = emptySnapshot
        // The steer keeps failing in the adapter (adapterError, not definitive).
        f.server.onRequest = { _, msg ->
            when (msg.method) {
                Methods.TurnStart.name -> FakeServer.rpcError(ErrorKind.AdapterError)
                Methods.TurnInterrupt.name -> JsonObject(mapOf("interrupted" to JsonPrimitive(true)))
                else -> JsonObject(emptyMap())
            }
        }
        f.connect()
        f.awaitOnline()
        val steer = f.engine.enqueue(Methods.TurnStart, turnStart("thr_1", "follow-up"))
        eventually(what = "waiting for its retry") { f.engine.outbox.value.firstOrNull { it.clientRequestId == steer && it.failures == 1 } }
        // Stop: sent at once although the lane's first entry waits.
        val interrupted = withTimeout(5_000) { f.engine.mutate(Methods.TurnInterrupt) { TurnInterruptParams(it, "thr_1") } }
        assertEquals(true, interrupted.interrupted)
        f.engine.enqueue(Methods.ThreadStop) { ThreadStopParams(it, "thr_1") }
        eventually(what = "stop sent") { f.server.requestsFor(Methods.ThreadStop.name).firstOrNull() }
        // Anything else of the thread keeps its place behind the waiting entry.
        f.engine.enqueue(Methods.TurnStart, turnStart("thr_1", "later"))
        delay(300)
        assertEquals(1, f.server.requestsFor(Methods.TurnStart.name).size, "the later input is not sent before the first")
        val order = f.server.requests.map { it.second.method }.filter { it in setOf(Methods.TurnStart.name, Methods.TurnInterrupt.name, Methods.ThreadStop.name) }
        assertEquals(listOf(Methods.TurnStart.name, Methods.TurnInterrupt.name, Methods.ThreadStop.name), order)
        assertEquals(listOf("follow-up", "later"), f.engine.outbox.value.filter { it.method == Methods.TurnStart.name }.map { (it.params["input"] as kotlinx.serialization.json.JsonArray).let { a -> (a[0] as JsonObject)["text"]!!.let { t -> (t as JsonPrimitive).content } } })
    }

    @Test
    fun aStopControlWaitsForAnEntryOnTheWire() = withFixture(EngineFixture(config = slowRetries)) { f ->
        f.server.snapshot = emptySnapshot
        val answerStart = CompletableDeferred<Unit>()
        f.server.intercept = { conn, msg ->
            if (msg.method == Methods.TurnStart.name) {
                // Answered only when the test says so: the request stays on the wire.
                f.scope.async {
                    answerStart.await()
                    conn.respond(msg, started("trn_1"))
                }
                true
            } else {
                false
            }
        }
        f.server.onRequest = { _, _ -> JsonObject(mapOf("interrupted" to JsonPrimitive(true))) }
        f.connect()
        f.awaitOnline()
        f.engine.enqueue(Methods.TurnStart, turnStart("thr_1", "x"))
        eventually(what = "on the wire") { f.server.requestsFor(Methods.TurnStart.name).firstOrNull() }
        f.engine.enqueue(Methods.TurnInterrupt) { TurnInterruptParams(it, "thr_1") }
        delay(300)
        assertEquals(0, f.server.requestsFor(Methods.TurnInterrupt.name).size, "the server handles one request per thread at a time anyway")
        answerStart.complete(Unit)
        eventually(what = "interrupt sent") { f.server.requestsFor(Methods.TurnInterrupt.name).firstOrNull() }
        eventually(what = "outbox empty") { f.engine.outbox.value.takeIf { it.isEmpty() } }
    }

    @Test
    fun aDiscardedEntryIsNeverSentAgainAndReleasesItsLane() = withFixture(EngineFixture(config = slowRetries)) { f ->
        f.server.snapshot = emptySnapshot
        f.server.onRequest = { _, msg ->
            if (msg.params.toString().contains("stale prompt")) FakeServer.rpcError(ErrorKind.HarnessUnavailable) else started("trn_ok")
        }
        f.connect()
        f.awaitOnline()
        val stale = f.engine.submit(Methods.TurnStart, turnStart("thr_1", "stale prompt"))
        val waiting = f.scope.async { runCatching { stale.await() } }
        eventually(what = "waiting for its retry") { f.engine.outbox.value.firstOrNull { it.failures == 1 } }
        val next = f.engine.enqueue(Methods.ThreadArchive) { dev.aas.android.protocol.ThreadArchiveParams(it, "thr_1", archived = true) }
        delay(200)
        assertEquals(0, f.server.requestsFor(Methods.ThreadArchive.name).size, "held back by the waiting input")

        // Right after its failure the send may still be finishing (then it says InFlight).
        assertEquals(OutboxDiscard.Discarded, eventually(what = "discarded") { f.engine.discardOutbox(stale.clientRequestId).takeIf { it != OutboxDiscard.InFlight } })
        assertIs<OutboxClearedException>(withTimeout(5_000) { waiting.await() }.exceptionOrNull())
        eventually(what = "discarded result") { f.results.filterIsInstance<OutboxResult.Discarded>().firstOrNull { it.entry.clientRequestId == stale.clientRequestId } }
        // The lane moves on; the discarded input is not sent again, not even after a reconnect.
        eventually(what = "archive sent") { f.server.requestsFor(Methods.ThreadArchive.name).firstOrNull() }
        eventually(what = "archive answered") { f.results.firstOrNull { it.entry.clientRequestId == next && it is OutboxResult.Succeeded } }
        f.server.lastConnection.kill()
        eventually(what = "reconnected") { f.server.connections.size.takeIf { it == 2 && f.engine.status.value.isOnline } }
        delay(200)
        assertEquals(1, f.server.requestsFor(Methods.TurnStart.name).size)
        assertTrue(f.store.state.value.outbox.isEmpty())
        assertEquals(OutboxDiscard.NotFound, f.engine.discardOutbox(stale.clientRequestId))
    }

    @Test
    fun anEntryOnTheWireCannotBeDiscarded() = withFixture { f ->
        f.server.snapshot = emptySnapshot
        val answer = CompletableDeferred<Unit>()
        f.server.intercept = { conn, msg ->
            if (msg.method == Methods.TurnStart.name) {
                f.scope.async {
                    answer.await()
                    conn.respond(msg, started("trn_1"))
                }
                true
            } else {
                false
            }
        }
        f.connect()
        f.awaitOnline()
        val pending = f.engine.submit(Methods.TurnStart, turnStart("thr_1", "x"))
        eventually(what = "on the wire") { f.server.requestsFor(Methods.TurnStart.name).firstOrNull() }
        assertEquals(OutboxDiscard.InFlight, f.engine.discardOutbox(pending.clientRequestId))
        answer.complete(Unit)
        assertEquals("trn_1", withTimeout(5_000) { pending.await() }.turnId)
    }

    @Test
    fun anOfflineEntryCanBeDiscardedBeforeItIsEverSent() = withFixture { f ->
        f.engine.start()
        val crid = f.engine.enqueue(Methods.TurnStart, turnStart("thr_1", "never mind"))
        assertEquals(OutboxDiscard.Discarded, f.engine.discardOutbox(crid))
        assertTrue(f.store.state.value.outbox.isEmpty())
        f.server.snapshot = emptySnapshot
        f.connect()
        f.awaitOnline()
        delay(200)
        assertEquals(0, f.server.requestsFor(Methods.TurnStart.name).size)
    }

    @Test
    fun anErrorKindThisClientDoesNotKnowIsFinal() = withFixture(EngineFixture(config = slowRetries)) { f ->
        f.server.snapshot = emptySnapshot
        val answered = AtomicBoolean(false)
        f.server.onRequest = { _, _ ->
            answered.set(true)
            RpcError(-32099, "a newer server's refusal", buildJsonObject { put("kind", "somethingNew") })
        }
        f.connect()
        f.awaitOnline()
        val failure = assertFailsWith<RpcException> { withTimeout(5_000) { f.engine.mutate(Methods.TurnStart, turnStart("thr_1", "x")) } }
        assertEquals(ErrorKind.Unknown, failure.kind)
        assertTrue(answered.get())
        assertTrue(f.store.state.value.outbox.isEmpty(), "not kept for a retry that would get the same answer")
        assertEquals(1, f.server.requestsFor(Methods.TurnStart.name).size)
    }
}
