package dev.aas.android.sync

import dev.aas.android.protocol.ClientInfo
import dev.aas.android.protocol.DeviceRevokeParams
import dev.aas.android.protocol.Disposition
import dev.aas.android.protocol.InteractionRequest
import dev.aas.android.protocol.InteractionRespondParams
import dev.aas.android.protocol.InteractionResolution
import dev.aas.android.protocol.InteractionStatus
import dev.aas.android.protocol.Item
import dev.aas.android.protocol.ItemStatus
import dev.aas.android.protocol.Methods
import dev.aas.android.protocol.NativeImportParams
import dev.aas.android.protocol.NativeListParams
import dev.aas.android.protocol.ProjectCreateParams
import dev.aas.android.protocol.ProjectInit
import dev.aas.android.protocol.ProjectListParams
import dev.aas.android.protocol.ProjectOpenParams
import dev.aas.android.protocol.QuestionAnswer
import dev.aas.android.protocol.ThreadCreateParams
import dev.aas.android.protocol.ThreadForkParams
import dev.aas.android.protocol.ThreadListParams
import dev.aas.android.protocol.ThreadReadParams
import dev.aas.android.protocol.ThreadUpdateParams
import dev.aas.android.protocol.Turn
import dev.aas.android.protocol.TurnStatus
import dev.aas.android.protocol.WORKSPACE_STREAM
import dev.aas.android.protocol.threadStream
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.SupervisorJob
import kotlinx.coroutines.cancel
import kotlinx.coroutines.delay
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.launch
import kotlinx.coroutines.runBlocking
import kotlinx.coroutines.withTimeout
import kotlinx.serialization.json.jsonObject
import kotlinx.serialization.json.jsonPrimitive
import okhttp3.OkHttpClient
import org.junit.After
import org.junit.Assume
import org.junit.Before
import org.junit.Rule
import org.junit.rules.Timeout
import java.util.concurrent.CopyOnWriteArrayList
import java.util.concurrent.TimeUnit
import java.util.concurrent.atomic.AtomicBoolean
import java.util.concurrent.atomic.AtomicInteger
import kotlin.random.Random
import kotlin.test.Test
import kotlin.test.assertEquals
import kotlin.test.assertIs
import kotlin.test.assertTrue

/**
 * The engine against the real daemon: `aas-test-server` (engine + transport, fake harness with
 * real agent processes) behind its chaos proxy. Runs when `AAS_TEST_SERVER` names the
 * executable; skipped otherwise. Each test starts its own server on its own state folder and
 * shuts it down afterwards, checking that no agent process survived.
 */
class RealServerTest {
    @get:Rule
    val timeout: Timeout = Timeout(TEST_TIMEOUT_MINUTES, TimeUnit.MINUTES)

    private lateinit var server: AasTestServer
    private val clients = CopyOnWriteArrayList<Client>()

    @Before
    fun startServer() {
        val exe = AasTestServer.locate()
        exe.exceptionOrNull()?.let { why ->
            val reason = "RealServerTest skipped: ${why.message} (point AAS_TEST_SERVER at target/aas-test-bin/aas-test-server.exe)"
            println(reason)
            Assume.assumeTrue(reason, false)
        }
        server = AasTestServer.start(exe.getOrThrow(), heartbeatMs = HEARTBEAT_MS, clientTimeoutMs = CLIENT_TIMEOUT_MS)
    }

    @After
    fun stopServer() {
        clients.forEach { it.close() }
        if (!::server.isInitialized) return
        val agents = server.recordedAgents()
        val exit = server.quit()
        val deadline = System.currentTimeMillis() + AGENT_EXIT_GRACE_MS
        var survivors = agents.filter { it.alive() }
        while (survivors.isNotEmpty() && System.currentTimeMillis() < deadline) {
            Thread.sleep(POLL_MS)
            survivors = survivors.filter { it.alive() }
        }
        val stderr = server.stderrTail()
        server.close()
        assertEquals(0, exit, "aas-test-server exit code\n$stderr")
        assertTrue(survivors.isEmpty(), "agent processes outlived the server: $survivors")
    }

    // ----- helpers ----------------------------------------------------------------------------------

