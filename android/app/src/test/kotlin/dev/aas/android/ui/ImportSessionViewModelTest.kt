package dev.aas.android.ui

import dev.aas.android.AppPolicy
import dev.aas.android.R
import dev.aas.android.data.HarnessRepository
import dev.aas.android.data.ProjectRepository
import dev.aas.android.data.WorkspaceRepository
import dev.aas.android.protocol.AasJson
import dev.aas.android.protocol.ErrorKind
import dev.aas.android.protocol.Event
import dev.aas.android.protocol.Harness
import dev.aas.android.protocol.HarnessCapabilities
import dev.aas.android.protocol.Methods
import dev.aas.android.protocol.NativeImportParams
import dev.aas.android.protocol.NativeListResult
import dev.aas.android.protocol.NativeSession
import dev.aas.android.protocol.ProjectDefaults
import dev.aas.android.protocol.RpcError
import dev.aas.android.protocol.RpcMessage
import dev.aas.android.protocol.ThreadResult
import dev.aas.android.protocol.WORKSPACE_STREAM
import dev.aas.android.sync.Samples
import dev.aas.android.sync.eventually
import dev.aas.android.testing.MainDispatcherRule
import dev.aas.android.testing.TestEngine
import dev.aas.android.testing.blockingTest
import dev.aas.android.testing.clearViewModels
import dev.aas.android.ui.common.UiText
import dev.aas.android.ui.common.UserMessages
import dev.aas.android.ui.projects.Fetched
import dev.aas.android.ui.projects.ImportSessionUiState
import dev.aas.android.ui.projects.ImportSessionViewModel
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.Job
import kotlinx.coroutines.delay
import kotlinx.coroutines.flow.first
import kotlinx.coroutines.launch
import kotlinx.coroutines.withContext
import kotlinx.coroutines.withTimeout
import kotlinx.serialization.json.JsonElement
import kotlinx.serialization.json.JsonObject
import kotlinx.serialization.json.JsonPrimitive
import kotlinx.serialization.json.buildJsonObject
import kotlinx.serialization.json.jsonPrimitive
import org.junit.After
import org.junit.Rule
import org.junit.Test
import java.util.concurrent.CopyOnWriteArrayList
import kotlin.test.assertEquals
import kotlin.test.assertFalse
import kotlin.test.assertTrue

private suspend fun <T> onMain(block: () -> T): T = withContext(Dispatchers.Main) { block() }

/**
 * 「PC のセッションを取り込む」 against a scripted daemon: which harness is listed first, sessions a
 * harness lists twice, a harness whose listing fails (shown in place, the others selectable),
 * and importing or opening a session.
 */
class ImportSessionViewModelTest {
    @get:Rule
    val main = MainDispatcherRule()

    private val env = TestEngine()
    private val jobs = mutableListOf<Job>()
    private val viewModels = mutableListOf<ImportSessionViewModel>()
    private val messages = UserMessages()
    private val canList = HarnessCapabilities(nativeSessions = true)
    private val codex = Samples.harness("codex", capabilities = canList).copy(displayName = "Codex")
    private val claude = Samples.harness("claude", capabilities = canList).copy(displayName = "Claude")
    private val project = Samples.project("prj_1").copy(defaults = ProjectDefaults(harnessId = "claude"))

    /**
     * An ACP agent (Devin) that is unavailable, so its capabilities are unknown; once the server
     * probes it, it is available without `nativeSessions` (its ACP lacks session list / load).
     */
    private val devinDown = Samples.harness("devin", available = false, reason = "devin not found").copy(displayName = "Devin")
    private val devinUp = devinDown.copy(available = true, unavailableReason = null)

    /** `native/list` requests by harness id, in order. */
    private val listed = CopyOnWriteArrayList<String>()

    @After
    fun tearDown() {
        jobs.forEach { it.cancel() }
        clearViewModels(viewModels)
        env.close()
    }

    private fun harnessOf(msg: RpcMessage): String = (msg.params as JsonObject)["harnessId"]!!.jsonPrimitive.content

    /**
     * The daemon: [harnesses], the project, and `native/list` answered per harness by [answers],
     * after [beforeAnswer] (what the server does before it answers, e.g. publish its probe).
     */
    private suspend fun connect(answers: Map<String, Any>, harnesses: List<Harness> = listOf(codex, claude), beforeAnswer: (String) -> Unit = {}) {
        env.serve(null, harnesses = harnesses, projects = listOf(project))
        env.answers[Methods.NativeList.name] = { msg ->
            val harness = harnessOf(msg)
            listed += harness
            beforeAnswer(harness)
            answers.getValue(harness)
        }
        env.connect()
    }

