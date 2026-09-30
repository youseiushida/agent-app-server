package dev.aas.android.sync

import dev.aas.android.protocol.AasJson
import dev.aas.android.protocol.Disposition
import dev.aas.android.protocol.ErrorKind
import dev.aas.android.protocol.Event
import dev.aas.android.protocol.HarnessListResult
import dev.aas.android.protocol.Methods
import dev.aas.android.protocol.RpcError
import dev.aas.android.protocol.RpcMessage
import dev.aas.android.protocol.ThreadCreateParams
import dev.aas.android.protocol.TurnInterruptParams
import dev.aas.android.protocol.TurnStartResult
import dev.aas.android.protocol.WORKSPACE_STREAM
import dev.aas.android.protocol.WorkspaceSnapshotResult
import kotlinx.coroutines.CompletableDeferred
import kotlinx.coroutines.async
import kotlinx.coroutines.delay
import kotlinx.serialization.json.JsonElement
import kotlinx.serialization.json.buildJsonObject
import kotlinx.serialization.json.jsonObject
import kotlinx.serialization.json.jsonPrimitive
import kotlinx.serialization.json.put
import java.util.concurrent.atomic.AtomicBoolean
import kotlin.test.Test
import kotlin.test.assertEquals
import kotlin.test.assertIs
import kotlin.test.assertTrue

/**
 * `harnessUnavailable` (protocol.md §1.3): not definitive, so the request stays in the outbox,
 * but it is not resent on a timer while the harness is known to be unavailable. It shows the
 * server's reason and is sent as soon as `harness/updated` (or a refresh) reports the harness
 * available.
 */
class SyncEngineHarnessTest {
    private val codexDown = Samples.harness("codex", available = false, reason = "not logged in")
    private val codexUp = Samples.harness("codex")

    private fun snapshot(vararg harnesses: dev.aas.android.protocol.Harness) = WorkspaceSnapshotResult(
        harnesses = harnesses.toList(),
        projects = listOf(Samples.project("prj_1")),
        threads = listOf(Samples.thread("thr_1", harnessId = "codex")),
        pendingInteractions = emptyList(),
        operations = emptyList(),
        head = 0,
    )

    private fun unavailable(harnessId: String? = "codex", reason: String? = "not logged in"): RpcError = RpcError(
        ErrorKind.HarnessUnavailable.code,
        "harness codex is unavailable",
        buildJsonObject {
            put("kind", ErrorKind.HarnessUnavailable.wire)
            harnessId?.let { put("harnessId", it) }
            reason?.let { put("reason", it) }
        },
    )

    private fun started(turnId: String): JsonElement =
        AasJson.encodeToJsonElement(TurnStartResult.serializer(), TurnStartResult(Disposition.Started, turnId = turnId))

    /** The server makes the harness available: the workspace stream says so. */
    private fun EngineFixture.harnessBack() {
        server.append(WORKSPACE_STREAM, Event.HarnessUpdated(codexUp))
        server.lastConnection.pushNew(WORKSPACE_STREAM)
    }

    private fun EngineFixture.sends(method: String) = server.requestsFor(method).size

    @Test
    fun aRequestForAnUnavailableHarnessWaitsForItInsteadOfRetrying() = withFixture { f ->
        f.server.snapshot = snapshot(codexDown)
        val available = AtomicBoolean(false)
        f.server.onRequest = { _, msg -> if (msg.method == "turn/start" && !available.get()) unavailable() else started("trn_1") }
        f.connect()
        f.awaitOnline()
        val answer = async { f.engine.mutate(Methods.TurnStart, turnStart("thr_1", "hello")) }
        val waiting = eventually(what = "waiting for codex") { f.engine.outbox.value.singleOrNull()?.takeIf { it.waitingForHarness != null } }
        assertEquals("codex", waiting.waitingForHarness)
        assertEquals("not logged in", waiting.lastError, "the server's reason, to show")
        assertEquals(1, waiting.failures)
        // Many retry delays (TEST_CONFIG caps them at 200 ms): nothing is resent meanwhile.
        delay(800)
        assertEquals(1, f.sends("turn/start"), "no silent retries while the harness is unavailable")
        assertTrue(!answer.isCompleted, "the caller keeps waiting (the request is not dropped)")
        // The user logged in on the PC; the server's probe publishes harness/updated.
        available.set(true)
        f.harnessBack()
        assertEquals("trn_1", answer.await().turnId)
        assertEquals(2, f.sends("turn/start"))
        assertEquals(1, f.server.requestsFor("turn/start").map(::crid).toSet().size, "resent with the same clientRequestId")
        eventually(what = "outbox empty") { f.engine.outbox.value.takeIf { it.isEmpty() } }
    }