    /** One app instance: its own store and engine. */
    private inner class Client(credentials: Credentials, name: String) : AutoCloseable {
        val store = InMemorySyncStore()
        val scope = CoroutineScope(SupervisorJob() + Dispatchers.Default)
        val signals = CopyOnWriteArrayList<SyncSignal>()
        val results = CopyOnWriteArrayList<OutboxResult>()
        val sent = CopyOnWriteArrayList<String>()
        val log = CopyOnWriteArrayList<String>()
        val engine = SyncEngine(
            store = store,
            http = OkHttpClient(),
            scope = scope,
            clientInfo = ClientInfo(name, "test", "jvm"),
            config = REAL_CONFIG,
            logger = { level, message, error -> log += "$level $message${error?.let { " ($it)" } ?: ""}" },
            wireTap = object : WireTap {
                override fun sent(text: String) {
                    sent += text
                }
            },
        )

        init {
            clients += this
            scope.launch { engine.signals.collect { signals += it } }
            scope.launch { engine.results.collect { results += it } }
            engine.setCredentials(credentials)
            engine.start()
        }

        fun describe(): String = "status ${engine.status.value}\nlog tail:\n  " + log.takeLast(25).joinToString("\n  ")

        suspend fun awaitOnline(timeoutMs: Long = STEP_MS) {
            try {
                eventually(timeoutMs, "online") { engine.status.value.isOnline.takeIf { it } }
            } catch (e: AssertionError) {
                throw AssertionError("${e.message}\n${describe()}\n${server.stderrTail()}")
            }
        }

        suspend fun <T : Any> waitFor(what: String, timeoutMs: Long = STEP_MS, check: suspend () -> T?): T = try {
            eventually(timeoutMs, what, check)
        } catch (e: AssertionError) {
            throw AssertionError("${e.message}\n${describe()}\n${server.stderrTail()}")
        }

        suspend fun createProject(name: String): String = engine.mutate(Methods.ProjectCreate) { crid ->
            ProjectCreateParams(crid, parentPath = server.ready.root, name = name, init = ProjectInit.Empty)
        }.project!!.id

        suspend fun createThread(projectId: String): String =
            engine.mutate(Methods.ThreadCreate) { crid -> ThreadCreateParams(crid, projectId, HARNESS) }.thread.id

        suspend fun startTurn(threadId: String, text: String): String {
            val r = engine.mutate(Methods.TurnStart, turnStart(threadId, text))
            assertEquals(Disposition.Started, r.disposition, "turn/start $text")
            return r.turnId!!
        }

        suspend fun awaitTurnEnd(threadId: String, turnId: String, timeoutMs: Long = STEP_MS): Turn {
            val state = engine.thread(threadId) ?: throw AssertionError("thread $threadId is not open")
            return waitFor("turn $turnId to end", timeoutMs) { state.value.turns.find { it.id == turnId && it.status.isTerminal } }
        }

        suspend fun runTurn(threadId: String, text: String): Turn = awaitTurnEnd(threadId, startTurn(threadId, text))

        /** Waits until the open thread's local state equals `thread/read` (all turns). */
        suspend fun assertThreadMatchesServer(threadId: String) {
            val state = engine.thread(threadId) ?: throw AssertionError("thread $threadId is not open")
            var diff = ""
            try {
                eventually(CONVERGE_MS, "local thread to equal thread/read") {
                    awaitOnline()
                    val read = try {
                        engine.query(Methods.ThreadRead, ThreadReadParams(threadId, limitTurns = MAX_TURNS))
                    } catch (e: ConnectionLostException) {
                        return@eventually null
                    } catch (e: NotConnectedException) {
                        return@eventually null
                    }
                    val local = state.value
                    diff = threadDiff(local, read.thread, read.turns, read.items, read.interactions, read.queued)
                    val cursor = store.state.value.cursors[threadStream(threadId)] ?: -1
                    if (diff.isEmpty() && cursor >= read.head) Unit else null.also { if (diff.isEmpty()) diff = "cursor $cursor < head ${read.head}" }
                }
            } catch (e: AssertionError) {
                throw AssertionError("${e.message}: $diff\n${describe()}")
            }
        }

        /** Waits until the local workspace equals `project/list` and `thread/list`. */
        suspend fun assertWorkspaceMatchesServer() {
            var diff = ""
            try {
                eventually(CONVERGE_MS, "local workspace to equal the server") {
                    awaitOnline()
                    val projects = engine.query(Methods.ProjectList, ProjectListParams()).projects.sortedBy { it.id }
                    val threads = engine.query(Methods.ThreadList, ThreadListParams(limit = MAX_TURNS)).threads
                    val ws = engine.workspace.value
                    val localProjects = ws.projects.filter { !it.archived }.sortedBy { it.id }
                    val localThreads = ws.threads.map { it.thread }.filter { !it.archived }
                    diff = buildString {
                        if (localProjects != projects) append("projects: $localProjects != $projects; ")
                        if (localThreads != threads) append("threads: $localThreads != $threads; ")
                    }
                    diff.isEmpty().takeIf { it }
                }
            } catch (e: AssertionError) {
                throw AssertionError("${e.message}: $diff\n${describe()}")
            }
        }

        /** How often each outbox request was put on the wire, by clientRequestId. */
        fun sendsByRequestId(method: String): Map<String, Int> = sent.mapNotNull { text ->
            val msg = dev.aas.android.protocol.AasJson.parseToJsonElement(text).jsonObject
            if (msg["method"]?.jsonPrimitive?.content != method) return@mapNotNull null
            msg["params"]?.jsonObject?.get("clientRequestId")?.jsonPrimitive?.content
        }.groupingBy { it }.eachCount()

        override fun close() {
            engine.stop()
            scope.cancel()
        }
    }