    /** The server publishes its probe of [harness] (`harness/updated` on the workspace stream). */
    private fun harnessUpdated(harness: Harness) {
        env.server.append(WORKSPACE_STREAM, Event.HarnessUpdated(harness))
        env.server.lastConnection.pushNew(WORKSPACE_STREAM)
    }

    /** The daemon's answer for a harness that is available but cannot list its sessions. */
    private fun unsupported() = RpcError(
        ErrorKind.CapabilityUnsupported.code,
        "this harness cannot list its sessions",
        buildJsonObject {
            put("kind", JsonPrimitive("capabilityUnsupported"))
            put("capability", JsonPrimitive("nativeSessions"))
        },
    )

    private suspend fun message(): UiText = withTimeout(MESSAGE_TIMEOUT_MS) { messages.messages.first() }.text

    private fun sessions(vararg sessions: NativeSession): JsonElement = AasJson.encodeToJsonElement(NativeListResult.serializer(), NativeListResult(sessions.toList()))

    private fun unavailable(harnessId: String, reason: String) = RpcError(
        ErrorKind.HarnessUnavailable.code,
        "harness $harnessId is unavailable",
        buildJsonObject {
            put("kind", JsonPrimitive("harnessUnavailable"))
            put("harnessId", JsonPrimitive(harnessId))
            put("reason", JsonPrimitive(reason))
        },
    )

    private suspend fun viewModel(requested: String?): ImportSessionViewModel {
        val vm = onMain {
            ImportSessionViewModel(
                "prj_1", requested, ProjectRepository(env.engine, env.reads, env.lists), WorkspaceRepository(env.engine), messages,
                env.engine.outbox, HarnessRepository(env.engine), AppPolicy(),
            )
        }
        viewModels += vm
        jobs += CoroutineScope(Dispatchers.Default).launch { vm.state.collect {} }
        return vm
    }

    private suspend fun loaded(vm: ImportSessionViewModel, harnessId: String): List<NativeSession> =
        eventually(what = "the sessions of $harnessId") { vm.state.value.takeIf { it.selected == harnessId }?.sessions as? Fetched.Loaded }.value

    /** Codex lists a thread resumed elsewhere once per rollout (same id): each session is shown once, the latest copy. */
    @Test
    fun theThreadsHarnessIsListedFirstAndASessionListedTwiceIsShownOnce() = blockingTest {
        connect(
            mapOf(
                "codex" to sessions(
                    NativeSession("019a", title = "Fix the build", updatedAt = 100),
                    NativeSession("019b", title = "Plan", updatedAt = 200),
                    NativeSession("019a", title = "Fix the build", updatedAt = 300),
                ),
                "claude" to sessions(),
            ),
        )
        val vm = viewModel(requested = "codex")
        val shown = loaded(vm, "codex")
        assertEquals(listOf("019a" to 300L, "019b" to 200L), shown.map { it.nativeSessionId to it.updatedAt })
        assertEquals(listOf("codex"), listed, "the thread's harness, not the project's default, is listed first")
        assertTrue(env.dataWarnings.single().contains("019a"), "${env.dataWarnings}")
        assertEquals(listOf("codex", "claude"), vm.state.value.harnesses.map { it.id })
    }

    @Test
    fun withoutAThreadTheProjectsDefaultHarnessIsListedFirst() = blockingTest {
        connect(mapOf("codex" to sessions(), "claude" to sessions(NativeSession("c1", title = "From Claude"))))
        val vm = viewModel(requested = null)
        assertEquals(listOf("c1"), loaded(vm, "claude").map { it.nativeSessionId })
        assertEquals(listOf("claude"), listed)
    }

    @Test
    fun aHarnessWhoseListingFailsShowsWhyInPlaceAndTheOthersStaySelectable() = blockingTest {
        connect(
            mapOf(
                "claude" to unavailable("claude", "claude is not logged in"),
                "codex" to RpcError(ErrorKind.Internal.code, "codex app-server exited"),
                "pi" to sessions(NativeSession("p1")),
            ),
            harnesses = listOf(codex, claude, Samples.harness("pi", capabilities = canList).copy(displayName = "pi")),
        )
        val vm = viewModel(requested = "claude")
        val failed = eventually(what = "claude's failure") { vm.state.value.sessions as? Fetched.Failed }
        assertEquals(UiText.of(R.string.import_harness_unavailable, "Claude", "claude is not logged in"), failed.message)
        assertTrue(vm.state.value.harnessUnavailable, "再確認 is offered")

        // Switching works while the failure is shown, and a server error is shown the same way.
        onMain { vm.select("codex") }
        val serverError = eventually(what = "codex's failure") { vm.state.value.takeIf { it.selected == "codex" }?.sessions as? Fetched.Failed }
        assertEquals(UiText.of(R.string.error_server, "codex app-server exited"), serverError.message)
        assertEquals(false, vm.state.value.harnessUnavailable)
        onMain { vm.select("pi") }
        assertEquals(listOf("p1"), loaded(vm, "pi").map { it.nativeSessionId })
        assertEquals(listOf("claude", "codex", "pi"), listed)
    }

