package dev.aas.android.sync

import dev.aas.android.protocol.AasJson
import dev.aas.android.protocol.Delivery
import dev.aas.android.protocol.Disposition
import dev.aas.android.protocol.ErrorKind
import dev.aas.android.protocol.FsMkdirParams
import dev.aas.android.protocol.FsMkdirResult
import dev.aas.android.protocol.InputPart
import dev.aas.android.protocol.Methods
import dev.aas.android.protocol.RpcException
import dev.aas.android.protocol.RpcMessage
import dev.aas.android.protocol.ThreadCreateParams
import dev.aas.android.protocol.ThreadCreateResult
import dev.aas.android.protocol.ThreadModesUpdate
import dev.aas.android.protocol.ThreadUpdateParams
import dev.aas.android.protocol.ThreadUpdateResult
import dev.aas.android.protocol.TurnStartParams
import dev.aas.android.protocol.TurnStartResult
import dev.aas.android.protocol.WorkspaceSnapshotResult
import kotlinx.coroutines.async
import kotlinx.coroutines.delay
import kotlinx.coroutines.withTimeout
import kotlinx.serialization.json.JsonObject
import kotlinx.serialization.json.jsonObject
import kotlinx.serialization.json.jsonPrimitive
import java.util.concurrent.atomic.AtomicBoolean
import kotlin.test.Test
import kotlin.test.assertEquals
import kotlin.test.assertFailsWith
import kotlin.test.assertIs
import kotlin.test.assertNull
import kotlin.test.assertTrue

/**
 * Chained outbox entries ([SyncEngine.submitChain], docs/android.md 6.3): committed together,
 * each sent only after the one before it succeeded, the rest dropped when one does not, and the
 * requests after a `thread/create` given the created thread's id.
 */
class SyncEngineChainTest {
    private val emptySnapshot = WorkspaceSnapshotResult(emptyList(), emptyList(), emptyList(), emptyList(), emptyList(), 0)

    private fun created(threadId: String) =
        AasJson.encodeToJsonElement(ThreadCreateResult.serializer(), ThreadCreateResult(Samples.thread(threadId, projectId = "prj_1")))

    private fun updated(threadId: String) =
        AasJson.encodeToJsonElement(ThreadUpdateResult.serializer(), ThreadUpdateResult(Samples.thread(threadId, projectId = "prj_1")))

    private val started = AasJson.encodeToJsonElement(TurnStartResult.serializer(), TurnStartResult(Disposition.Started, turnId = "trn_1"))

    private fun threadIdOf(msg: RpcMessage): String? = (msg.params as JsonObject)["threadId"]?.jsonPrimitive?.content

    private data class PlanChain(
        val creation: PendingMutation<ThreadCreateResult>,
        val modes: PendingMutation<ThreadUpdateResult>,
        val request: PendingMutation<TurnStartResult>,
    )

    /** `/plan <request>` of a new thread: create, plan mode, the request (as the app commits it). */
    private suspend fun planChain(engine: SyncEngine): PlanChain = engine.submitChain {
        PlanChain(
            add(Methods.ThreadCreate) { crid -> ThreadCreateParams(crid, "prj_1", "fake") },
            add(Methods.ThreadUpdate) { crid -> ThreadUpdateParams(crid, OutboxChain.CREATED_THREAD, modes = ThreadModesUpdate(plan = true)) },
            add(Methods.TurnStart) { crid -> TurnStartParams(crid, OutboxChain.CREATED_THREAD, listOf(InputPart.Text("plan this")), Delivery.Auto) },
        )
    }