    private fun threadDiff(
        local: ThreadState,
        thread: dev.aas.android.protocol.Thread,
        turns: List<Turn>,
        items: List<Item>,
        interactions: List<dev.aas.android.protocol.Interaction>,
        queued: List<dev.aas.android.protocol.QueuedInput>,
    ): String = buildString {
        if (local.thread != thread) append("thread summary differs: ${local.thread} vs $thread; ")
        if (local.turns != turns) append("turns differ: ${local.turns} vs $turns; ")
        if (local.items != items) {
            append("items differ: ${local.items.size} vs ${items.size}; ")
            local.items.zip(items).firstOrNull { (a, b) -> a != b }?.let { (a, b) -> append("first difference: $a vs $b; ") }
        }
        val byId = compareBy<dev.aas.android.protocol.Interaction> { it.id }
        if (local.interactions.sortedWith(byId) != interactions.sortedWith(byId)) append("interactions differ: ${local.interactions} vs $interactions; ")
        if (local.queued != queued) append("queue differs: ${local.queued} vs $queued; ")
    }

    /** Runs a scenario; its own timeout ends it before the JUnit rule would abandon the thread. */
    private fun realTest(block: suspend CoroutineScope.() -> Unit) {
        runBlocking { withTimeout(SCENARIO_TIMEOUT_MS) { block() } }
    }

    private suspend fun openLive(client: Client, threadId: String): StateFlow<ThreadState> {
        val state = client.engine.openThread(threadId)
        client.waitFor("thread $threadId live") { state.value.takeIf { it.sync == ThreadSync.Live } }
        return state
    }

    // ----- scenarios ----------------------------------------------------------------------------------

    @Test
    fun firstSyncProjectThreadAndATurn() = realTest {
        val c = Client(server.ready.credentials, "phone")
        c.awaitOnline()
        val ws = c.engine.workspace.value
        assertTrue(ws.synced)
        assertEquals(listOf(HARNESS), ws.harnesses.map { it.id })
        assertEquals(server.ready.epoch, c.store.state.value.epoch)
        assertEquals(1, c.sent.count { it.contains("\"workspace/snapshot\"") }, "one full sync")

        val projectId = c.createProject("app")
        c.waitFor("project in the workspace") { c.engine.workspace.value.projects.find { it.id == projectId } }
        val threadId = c.createThread(projectId)
        c.waitFor("thread in the workspace") { c.engine.workspace.value.threads.find { it.thread.id == threadId } }
        openLive(c, threadId)

        val turn = c.runTurn(threadId, "@text hello from kotlin")
        assertEquals(TurnStatus.Completed, turn.status)
        val items = c.engine.thread(threadId)!!.value.items
        assertEquals("@text hello from kotlin", (items.first() as Item.UserMessage).text)
        assertEquals("hello from kotlin", (items.last() as Item.AgentMessage).text)
        c.waitFor("turn finished signal") { c.signals.filterIsInstance<SyncSignal.TurnFinished>().find { it.turn.id == turn.id } }
        c.assertThreadMatchesServer(threadId)
        c.assertWorkspaceMatchesServer()
    }

    @Test
    fun approvalsAndQuestionsRoundTrip() = realTest {
        val c = Client(server.ready.credentials, "phone")
        c.awaitOnline()
        val threadId = c.createThread(c.createProject("approvals"))
        openLive(c, threadId)

        val turnId = c.startTurn(threadId, "@approve some-cmd")
        val pending = c.waitFor("approval signal") {
            c.signals.filterIsInstance<SyncSignal.InteractionPending>().find { it.interaction.threadId == threadId }
        }
        assertIs<InteractionRequest.Approval>(pending.interaction.request)
        assertEquals(listOf(pending.interaction.id), c.engine.workspace.value.pendingInteractions.map { it.id })
        val answer = c.engine.mutate(Methods.InteractionRespond) { crid ->
            InteractionRespondParams(crid, pending.interaction.id, InteractionResolution.Approval("allow"))
        }
        assertEquals(false, answer.alreadyResolved)
        assertEquals(TurnStatus.Completed, c.awaitTurnEnd(threadId, turnId).status)
        c.waitFor("closed signal") { c.signals.filterIsInstance<SyncSignal.InteractionClosed>().find { it.interactionId == pending.interaction.id } }
        val state = c.engine.thread(threadId)!!.value
        val command = state.items.filterIsInstance<Item.CommandExecution>().single()
        assertEquals(0, command.exitCode)
        assertEquals(ItemStatus.Completed, command.status)
        val resolved = state.interactions.single()
        assertEquals(InteractionStatus.Resolved, resolved.status)
        assertEquals(InteractionResolution.Approval("allow"), resolved.resolution)
        assertTrue(c.engine.workspace.value.pendingInteractions.isEmpty())

        val questionTurn = c.startTurn(threadId, "@question")
        val question = c.waitFor("question signal") {
            c.signals.filterIsInstance<SyncSignal.InteractionPending>().find { it.interaction.request is InteractionRequest.Question }
        }
        c.engine.mutate(Methods.InteractionRespond) { crid ->
            InteractionRespondParams(crid, question.interaction.id, InteractionResolution.Question(listOf(QuestionAnswer("q1", listOf("blue")))))
        }
        assertEquals(TurnStatus.Completed, c.awaitTurnEnd(threadId, questionTurn).status)
        assertEquals("answer: blue", (c.engine.thread(threadId)!!.value.items.last() as Item.AgentMessage).text)
        c.assertThreadMatchesServer(threadId)
    }