    /** A thread of a harness that is unavailable now: its chip stays, with why, next to the others. */
    @Test
    fun anUnavailableThreadHarnessIsListedFirstWithItsReason() = blockingTest {
        val claudeDown = claude.copy(available = false, unavailableReason = "claude not found on PATH", capabilities = HarnessCapabilities())
        connect(mapOf("claude" to unavailable("claude", "claude not found on PATH"), "codex" to sessions(NativeSession("x1"))), harnesses = listOf(codex, claudeDown))
        val vm = viewModel(requested = "claude")
        eventually(what = "claude's failure") { vm.state.value.sessions as? Fetched.Failed }
        assertEquals(listOf("codex", "claude"), vm.state.value.harnesses.map { it.id })
        onMain { vm.select("codex") }
        assertEquals(listOf("x1"), loaded(vm, "codex").map { it.nativeSessionId })
        assertEquals(listOf("codex", "claude"), vm.state.value.harnesses.map { it.id }, "the chip asked for stays")
    }

    @Test
    fun anImportedSessionOpensItsThreadAndAnotherIsImported() = blockingTest {
        connect(
            mapOf(
                "codex" to sessions(NativeSession("s_old", title = "Imported before", importedThreadId = "thr_old"), NativeSession("s_new", title = "New")),
                "claude" to sessions(),
            ),
        )
        env.answers[Methods.NativeImport.name] = { AasJson.encodeToJsonElement(ThreadResult.serializer(), ThreadResult(Samples.thread("thr_new", harnessId = "codex"))) }
        val vm = viewModel(requested = "codex")
        val opened = CopyOnWriteArrayList<String>()
        jobs += CoroutineScope(Dispatchers.Default).launch { vm.openThread.collect { opened += it } }
        val shown = loaded(vm, "codex")

        onMain { vm.import(shown.first { it.nativeSessionId == "s_old" }) }
        eventually(what = "the imported thread opened") { opened.firstOrNull() }
        assertEquals(listOf("thr_old"), opened)
        assertTrue(env.requests(Methods.NativeImport.name).isEmpty(), "an imported session is not imported again")

        onMain { vm.import(shown.first { it.nativeSessionId == "s_new" }) }
        eventually(what = "the new thread opened") { opened.takeIf { it.size == 2 } }
        assertEquals("thr_new", opened.last())
        val params = AasJson.decodeFromJsonElement(NativeImportParams.serializer(), env.requests(Methods.NativeImport.name).single().params!!)
        assertEquals(Triple("prj_1", "codex", "s_new"), Triple(params.projectId, params.harnessId, params.nativeSessionId))
        val finished = eventually(what = "the import finished") { vm.state.value.takeIf { it.importing == null } }
        assertEquals(null, finished.waiting)
    }

    /**
     * `/resume` in the thread of an ACP agent that was unavailable, so it was listed first. The
     * server probes it before answering, publishes it as available without `nativeSessions`, and
     * answers `capabilityUnsupported`. The screen lists the harness that can, and says why
     * (it used to keep showing the refusal with a 再試行 that always failed, with no chip to
     * switch: the only other choice left was Codex, and one choice hid the chips).
     */
    @Test
    fun aHarnessThatTurnsOutUnableToListGivesWayToOneThatCan() = blockingTest {
        connect(
            mapOf("devin" to unsupported(), "codex" to sessions(NativeSession("x1"))),
            harnesses = listOf(codex, devinDown),
            beforeAnswer = { if (it == "devin") harnessUpdated(devinUp) },
        )
        val vm = viewModel(requested = "devin")
        assertEquals(listOf("x1"), loaded(vm, "codex").map { it.nativeSessionId })
        assertEquals(UiText.of(R.string.import_harness_unsupported_switched, "Devin", "Codex"), message())
        assertEquals(listOf("devin", "codex"), listed, "Devin is not listed again")
        eventually(what = "Devin available in the workspace") { env.engine.workspace.value.harnesses.firstOrNull { it.id == "devin" && it.available } }
        val shown = eventually(what = "Devin no longer offered") { vm.state.value.takeIf { s -> s.harnesses.map { it.id } == listOf("codex") } }
        assertFalse(shown.canSwitch)
        assertEquals(setOf("devin"), shown.unable)
    }