    @Test
    fun aWaitingRequestHoldsItsLaneButNotTheStopControls() = withFixture { f ->
        f.server.snapshot = snapshot(codexDown)
        val available = AtomicBoolean(false)
        f.server.onRequest = { _, msg ->
            when (msg.method) {
                "turn/start" -> if (available.get()) started("trn_${f.sends("turn/start")}") else unavailable()
                "turn/interrupt" -> buildJsonObject { put("interrupted", false) }
                else -> buildJsonObject { }
            }
        }
        f.connect()
        f.awaitOnline()
        f.engine.enqueue(Methods.TurnStart, turnStart("thr_1", "first"))
        eventually(what = "waiting") { f.engine.outbox.value.firstOrNull()?.waitingForHarness }
        f.engine.enqueue(Methods.TurnStart, turnStart("thr_1", "second"))
        // Stopping the agent never depends on a harness coming back.
        f.engine.mutate(Methods.TurnInterrupt) { crid -> TurnInterruptParams(crid, "thr_1") }
        delay(300)
        assertEquals(1, f.sends("turn/start"), "the second input stays behind the first")
        available.set(true)
        f.harnessBack()
        eventually(what = "both sent") { f.engine.outbox.value.takeIf { it.isEmpty() } }
        val texts = f.server.requestsFor("turn/start").map { it.second.params!!.jsonObject["input"].toString() }
        assertEquals(3, texts.size)
        assertTrue(texts[1].contains("first") && texts[2].contains("second"), "order kept: $texts")
    }

    @Test
    fun aRefreshThatFindsTheHarnessSendsWaitingRequestsAtOnce() = withFixture { f ->
        f.server.snapshot = snapshot(codexDown)
        val available = AtomicBoolean(false)
        val refreshCall = CompletableDeferred<Pair<FakeServer.Conn, RpcMessage>>()
        f.server.onRequest = { conn, msg ->
            when (msg.method) {
                "harness/refresh" -> {
                    // Answered by the test below, while it checks the probing state.
                    refreshCall.complete(conn to msg)
                    null
                }
                "thread/create" -> if (available.get()) {
                    buildJsonObject { put("thread", AasJson.encodeToJsonElement(dev.aas.android.protocol.Thread.serializer(), Samples.thread("thr_2", harnessId = "codex"))) }
                } else {
                    unavailable()
                }
                else -> buildJsonObject { }
            }
        }
        f.connect()
        f.awaitOnline()
        val created = async { f.engine.mutate(Methods.ThreadCreate) { crid -> ThreadCreateParams(crid, "prj_1", "codex") } }
        eventually(what = "waiting") { f.engine.outbox.value.firstOrNull()?.waitingForHarness }
        val refresh = async { f.engine.refreshHarnesses("codex") }
        val (conn, msg) = refreshCall.await()
        assertEquals("codex", msg.params!!.jsonObject["harnessId"]!!.jsonPrimitive.content)
        assertEquals(setOf("codex"), f.engine.refreshingHarnesses.value, "probing while the refresh runs")
        available.set(true)
        // The answer arrives before the workspace event would.
        conn.respond(msg, AasJson.encodeToJsonElement(HarnessListResult.serializer(), HarnessListResult(listOf(codexUp))))
        assertEquals(listOf(codexUp), refresh.await())
        assertEquals(emptySet(), f.engine.refreshingHarnesses.value)
        assertEquals("thr_2", created.await().thread.id)
    }