    @Test
    fun usageContextLargeOutputsAndFailures() = realTest {
        val c = Client(server.ready.credentials, "phone")
        c.awaitOnline()
        val threadId = c.createThread(c.createProject("outputs"))
        val state = openLive(c, threadId)

        val usageTurn = c.runTurn(threadId, "@context 1200 8000\n@stream 50 2")
        assertEquals(TurnStatus.Completed, usageTurn.status)
        assertEquals(1200L, usageTurn.usage?.context?.usedTokens)
        assertEquals(8000L, usageTurn.usage?.context?.windowTokens)
        c.waitFor("thread context usage") { state.value.thread?.usage?.context?.takeIf { it.usedTokens == 1200L } }
        assertEquals((0 until 50).joinToString("") { "tok$it " }, state.value.items.filterIsInstance<Item.AgentMessage>().last().text)

        val bigTurn = c.runTurn(threadId, "@bigoutput 100000")
        assertEquals(TurnStatus.Completed, bigTurn.status)
        val big = state.value.items.filterIsInstance<Item.CommandExecution>().last()
        assertTrue(big.outputTruncated, "a 100 kB output is not inlined")
        val blob = AasHttp(OkHttpClient()).downloadBlob(server.ready.credentials, big.outputBlobId!!)
        assertEquals(100_000, blob.size)
        assertTrue(String(blob, Charsets.UTF_8).startsWith(big.output.take(64)))

        val failed = c.runTurn(threadId, "@fail on purpose")
        assertEquals(TurnStatus.Failed, failed.status)
        assertEquals("harnessError", failed.error?.kind)
        val crashed = c.runTurn(threadId, "@crash")
        assertEquals(TurnStatus.Failed, crashed.status)
        assertEquals("agentExited", crashed.error?.kind)
        val finished = c.waitFor("finished signals") { c.signals.filterIsInstance<SyncSignal.TurnFinished>().takeIf { it.size >= 4 } }
        assertEquals(TurnStatus.Failed, finished.last().turn.status)
        // A new process takes the next turn.
        assertEquals(TurnStatus.Completed, c.runTurn(threadId, "after the crash").status)
        c.assertThreadMatchesServer(threadId)
    }

    @Test
    fun dropsAndABlackholeDuringStreamingLoseAndDuplicateNothing() = realTest {
        val c = Client(server.ready.credentials, "phone")
        c.awaitOnline()
        val threadId = c.createThread(c.createProject("chaos"))
        val state = openLive(c, threadId)

        val turnId = c.startTurn(threadId, "@stream 400 10")
        c.waitFor("streaming") { state.value.items.filterIsInstance<Item.AgentMessage>().firstOrNull()?.text?.takeIf { it.isNotEmpty() } }
        // Requests made while the connection is being dropped: queued turns and a rename.
        val queued = (1..3).map { i -> c.engine.enqueue(Methods.TurnStart, turnStart(threadId, "queued $i")) }
        val rename = c.engine.enqueue(Methods.ThreadUpdate) { crid -> ThreadUpdateParams(crid, threadId, title = "renamed under chaos") }
        server.chaos(AasTestServer.Chaos.Drop)
        delay(300)
        server.chaos(AasTestServer.Chaos.Drop)
        c.awaitOnline()
        // Longer than the client timeout: only the watchdogs notice.
        server.chaos(AasTestServer.Chaos.Blackhole)
        delay(CLIENT_TIMEOUT_MS + 700)
        server.chaos(AasTestServer.Chaos.Pass)

        assertEquals(TurnStatus.Completed, c.awaitTurnEnd(threadId, turnId).status)
        c.waitFor("all requests answered") { c.store.state.value.outbox.takeIf { it.isEmpty() } }
        for (crid in queued + rename) {
            val outcome = c.results.filter { it.entry.clientRequestId == crid }
            assertEquals(1, outcome.size, "one final outcome for $crid")
            assertIs<OutboxResult.Succeeded>(outcome.single())
        }
        c.waitFor("queued turns ran") { state.value.turns.takeIf { t -> t.size == 4 && t.all { it.status.isTerminal } } }
        c.assertThreadMatchesServer(threadId)
        val users = state.value.items.filterIsInstance<Item.UserMessage>().map { it.text }
        assertEquals(listOf("@stream 400 10", "queued 1", "queued 2", "queued 3"), users, "each input exactly once, in order")
        assertEquals("renamed under chaos", state.value.thread?.title)
        assertTrue(c.engine.status.value.reconnects >= 2, "the chaos forced reconnects: ${c.engine.status.value}")
        assertTrue(c.sendsByRequestId("turn/start").keys.containsAll(queued))
        c.assertWorkspaceMatchesServer()
    }