    /**
     * The same refusal while the workspace still shows Devin unavailable (the server's
     * `harness/updated` has not arrived): Devin is neither listed again nor offered as a chip.
     */
    @Test
    fun theServersRefusalCountsBeforeTheWorkspaceShowsIt() = blockingTest {
        connect(mapOf("devin" to unsupported(), "codex" to sessions(NativeSession("x1"))), harnesses = listOf(codex, devinDown))
        val vm = viewModel(requested = "devin")
        assertEquals(listOf("x1"), loaded(vm, "codex").map { it.nativeSessionId })
        assertEquals(UiText.of(R.string.import_harness_unsupported_switched, "Devin", "Codex"), message())
        assertEquals(listOf("codex"), vm.state.value.harnesses.map { it.id })
        assertEquals(false, env.engine.workspace.value.harnesses.single { it.id == "devin" }.available)
        assertEquals(listOf("devin", "codex"), listed)
    }

    /**
     * No other harness can list: the refusal stays in place without 再試行 (the answer would be the
     * same); when `harness/updated` makes one able to, its sessions are listed.
     */
    @Test
    fun whenNoOtherHarnessCanListTheRefusalStaysUntilOneCan() = blockingTest {
        val codexDown = codex.copy(available = false, unavailableReason = "codex is not logged in", capabilities = HarnessCapabilities())
        connect(mapOf("devin" to unsupported(), "codex" to sessions(NativeSession("x1"))), harnesses = listOf(codexDown, devinDown))
        val vm = viewModel(requested = "devin")
        val refused = eventually(what = "Devin's refusal") { vm.state.value.takeIf { it.listingUnsupported } }
        assertEquals(Fetched.Failed(UiText.of(R.string.import_harness_unsupported, "Devin")), refused.sessions)
        assertEquals("devin", refused.selected)
        assertFalse(refused.harnessUnavailable)
        assertEquals(listOf("devin"), refused.harnesses.map { it.id })
        assertFalse(refused.canSwitch)

        // The user logs in to Codex on the PC; the server's probe publishes it.
        harnessUpdated(codex)
        assertEquals(listOf("x1"), loaded(vm, "codex").map { it.nativeSessionId })
        assertEquals(UiText.of(R.string.import_harness_unsupported_switched, "Devin", "Codex"), message())
        assertEquals(listOf("devin", "codex"), listed)
    }

    /**
     * The workspace shows both harnesses able to list, but the server refuses both (their
     * `harness/updated` has not arrived): the screen moves on once and stops, instead of listing
     * the two back and forth. Both stay choices (the workspace shows them importable).
     */
    @Test
    fun twoRefusalsNeverMakeTheScreenListBackAndForth() = blockingTest {
        connect(mapOf("codex" to unsupported(), "claude" to unsupported()))
        val vm = viewModel(requested = "codex")
        val refused = eventually(what = "Claude's refusal") { vm.state.value.takeIf { it.selected == "claude" && it.listingUnsupported } }
        assertEquals(UiText.of(R.string.import_harness_unsupported_switched, "Codex", "Claude"), message())
        assertEquals(setOf("codex", "claude"), refused.unable)
        assertEquals(listOf("codex", "claude"), refused.harnesses.map { it.id })
        delay(QUIET_MS)
        assertEquals(listOf("codex", "claude"), listed, "no listing after the second refusal")
        assertEquals("claude", vm.state.value.selected)
    }

    /**
     * Nothing can list at first, so nothing is selected; a harness that `harness/updated` makes
     * able to list is listed (the one chip it would be was hidden, with nothing selected).
     */
    @Test
    fun withoutAHarnessThatCanListSessionsTheScreenSaysSoUntilOneCan() = blockingTest {
        connect(mapOf("codex" to sessions(NativeSession("x1"))), harnesses = listOf(Samples.harness("acp")))
        val vm = viewModel(requested = "acp")
        eventually(what = "nothing to list") { vm.state.value.sessions as? Fetched.Loaded }
        assertEquals(null, vm.state.value.selected)
        assertTrue(listed.isEmpty())

        harnessUpdated(codex)
        assertEquals(listOf("x1"), loaded(vm, "codex").map { it.nativeSessionId })
        assertEquals(listOf("codex"), listed)
    }

    @Test
    fun theChipsAreShownWheneverAnotherHarnessThanTheListedOneIsOffered() {
        val state = ImportSessionUiState(listOf(codex), selected = null, sessions = Fetched.Loaded(emptyList()), importing = null)
        assertTrue(state.canSwitch, "nothing listed: the one harness offered is a chip")
        assertFalse(state.copy(selected = "codex").canSwitch)
        assertTrue(state.copy(harnesses = listOf(codex, devinUp), selected = "devin").canSwitch)
        assertTrue(state.copy(selected = "gone").canSwitch, "the listed harness is no longer offered")
        assertFalse(state.copy(harnesses = emptyList()).canSwitch)
    }

    private companion object {
        const val MESSAGE_TIMEOUT_MS = 10_000L

        /** Long enough for a listing the screen would start by itself to reach the scripted daemon. */
        const val QUIET_MS = 500L
    }
}