    @Test
    fun theRequestsAfterACreationWaitForItAndGoToTheCreatedThreadInOrder() = withFixture { f ->
        f.server.snapshot = emptySnapshot
        val answerCreate = AtomicBoolean(false)
        f.server.onRequest = { _, msg ->
            when (msg.method) {
                Methods.ThreadCreate.name -> if (answerCreate.get()) created("thr_new") else null
                Methods.ThreadUpdate.name -> updated("thr_new")
                Methods.TurnStart.name -> started
                Methods.FsMkdir.name -> AasJson.encodeToJsonElement(FsMkdirResult.serializer(), FsMkdirResult("C:/work/new"))
                else -> JsonObject(emptyMap())
            }
        }
        // Committed offline, all three at once.
        f.engine.start()
        val chain = planChain(f.engine)
        val stored = f.store.state.value.outbox
        assertEquals(listOf(Methods.ThreadCreate.name, Methods.ThreadUpdate.name, Methods.TurnStart.name), stored.map { it.method })
        assertEquals(listOf(null, stored[0].clientRequestId, stored[1].clientRequestId), stored.map { it.after })
        assertEquals(listOf<String?>(null, null), stored.drop(1).map { it.threadId }, "no thread before it exists (no placeholder lane)")

        f.connect()
        f.awaitOnline()
        eventually(what = "thread/create sent") { f.server.requestsFor(Methods.ThreadCreate.name).firstOrNull() }
        // While the creation waits, its chain holds no lane: another request goes through.
        f.engine.mutate(Methods.FsMkdir) { crid -> FsMkdirParams(crid, "C:/work/new") }
        delay(200)
        assertTrue(f.server.requestsFor(Methods.ThreadUpdate.name).isEmpty() && f.server.requestsFor(Methods.TurnStart.name).isEmpty(), "nothing before the creation's answer")

        answerCreate.set(true)
        f.server.lastConnection.kill()
        assertEquals("thr_new", withTimeout(10_000) { chain.creation.await() }.thread.id)
        assertEquals(Disposition.Started, withTimeout(10_000) { chain.request.await() }.disposition)
        val update = f.server.requestsFor(Methods.ThreadUpdate.name).single()
        val start = f.server.requestsFor(Methods.TurnStart.name).single()
        assertEquals("thr_new", threadIdOf(update.second))
        assertEquals("thr_new", threadIdOf(start.second))
        assertEquals(ThreadModesUpdate(plan = true), AasJson.decodeFromJsonElement(Methods.ThreadUpdate.params, update.second.params!!).modes)
        assertTrue(f.server.requests.indexOf(update) < f.server.requests.indexOf(start), "plan mode before the request")
        eventually(what = "outbox empty") { f.store.state.value.outbox.takeIf { it.isEmpty() } }
    }

    @Test
    fun aRefusedCreationDropsTheRestOfItsChainUnsent() = withFixture { f ->
        f.server.snapshot = emptySnapshot
        f.server.onRequest = { _, msg ->
            if (msg.method == Methods.ThreadCreate.name) FakeServer.rpcError(ErrorKind.InvalidState, "the project folder is gone") else JsonObject(emptyMap())
        }
        f.connect()
        f.awaitOnline()
        val chain = planChain(f.engine)
        assertEquals(ErrorKind.InvalidState, assertFailsWith<RpcException> { withTimeout(10_000) { chain.creation.await() } }.kind)
        val broken = assertFailsWith<OutboxChainBrokenException> { withTimeout(10_000) { chain.request.await() } }
        assertEquals(chain.modes.clientRequestId, broken.after, "the request waited for plan mode")
        assertEquals(ErrorKind.InvalidState, broken.error?.kind, "the error that broke the chain")
        assertFailsWith<OutboxChainBrokenException> { withTimeout(10_000) { chain.modes.await() } }
        val results = eventually(what = "three results") { f.results.toList().takeIf { it.size == 3 } }
        assertIs<OutboxResult.Failed>(results[0])
        assertEquals(listOf(Methods.ThreadUpdate.name, Methods.TurnStart.name), results.drop(1).map { assertIs<OutboxResult.Dropped>(it).entry.method })
        delay(200)
        assertTrue(f.server.requestsFor(Methods.ThreadUpdate.name).isEmpty() && f.server.requestsFor(Methods.TurnStart.name).isEmpty(), "never sent")
        assertTrue(f.store.state.value.outbox.isEmpty())
    }

    @Test
    fun aRefusedModeChangeKeepsTheRequestFromBeingSentWithoutIt() = withFixture { f ->
        f.server.snapshot = emptySnapshot
        f.server.onRequest = { _, msg ->
            when (msg.method) {
                Methods.ThreadUpdate.name -> FakeServer.rpcError(ErrorKind.CapabilityUnsupported, "planMode")
                else -> started
            }
        }
        f.connect()
        f.awaitOnline()
        val request = f.engine.submitChain {
            add(Methods.ThreadUpdate) { crid -> ThreadUpdateParams(crid, "thr_1", modes = ThreadModesUpdate(plan = true)) }
            add(Methods.TurnStart) { crid -> TurnStartParams(crid, "thr_1", listOf(InputPart.Text("plan this")), Delivery.Queue) }
        }
        // The request was in the thread's lane (it names the thread) and waited for the update.
        val broken = assertFailsWith<OutboxChainBrokenException> { withTimeout(10_000) { request.await() } }
        assertEquals(ErrorKind.CapabilityUnsupported, broken.error?.kind)
        delay(200)
        assertTrue(f.server.requestsFor(Methods.TurnStart.name).isEmpty(), "a /plan request never goes without plan mode")
        // The thread's lane moves on: the next message is sent.
        assertEquals(Disposition.Started, f.engine.mutate(Methods.TurnStart, turnStart("thr_1", "next")).disposition)
    }