    @Test
    fun aRestartMidTurnEndsTheTurnAndTheClientResumes() = realTest {
        val c = Client(server.ready.credentials, "phone")
        c.awaitOnline()
        val threadId = c.createThread(c.createProject("restart"))
        val state = openLive(c, threadId)
        val turnId = c.startTurn(threadId, "@stream 1000 10")
        c.waitFor("streaming") { state.value.items.filterIsInstance<Item.AgentMessage>().firstOrNull()?.text?.takeIf { it.length > 20 } }
        val before = c.engine.status.value.reconnects
        val ready = server.restart()
        assertEquals(server.ready.epoch, ready.epoch)
        val ended = c.awaitTurnEnd(threadId, turnId)
        // protocol.md §3.1: a turn cut short by the daemon stopping is interrupted, with the reason.
        assertEquals(TurnStatus.Interrupted, ended.status)
        assertTrue(ended.error?.kind in setOf("daemonShutdown", "daemonRestarted"), "error ${ended.error}")
        c.waitFor("reconnected") { c.engine.status.value.takeIf { it.isOnline && it.reconnects > before } }
        assertEquals(1, c.sent.count { it.contains("\"workspace/snapshot\"") }, "same epoch: resumed, not resynced")
        c.waitFor("interrupted signal") { c.signals.filterIsInstance<SyncSignal.TurnFinished>().find { it.turn.id == turnId } }
        assertEquals(TurnStatus.Completed, c.runTurn(threadId, "@text after the restart").status)
        c.assertThreadMatchesServer(threadId)
        c.assertWorkspaceMatchesServer()
    }

    @Test
    fun aResetIsANewEpochTheClientWipesAndResyncs() = realTest {
        val c = Client(server.ready.credentials, "phone")
        c.awaitOnline()
        val oldProject = c.createProject("before-reset")
        val threadId = c.createThread(oldProject)
        val state = openLive(c, threadId)
        assertEquals(TurnStatus.Completed, c.runTurn(threadId, "@text old history").status)
        val oldEpoch = server.ready.epoch

        val fresh = server.reset()
        // The old token is gone with the old database.
        val suspended = c.waitFor("suspended") { c.engine.status.value.connection as? ConnectionState.Suspended }
        assertEquals(SuspendReason.Unauthorized(401), suspended.reason)
        // A request made meanwhile stays in the outbox across the wipe.
        val stale = c.engine.enqueue(Methods.ThreadUpdate) { crid -> ThreadUpdateParams(crid, threadId, title = "gone") }
        // Pair again over HTTP with the new pairing code (as the app does after a QR scan).
        val paired = AasHttp(OkHttpClient()).pair(fresh.wsUrl, fresh.pairingCode, "phone again")
        assertEquals(fresh.epoch, paired.server.epoch)
        c.engine.setCredentials(Credentials(fresh.wsUrl, paired.token))
        c.awaitOnline()

        assertTrue(fresh.epoch != oldEpoch)
        assertEquals(fresh.epoch, c.store.state.value.epoch)
        assertTrue(c.engine.workspace.value.projects.none { it.id == oldProject }, "the old workspace was wiped")
        c.waitFor("open thread removed") { state.value.takeIf { it.sync == ThreadSync.Removed } }
        val failure = c.waitFor("stale request answered") { c.results.find { it.entry.clientRequestId == stale } }
        assertIs<OutboxResult.Failed>(failure)
        assertTrue(c.store.state.value.outbox.isEmpty())
        assertEquals(2, c.sent.count { it.contains("\"workspace/snapshot\"") }, "a second full sync after the epoch change")

        // The new server works as usual.
        val newThread = c.createThread(c.createProject("after-reset"))
        openLive(c, newThread)
        assertEquals(TurnStatus.Completed, c.runTurn(newThread, "@text new epoch").status)
        c.assertThreadMatchesServer(newThread)
        c.assertWorkspaceMatchesServer()
    }