    /**
     * The server refuses although the local list says available (its harness/updated may still
     * be on the way): the request is resent, but only on the retry delay, never in a loop. The
     * test waits for the resends themselves and measures their spacing with the engine's own
     * clock (not a window of wall time in which some number of sends must fit).
     */
    @Test
    fun whileTheWorkspaceShowsTheHarnessAvailableTheRetryDelayStillApplies() {
        val clock = object : Clock {
            // One monotonic time base for the engine and the server's record of arrivals.
            override fun nowMs(): Long = System.nanoTime() / 1_000_000

            override fun monotonicMs(): Long = nowMs()
        }
        val delayMs = 150L
        withFixture(EngineFixture(config = TEST_CONFIG.copy(outboxRetryBaseMs = delayMs, outboxRetryCapMs = delayMs), clock = clock)) { f ->
            f.server.snapshot = snapshot(codexUp)
            val arrivals = java.util.concurrent.CopyOnWriteArrayList<Long>()
            f.server.onRequest = { _, msg ->
                if (msg.method == "turn/start") {
                    arrivals += clock.nowMs()
                    unavailable()
                } else {
                    buildJsonObject { }
                }
            }
            f.connect()
            f.awaitOnline()
            f.engine.enqueue(Methods.TurnStart, turnStart("thr_1", "hello"))
            val sends = eventually(what = "two resends") { arrivals.toList().takeIf { it.size >= 3 } }
            val gaps = sends.zipWithNext { a, b -> b - a }
            assertTrue(gaps.all { it >= delayMs }, "resent on the retry delay only, not in a loop: $gaps")
            assertEquals("codex", f.engine.outbox.value.single().waitingForHarness)
            assertEquals(1, f.server.requestsFor("turn/start").map(::crid).toSet().size, "always the same clientRequestId")
        }
    }

    @Test
    fun anUnavailableHarnessTheClientCannotNameIsRetriedLikeOtherFailures() = withFixture { f ->
        f.server.snapshot = snapshot()
        val calls = java.util.concurrent.atomic.AtomicInteger()
        f.server.onRequest = { _, msg ->
            // No data.harnessId, and fs/mkdir names no harness: nothing to wait for.
            if (msg.method == "fs/mkdir" && calls.incrementAndGet() < 3) unavailable(harnessId = null) else buildJsonObject { put("path", "C:\\p\\x") }
        }
        f.connect()
        f.awaitOnline()
        val path = f.engine.mutate(Methods.FsMkdir) { crid -> dev.aas.android.protocol.FsMkdirParams(crid, "C:\\p\\x") }.path
        assertEquals("C:\\p\\x", path)
        assertEquals(3, f.sends("fs/mkdir"))
    }

    @Test
    fun theHarnessComesFromTheRequestWhenTheErrorDoesNotNameIt() = withFixture { f ->
        f.server.snapshot = snapshot(codexDown)
        f.server.onRequest = { _, msg -> if (msg.method == "turn/start") unavailable(harnessId = null, reason = null) else buildJsonObject { } }
        f.connect()
        f.awaitOnline()
        f.engine.enqueue(Methods.TurnStart, turnStart("thr_1", "hello"))
        val waiting = eventually(what = "waiting") { f.engine.outbox.value.firstOrNull()?.takeIf { it.waitingForHarness != null } }
        assertEquals("codex", waiting.waitingForHarness, "the thread's harness")
        assertEquals("harness codex is unavailable", waiting.lastError, "without a reason, the server's message")
    }

    @Test
    fun aWaitingRequestStillWaitsAfterTheAppRestarts() {
        val store = InMemorySyncStore()
        EngineFixture(store = store).use { first ->
            engineTest {
                first.server.snapshot = snapshot(codexDown)
                first.server.onRequest = { _, msg -> if (msg.method == "turn/start") unavailable() else buildJsonObject { } }
                first.connect()
                first.awaitOnline()
                first.engine.enqueue(Methods.TurnStart, turnStart("thr_1", "hello"))
                eventually(what = "waiting") { store.state.value.outbox.firstOrNull()?.waitingForHarness }
                first.engine.stop().join()
            }
        }
        // A new process with the same store: the wait is persisted, nothing is sent on connect.
        withFixture(EngineFixture(store = store)) { f ->
            f.server.snapshot = snapshot(codexDown)
            f.server.epoch = "epoch-1"
            f.server.onRequest = { _, msg -> if (msg.method == "turn/start") started("trn_1") else buildJsonObject { } }
            f.connect()
            f.awaitOnline()
            delay(500)
            assertEquals(0, f.sends("turn/start"))
            assertEquals("codex", f.engine.outbox.value.single().waitingForHarness)
            f.harnessBack()
            eventually(what = "sent") { f.engine.outbox.value.takeIf { it.isEmpty() } }
            // The fixture collects `results` on its own coroutine: the result may land just after
            // the outbox emptied.
            assertIs<OutboxResult.Succeeded>(eventually(what = "the result") { f.results.singleOrNull() })
        }
    }
}