    @Test
    fun takingBackTheCreationDropsItsChain() = withFixture { f ->
        f.engine.start()
        val chain = planChain(f.engine)
        assertEquals(OutboxDiscard.Discarded, f.engine.discardOutbox(chain.creation.clientRequestId))
        assertIs<OutboxClearedException>(runCatching { withTimeout(10_000) { chain.creation.await() } }.exceptionOrNull())
        val broken = assertFailsWith<OutboxChainBrokenException> { withTimeout(10_000) { chain.request.await() } }
        assertNull(broken.error, "taken back, not refused")
        val results = eventually(what = "three results") { f.results.toList().takeIf { it.size == 3 } }
        assertIs<OutboxResult.Discarded>(results[0])
        assertTrue(results.drop(1).all { it is OutboxResult.Dropped })
        assertTrue(f.store.state.value.outbox.isEmpty())
    }

    /**
     * The process ends while the creation waits (the new-thread screen is long gone): the chain
     * is in the store, so the next engine sends its requests to the created thread.
     */
    @Test
    fun aChainOutlivesTheProcess() {
        val store = InMemorySyncStore()
        EngineFixture(store = store).use { first ->
            engineTest {
                first.engine.start()
                planChain(first.engine)
            }
        }
        withFixture(EngineFixture(store = store)) { f ->
            f.server.snapshot = emptySnapshot
            f.server.onRequest = { _, msg ->
                when (msg.method) {
                    Methods.ThreadCreate.name -> created("thr_new")
                    Methods.ThreadUpdate.name -> updated("thr_new")
                    else -> started
                }
            }
            f.connect()
            f.awaitOnline()
            eventually(what = "the request sent") { f.server.requestsFor(Methods.TurnStart.name).firstOrNull() }
            assertEquals("thr_new", threadIdOf(f.server.requestsFor(Methods.ThreadUpdate.name).single().second))
            assertEquals("thr_new", threadIdOf(f.server.requestsFor(Methods.TurnStart.name).single().second))
            val methods = f.server.requests.map { it.second.method }.filter { it in setOf(Methods.ThreadCreate.name, Methods.ThreadUpdate.name, Methods.TurnStart.name) }
            assertEquals(listOf(Methods.ThreadCreate.name, Methods.ThreadUpdate.name, Methods.TurnStart.name), methods)
            eventually(what = "outbox empty") { f.store.state.value.outbox.takeIf { it.isEmpty() } }
        }
    }

    @Test
    fun aRequestAfterACreationMustBeForTheCreatedThread() = withFixture { f ->
        f.engine.start()
        assertFailsWith<IllegalArgumentException> {
            f.engine.submitChain {
                add(Methods.ThreadCreate) { crid -> ThreadCreateParams(crid, "prj_1", "fake") }
                add(Methods.TurnStart) { crid -> TurnStartParams(crid, "thr_other", listOf(InputPart.Text("x")), Delivery.Auto) }
            }
        }
        assertTrue(f.store.state.value.outbox.isEmpty(), "nothing of a malformed chain is committed")
    }

    @Test
    fun theWaitingRequestsShowInTheCreatedThreadOnceItExists() = withFixture { f ->
        f.server.snapshot = emptySnapshot
        f.server.onRequest = { _, msg ->
            when (msg.method) {
                Methods.ThreadCreate.name -> created("thr_new")
                // The mode change is not answered: the request waits behind it, in the new thread.
                Methods.ThreadUpdate.name -> null
                else -> started
            }
        }
        f.connect()
        f.awaitOnline()
        val chain = planChain(f.engine)
        withTimeout(10_000) { chain.creation.await() }
        val waiting = eventually(what = "the chain in the new thread") {
            f.engine.outbox.value.takeIf { entries -> entries.size == 2 && entries.all { it.threadId == "thr_new" } }
        }
        assertEquals(listOf(null, waiting[0].clientRequestId), waiting.map { it.after })
        val opened = f.scope.async { f.engine.openThread("thr_new") }
        val pending = eventually(what = "pending in the thread") { opened.await().value.pending.takeIf { it.size == 2 } }
        assertEquals(listOf(Methods.ThreadUpdate.name, Methods.TurnStart.name), pending.map { it.method })
        assertTrue(f.server.requestsFor(Methods.TurnStart.name).isEmpty())
        assertEquals("thr_new", f.server.requestsFor(Methods.ThreadUpdate.name).single().second.params!!.jsonObject["threadId"]!!.jsonPrimitive.content)
    }
}