    @Test
    fun replacedAndRevokedConnectionsFollowTheCloseCodes() = realTest {
        val first = Client(server.ready.credentials, "phone")
        first.awaitOnline()
        // The same device connects again (a second app process): the first is replaced (4000).
        val second = Client(server.ready.credentials, "phone-again")
        second.awaitOnline()
        first.waitFor("replaced") { (first.engine.status.value.connection as? ConnectionState.Suspended)?.takeIf { it.reason == SuspendReason.Replaced } }
        delay(1_000)
        assertIs<ConnectionState.Suspended>(first.engine.status.value.connection, "no fight for the connection")
        assertTrue(second.engine.status.value.isOnline)
        // Coming back to the foreground takes the connection back.
        first.engine.onAppForeground()
        first.awaitOnline()
        second.waitFor("second replaced") { (second.engine.status.value.connection as? ConnectionState.Suspended)?.takeIf { it.reason == SuspendReason.Replaced } }
        second.close()

        // Another device revokes this one (4001): no reconnect until it pairs again.
        val token = AasHttp(OkHttpClient()).pair(server.ready.wsUrl, server.pairingCode(), "tablet").token
        val tablet = Client(Credentials(server.ready.wsUrl, token), "tablet")
        tablet.awaitOnline()
        tablet.engine.mutate(Methods.DeviceRevoke) { crid -> DeviceRevokeParams(crid, server.ready.deviceId) }
        first.waitFor("revoked") { (first.engine.status.value.connection as? ConnectionState.Suspended)?.takeIf { it.reason == SuspendReason.Revoked } }
        first.engine.reconnectNow()
        first.engine.onAppForeground()
        delay(1_000)
        assertEquals(ConnectionState.Suspended(SuspendReason.Revoked), first.engine.status.value.connection)
    }

    /**
     * "PC のセッションを取り込む" against the real daemon: the fake harness keeps native sessions like
     * a CLI, and the test server seeded two in its `nativeProject` folder. The client lists them
     * (`native/list`), imports one (`native/import`, the history arrives as completed turns),
     * continues it (the next turn resumes the native session), and sees sessions recorded later.
     */
    @Test
    fun nativeSessionsAreListedImportedAndResumed() = realTest {
        val c = Client(server.ready.credentials, "phone")
        c.awaitOnline()
        val fake = c.engine.workspace.value.harnesses.single { it.id == HARNESS }
        assertTrue(fake.capabilities.nativeSessions && fake.capabilities.fork, "the fake harness keeps sessions: ${fake.capabilities}")
        // harness/refresh answers with the same harness (and publishes nothing new).
        assertEquals(listOf(HARNESS to true), c.engine.refreshHarnesses().map { it.id to it.available })
        assertTrue(c.engine.refreshingHarnesses.value.isEmpty())

        val project = c.engine.mutate(Methods.ProjectOpen) { crid -> ProjectOpenParams(crid, server.ready.nativeProject) }.project
        val listed = c.engine.query(Methods.NativeList, NativeListParams(project.id, HARNESS)).sessions
        assertEquals(server.ready.nativeSessions.map { it.nativeSessionId }.toSet(), listed.map { it.nativeSessionId }.toSet())
        assertEquals(setOf("Explain the build", "Check the tests"), listed.mapNotNull { it.title }.toSet())
        assertTrue(listed.all { it.importedThreadId == null }, "nothing imported yet: $listed")
        val explain = listed.single { it.title == "Explain the build" }

        val imported = c.engine.mutate(Methods.NativeImport) { crid -> NativeImportParams(crid, project.id, HARNESS, explain.nativeSessionId) }.thread
        assertEquals(explain.nativeSessionId, imported.nativeSessionId)
        c.waitFor("the imported thread in the workspace") { c.engine.workspace.value.threads.find { it.thread.id == imported.id } }
        val state = openLive(c, imported.id)
        assertEquals(2, state.value.turns.size, "the session's two turns")
        assertTrue(state.value.turns.all { it.status == TurnStatus.Completed }, "${state.value.turns}")
        val kinds = state.value.items.map { it::class }.toSet()
        for (kind in listOf(Item.UserMessage::class, Item.Reasoning::class, Item.CommandExecution::class, Item.AgentMessage::class, Item.Plan::class)) {
            assertTrue(kind in kinds, "$kind in the imported history: $kinds")
        }
        c.assertThreadMatchesServer(imported.id)
        // Imported once: the list says so, and importing again opens the same thread.
        val again = c.engine.query(Methods.NativeList, NativeListParams(project.id, HARNESS)).sessions.single { it.nativeSessionId == explain.nativeSessionId }
        assertEquals(imported.id, again.importedThreadId)
        val twice = c.engine.mutate(Methods.NativeImport) { crid -> NativeImportParams(crid, project.id, HARNESS, explain.nativeSessionId) }.thread
        assertEquals(imported.id, twice.id)

        // The next message continues the native session (resume), it does not start a new one.
        assertEquals(TurnStatus.Completed, c.runTurn(imported.id, "@text continued on the phone").status)
        assertEquals("continued on the phone", (state.value.items.last() as Item.AgentMessage).text)
        assertEquals(3, state.value.turns.size)
        val transcript = java.io.File(server.ready.nativeSessionsDir, "${explain.nativeSessionId}.jsonl")
        // The fake CLI's session file: a header line, then one line per finished turn.
        c.waitFor("the resumed turn in the native session") { transcript.readLines().count { it.isNotBlank() }.takeIf { it == 1 + 3 } }
        c.assertThreadMatchesServer(imported.id)

        // A session recorded on the PC later, in another folder.
        val later = server.nativeSession("later/app", "Review the diff\n@exec git diff\n@text Looks fine.")
        assertEquals("Review the diff", later.title)
        val other = c.engine.mutate(Methods.ProjectOpen) { crid -> ProjectOpenParams(crid, later.cwd) }.project
        val otherSessions = c.engine.query(Methods.NativeList, NativeListParams(other.id, HARNESS)).sessions
        assertEquals(listOf(later.nativeSessionId), otherSessions.map { it.nativeSessionId })
        c.assertWorkspaceMatchesServer()
    }

    /**
     * `thread/fork` against the real daemon: the new thread carries the source's history under
     * new ids and `forkedFrom`, its first turn branches a native session of its own, and the
     * source is unchanged.
     */
    @Test
    fun aForkBranchesTheThreadAndItsNativeSession() = realTest {
        val c = Client(server.ready.credentials, "phone")
        c.awaitOnline()
        val project = c.engine.mutate(Methods.ProjectOpen) { crid -> ProjectOpenParams(crid, server.ready.nativeProject) }.project
        val source = c.createThread(project.id)
        val sourceState = openLive(c, source)
        val firstTurn = c.runTurn(source, "@text the original answer")
        assertEquals(TurnStatus.Completed, firstTurn.status)
        val sourceSession = c.waitFor("the source's native session") { sourceState.value.thread?.nativeSessionId }

        val fork = c.engine.mutate(Methods.ThreadFork) { crid -> ThreadForkParams(crid, source) }.thread
        assertTrue(fork.id != source)
        assertEquals(source, fork.forkedFrom?.threadId)
        assertEquals(firstTurn.id, fork.forkedFrom?.turnId)
        c.waitFor("the fork in the workspace") { c.engine.workspace.value.threads.find { it.thread.id == fork.id } }
        val forkState = openLive(c, fork.id)
        assertEquals(1, forkState.value.turns.size, "the source's history")
        assertTrue(forkState.value.turns.single().id != firstTurn.id, "copied under a new id")
        assertEquals("the original answer", (forkState.value.items.last() as Item.AgentMessage).text)

        assertEquals(TurnStatus.Completed, c.runTurn(fork.id, "@text only in the fork").status)
        val forkSession = c.waitFor("the fork's native session") { forkState.value.thread?.nativeSessionId?.takeIf { it != sourceSession } }
        val sessions = c.engine.query(Methods.NativeList, NativeListParams(project.id, HARNESS)).sessions
        assertEquals(fork.id, sessions.single { it.nativeSessionId == forkSession }.importedThreadId)
        assertEquals(source, sessions.single { it.nativeSessionId == sourceSession }.importedThreadId)
        // The source did not move.
        assertEquals(1, sourceState.value.turns.size)
        assertTrue(sourceState.value.items.none { it is Item.AgentMessage && it.text == "only in the fork" })
        c.assertThreadMatchesServer(source)
        c.assertThreadMatchesServer(fork.id)
        c.assertWorkspaceMatchesServer()
    }

    // ----- randomised chaos ---------------------------------------------------------------------------

    @Test
    fun chaosSeed1() = chaosRun(1)

    @Test
    fun chaosSeed2() = chaosRun(2)

    @Test
    fun chaosSeed3() = chaosRun(3)

    /**
     * Like crates/aas-testkit/tests/chaos.rs: while turns stream, ask for approvals and produce
     * outputs too large to inline, the proxy (driven by a seeded RNG) drops every connection,
     * delays traffic, or blackholes it for longer than the client timeout. Afterwards the local
     * state must equal the server's.
     */
    private fun chaosRun(seed: Long) = realTest {
        try {
            chaosScenario(seed)
        } catch (e: Throwable) {
            System.err.println("RealServerTest chaos FAILED with seed $seed")
            throw e
        }
    }

    private suspend fun CoroutineScope.chaosScenario(seed: Long) {
        val c = Client(server.ready.credentials, "phone")
        c.awaitOnline()
        val threadId = c.createThread(c.createProject("chaos-$seed"))
        val state = openLive(c, threadId)

        val stop = AtomicBoolean(false)
        val stats = AtomicInteger(0)
        val chaos = launch(Dispatchers.IO) {
            val rng = Random(seed)
            while (!stop.get()) {
                delay(rng.nextLong(100, 501))
                if (stop.get()) break
                when (rng.nextInt(4)) {
                    0 -> server.chaos(AasTestServer.Chaos.Drop)
                    1 -> server.chaos(AasTestServer.Chaos.Delay(rng.nextLong(50, 301)))
                    2 -> {
                        server.chaos(AasTestServer.Chaos.Blackhole)
                        delay(CLIENT_TIMEOUT_MS + rng.nextLong(100, 701))
                        server.chaos(AasTestServer.Chaos.Pass)
                    }
                    else -> server.chaos(AasTestServer.Chaos.Pass)
                }
                stats.incrementAndGet()
            }
            server.chaos(AasTestServer.Chaos.Pass)
        }

        val scripts = (0 until CHAOS_TURNS).map { CHAOS_SCRIPTS[((it + seed) % CHAOS_SCRIPTS.size).toInt()] }
        val answers = mutableMapOf<String, String>() // interaction → clientRequestId of its answer
        for (script in scripts) {
            val turnId = c.startTurn(threadId, script)
            val deadline = System.currentTimeMillis() + STEP_MS
            while (true) {
                if (System.currentTimeMillis() > deadline) throw AssertionError("turn '$script' did not end; ${c.describe()}")
                val done = state.value.turns.any { it.id == turnId && it.status.isTerminal }
                for (i in state.value.interactions) {
                    if (i.status == InteractionStatus.Pending && i.id !in answers) {
                        answers[i.id] = c.engine.enqueue(Methods.InteractionRespond) { crid ->
                            InteractionRespondParams(crid, i.id, InteractionResolution.Approval("allow"))
                        }
                    }
                }
                if (done) break
                delay(POLL_MS)
            }
        }
        stop.set(true)
        withTimeout(STEP_MS) { chaos.join() }

        c.waitFor("every request answered") { c.store.state.value.outbox.takeIf { it.isEmpty() } }
        c.assertThreadMatchesServer(threadId)
        c.assertWorkspaceMatchesServer()
        val local = state.value
        assertEquals(CHAOS_TURNS, local.turns.size, "exactly one turn per turn/start")
        assertTrue(local.turns.all { it.status == TurnStatus.Completed }, "${local.turns}")
        assertEquals(scripts, local.items.filterIsInstance<Item.UserMessage>().map { it.text }, "inputs neither lost nor duplicated")
        val bigOutputs = local.items.filterIsInstance<Item.CommandExecution>().count { it.outputTruncated && it.outputBlobId != null }
        assertEquals(scripts.count { it.startsWith("@bigoutput") }, bigOutputs)
        val approvals = scripts.count { it.startsWith("@approve") }
        assertEquals(approvals, answers.size, "one answer per approval")
        assertEquals(approvals, local.interactions.size)
        for (interaction in local.interactions) {
            assertEquals(InteractionStatus.Resolved, interaction.status)
            assertEquals(InteractionResolution.Approval("allow"), interaction.resolution)
            val crid = answers.getValue(interaction.id)
            val outcome = c.results.filter { it.entry.clientRequestId == crid }
            assertEquals(1, outcome.size)
            val result = assertIs<OutboxResult.Succeeded>(outcome.single()).result.jsonObject
            assertEquals("false", result["alreadyResolved"]!!.jsonPrimitive.content, "answered once, applied once")
        }
        println("chaos seed $seed: ${stats.get()} chaos steps, ${c.engine.status.value.reconnects} reconnects, " +
            "${c.sendsByRequestId("turn/start").values.sum()} turn/start sends for $CHAOS_TURNS turns")
        assertTrue(c.engine.status.value.reconnects >= 2, "the chaos forced reconnects: ${c.engine.status.value}")
    }

    companion object {
        const val HARNESS = "fake"
        const val HEARTBEAT_MS = 300L
        const val CLIENT_TIMEOUT_MS = 1_500L
        const val TEST_TIMEOUT_MINUTES = 6L
        const val SCENARIO_TIMEOUT_MS = 4 * 60_000L
        const val STEP_MS = 60_000L
        const val CONVERGE_MS = 60_000L
        const val AGENT_EXIT_GRACE_MS = 10_000L
        const val POLL_MS = 20L
        const val MAX_TURNS = 200
        const val CHAOS_TURNS = 10
        val CHAOS_SCRIPTS = listOf("@stream 200 2", "@approve some-cmd", "@bigoutput 100000")

        /** Short client policies: the test server's timeouts are short too. */
        val REAL_CONFIG = SyncConfig(
            connectTimeoutMs = 5_000,
            initialClientTimeoutMs = 5_000,
            callTimeoutMs = 60_000,
            backoffBaseMs = 50,
            backoffCapMs = 1_000,
            outboxRetryBaseMs = 200,
            outboxRetryCapMs = 1_000,
        )
    }
}
