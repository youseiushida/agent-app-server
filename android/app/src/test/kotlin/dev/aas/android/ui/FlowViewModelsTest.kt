package dev.aas.android.ui

import androidx.lifecycle.viewModelScope
import dev.aas.android.AppPolicy
import dev.aas.android.DiffViewPolicy
import dev.aas.android.R
import dev.aas.android.data.BlobCache
import dev.aas.android.data.BlobRepository
import dev.aas.android.data.ComposerDrafts
import dev.aas.android.data.HarnessRepository
import dev.aas.android.data.ProjectRepository
import dev.aas.android.data.ThreadRepository
import dev.aas.android.domain.diff.DiffLine
import dev.aas.android.protocol.AasJson
import dev.aas.android.protocol.DiffFile
import dev.aas.android.protocol.DiffSummary
import dev.aas.android.protocol.ErrorKind
import dev.aas.android.protocol.Event
import dev.aas.android.protocol.FileChangeKind
import dev.aas.android.protocol.FsListResult
import dev.aas.android.protocol.FsRootsResult
import dev.aas.android.protocol.InputPart
import dev.aas.android.protocol.Methods
import dev.aas.android.protocol.Operation
import dev.aas.android.protocol.OperationCancelParams
import dev.aas.android.protocol.OperationKind
import dev.aas.android.protocol.OperationStatus
import dev.aas.android.protocol.ProjectCreateParams
import dev.aas.android.protocol.ProjectCreateResult
import dev.aas.android.protocol.ProjectDefaults
import dev.aas.android.protocol.ProjectInit
import dev.aas.android.protocol.ProjectOpenParams
import dev.aas.android.protocol.ProjectResult
import dev.aas.android.protocol.ProjectUpdateParams
import dev.aas.android.protocol.ThreadCreateParams
import dev.aas.android.protocol.ThreadCreateResult
import dev.aas.android.protocol.ThreadDiffResult
import dev.aas.android.protocol.ThreadSettings
import dev.aas.android.protocol.WORKSPACE_STREAM
import dev.aas.android.protocol.WorkspaceSpec
import dev.aas.android.sync.FakeServer
import dev.aas.android.sync.Samples
import dev.aas.android.sync.eventually
import dev.aas.android.testing.FakeUploader
import dev.aas.android.testing.Fixtures
import dev.aas.android.testing.MainDispatcherRule
import dev.aas.android.testing.TestEngine
import dev.aas.android.testing.blockingTest
import dev.aas.android.ui.common.UiText
import dev.aas.android.ui.common.UserMessages
import dev.aas.android.ui.composer.PromptTemplates
import dev.aas.android.ui.diff.DiffContent
import dev.aas.android.ui.diff.DiffScopeChoice
import dev.aas.android.ui.diff.DiffViewModel
import dev.aas.android.ui.diff.FileNote
import dev.aas.android.ui.navigation.DiffRoute
import dev.aas.android.ui.navigation.NewThreadRoute
import dev.aas.android.ui.newproject.BrowsePurpose
import dev.aas.android.ui.newproject.InitKind
import dev.aas.android.ui.newproject.Listing
import dev.aas.android.ui.newproject.NewProjectEvent
import dev.aas.android.ui.newproject.NewProjectStep
import dev.aas.android.ui.newproject.NewProjectViewModel
import dev.aas.android.ui.newthread.NewThreadEvent
import dev.aas.android.ui.newthread.NewThreadViewModel
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.Job
import kotlinx.coroutines.cancel
import kotlinx.coroutines.flow.first
import kotlinx.coroutines.launch
import kotlinx.coroutines.withContext
import kotlinx.serialization.json.JsonElement
import kotlinx.serialization.json.JsonObject
import org.junit.After
import org.junit.Rule
import org.junit.Test
import org.junit.rules.TemporaryFolder
import kotlin.test.assertEquals
import kotlin.test.assertIs
import kotlin.test.assertTrue

private suspend fun <T> onMain(block: () -> T): T = withContext(Dispatchers.Main) { block() }

private fun <T> json(serializer: kotlinx.serialization.KSerializer<T>, value: T): JsonElement = AasJson.encodeToJsonElement(serializer, value)

/** The new-project flow against a scripted daemon: browse, open, create, clone with progress and cancel. */
class NewProjectViewModelTest {
    @get:Rule
    val main = MainDispatcherRule()

    private val env = TestEngine()
    private val jobs = mutableListOf<Job>()
    private val events = mutableListOf<NewProjectEvent>()
    private val roots = Fixtures.result("fs_roots", FsRootsResult.serializer())
    private val listing = Fixtures.result("fs_list", FsListResult.serializer())
    private val project = Samples.project("prj_new", name = "new-app")

    private suspend fun viewModel(): NewProjectViewModel {
        val vm = onMain { NewProjectViewModel(ProjectRepository(env.engine, env.reads, env.lists), env.engine.workspace) }
        jobs += CoroutineScope(Dispatchers.Default).launch { vm.eventFlow.collect { events += it } }
        return vm
    }

    private fun clone(status: OperationStatus, progress: String? = null, projectId: String? = null, message: String? = null, id: String = "op_1") =
        Operation(id, OperationKind.GitClone, status, projectId = projectId, message = message, progress = progress, startedAt = 1, finishedAt = if (status.isTerminal) 2 else null)

    private fun push(operation: Operation) {
        env.server.append(WORKSPACE_STREAM, Event.OperationUpdated(operation))
        env.server.lastConnection.pushNew(WORKSPACE_STREAM)
    }

    @After
    fun tearDown() {
        jobs.forEach { it.cancel() }
        env.close()
    }

    /** `fs/list` of the fixture: its entries at the root, nothing below; the path is echoed. */
    private fun listAnswer(msg: dev.aas.android.protocol.RpcMessage): JsonElement {
        val path = ((msg.params as JsonObject)["path"] as kotlinx.serialization.json.JsonPrimitive).content
        val entries = if (path == listing.path) listing.entries else emptyList()
        return json(FsListResult.serializer(), FsListResult(path, entries))
    }

    @Test
    fun anExistingFolderIsBrowsedAndOpened() = blockingTest {
        env.serve(null)
        env.answers[Methods.FsRoots.name] = { json(FsRootsResult.serializer(), roots) }
        env.answers[Methods.FsList.name] = ::listAnswer
        env.answers[Methods.ProjectOpen.name] = { json(ProjectResult.serializer(), ProjectResult(project)) }
        env.connect()
        val vm = viewModel()

        onMain { vm.chooseExisting() }
        val rootRows = eventually(what = "roots") { ((vm.state.value.step as? NewProjectStep.Browse)?.listing as? Listing.Loaded)?.entries }
        assertEquals(listOf("C:\\Users\\me\\Documents"), rootRows.map { it.path })
        onMain { vm.open(rootRows.single().path) }
        val folders = eventually(what = "folders") {
            (vm.state.value.step as? NewProjectStep.Browse)?.takeIf { it.path != null }?.let { it.listing as? Listing.Loaded }?.entries
        }
        assertEquals(listOf("agent-app-server"), folders.map { it.name })
        onMain { vm.open(folders.single().path) }
        eventually(what = "inside the repository") { (vm.state.value.step as? NewProjectStep.Browse)?.takeIf { it.path == folders.single().path && it.listing is Listing.Loaded } }
        // Up returns to the root; up from the root to the list of roots.
        onMain { vm.up() }
        eventually(what = "the root again") { (vm.state.value.step as? NewProjectStep.Browse)?.takeIf { it.path == roots.roots.single().path } }
        onMain { vm.open(folders.single().path) }
        eventually(what = "loaded") { (vm.state.value.step as? NewProjectStep.Browse)?.takeIf { it.path == folders.single().path && it.listing is Listing.Loaded } }
        onMain { vm.openCurrentFolder() }
        assertEquals(NewProjectEvent.Created("prj_new", startThread = true), eventually(what = "created") { events.firstOrNull() })
        val params = AasJson.decodeFromJsonElement(ProjectOpenParams.serializer(), env.requests(Methods.ProjectOpen.name).single().params!!)
        assertEquals(folders.single().path, params.path)
    }

    @Test
    fun aCloneShowsGitProgressCanBeCancelledAndRetried() = blockingTest {
        env.serve(null)
        env.answers[Methods.FsRoots.name] = { json(FsRootsResult.serializer(), roots) }
        env.answers[Methods.FsList.name] = ::listAnswer
        val creates = java.util.concurrent.atomic.AtomicInteger()
        env.answers[Methods.ProjectCreate.name] = {
            json(ProjectCreateResult.serializer(), ProjectCreateResult(operation = clone(OperationStatus.Running, id = "op_${creates.incrementAndGet()}")))
        }
        env.answers[Methods.OperationCancel.name] = { json(dev.aas.android.protocol.OperationResult.serializer(), dev.aas.android.protocol.OperationResult(clone(OperationStatus.Cancelled))) }
        env.connect()
        val vm = viewModel()

        onMain {
            vm.chooseNew()
            vm.setKind(InitKind.GitClone)
            vm.setUrl("https://github.com/example/new-app.git")
        }
        val details = vm.state.value.step as NewProjectStep.Details
        // The folder name follows the URL until the user types one.
        assertEquals("new-app", details.name)
        assertTrue(details.canContinue)
        onMain { vm.toLocation() }
        val rootPath = eventually(what = "roots") { ((vm.state.value.step as? NewProjectStep.Browse)?.listing as? Listing.Loaded)?.entries?.single()?.path }
        assertEquals(BrowsePurpose.Location, (vm.state.value.step as NewProjectStep.Browse).purpose)
        onMain { vm.open(rootPath) }
        eventually(what = "root listed") { (vm.state.value.step as? NewProjectStep.Browse)?.takeIf { it.path == rootPath && it.listing is Listing.Loaded } }
        onMain { vm.createHere() }
        eventually(what = "cloning") { vm.state.value.step as? NewProjectStep.Cloning }
        val create = AasJson.decodeFromJsonElement(ProjectCreateParams.serializer(), env.requests(Methods.ProjectCreate.name).single().params!!)
        assertEquals(rootPath, create.parentPath)
        assertEquals("new-app", create.name)
        assertEquals(ProjectInit.GitClone("https://github.com/example/new-app.git"), create.init)

        // Progress lines arrive as operation/updated and are shown as they are.
        push(clone(OperationStatus.Running, progress = "Receiving objects:  42% (420/1000)"))
        eventually(what = "progress") { (vm.state.value.step as? NewProjectStep.Cloning)?.operation?.progress?.takeIf { it.startsWith("Receiving") } }
        onMain { vm.cancelClone() }
        val cancel = eventually(what = "operation/cancel") { env.requests(Methods.OperationCancel.name).firstOrNull() }
        assertEquals("op_1", AasJson.decodeFromJsonElement(OperationCancelParams.serializer(), cancel.params!!).operationId)
        push(clone(OperationStatus.Cancelled, message = "Cancelled"))
        val ended = eventually(what = "ended") { vm.state.value.step as? NewProjectStep.CloneEnded }
        assertEquals(OperationStatus.Cancelled, ended.operation.status)

        // Retry: the same clone again, this time it succeeds and names its project.
        onMain { vm.retryClone() }
        eventually(what = "second create") { env.requests(Methods.ProjectCreate.name).takeIf { it.size == 2 } }
        eventually(what = "cloning again") { vm.state.value.step as? NewProjectStep.Cloning }
        push(clone(OperationStatus.Succeeded, projectId = "prj_new", id = "op_2"))
        assertEquals(NewProjectEvent.Created("prj_new", startThread = true), eventually(what = "created") { events.firstOrNull() })
    }

    @Test
    fun errorsAndOfflineAreShownInPlace() = blockingTest {
        env.serve(null)
        env.answers[Methods.FsRoots.name] = { json(FsRootsResult.serializer(), roots) }
        env.answers[Methods.FsList.name] = { msg ->
            val path = (msg.params as JsonObject)["path"].toString()
            if (path.contains("nowhere")) FakeServer.rpcError(ErrorKind.PathNotAllowed) else listAnswer(msg)
        }
        env.answers[Methods.ProjectCreate.name] = { FakeServer.rpcError(ErrorKind.AlreadyExists) }
        env.connect()
        val vm = viewModel()
        onMain {
            vm.chooseNew()
            vm.setName("bad/name")
        }
        assertEquals(dev.aas.android.domain.NameProblem.InvalidCharacters, (vm.state.value.step as NewProjectStep.Details).nameProblem)
        onMain { vm.setName("taken") }
        onMain { vm.toLocation() }
        val rootPath = eventually(what = "roots") { ((vm.state.value.step as? NewProjectStep.Browse)?.listing as? Listing.Loaded)?.entries?.single()?.path }
        onMain { vm.openTyped("C:\\nowhere") }
        val failed = eventually(what = "not allowed") { (vm.state.value.step as? NewProjectStep.Browse)?.listing as? Listing.Failed }
        assertEquals(UiText.of(R.string.newproject_path_not_allowed), failed.message)
        onMain { vm.open(rootPath) }
        eventually(what = "root") { (vm.state.value.step as? NewProjectStep.Browse)?.takeIf { it.path == rootPath && it.listing is Listing.Loaded } }
        onMain { vm.createHere() }
        val error = eventually(what = "already exists") { vm.state.value.error }
        assertEquals(UiText.of(R.string.newproject_already_exists), error)
        // The flow stays where it was, ready for another name.
        assertIs<NewProjectStep.Browse>(vm.state.value.step)
        assertTrue(onMain { vm.back() })
    }

    /** Browses to the fixture's root folder while online, then the phone loses its network. */
    private suspend fun atTheRootThenOffline(vm: NewProjectViewModel): String {
        onMain { vm.chooseExisting() }
        val rootPath = eventually(what = "roots") { ((vm.state.value.step as? NewProjectStep.Browse)?.listing as? Listing.Loaded)?.entries?.single()?.path }
        onMain { vm.open(rootPath) }
        eventually(what = "the root listed") { (vm.state.value.step as? NewProjectStep.Browse)?.takeIf { it.path == rootPath && it.listing is Listing.Loaded } }
        env.engine.onNetworkLost()
        eventually(what = "offline") { env.engine.status.value.connection.takeIf { it == dev.aas.android.sync.ConnectionState.Offline } }
        return rootPath
    }

    /**
     * A change that cannot be sent (offline here; the same while the daemon drains or is stopped)
     * waits in the outbox, possibly for hours. Back leaves the flow instead of doing nothing, and
     * 送信を取り消す takes the request back: never sent, the flow is at the folder again.
     */
    @Test
    fun aWaitingChangeDoesNotTrapTheUser() = blockingTest {
        env.serve(null)
        env.answers[Methods.FsRoots.name] = { json(FsRootsResult.serializer(), roots) }
        env.answers[Methods.FsList.name] = ::listAnswer
        env.connect()
        val vm = viewModel()
        val rootPath = atTheRootThenOffline(vm)

        onMain { vm.openCurrentFolder() }
        val working = eventually(what = "waiting, in the outbox") { (vm.state.value.step as? NewProjectStep.Working)?.takeIf { it.clientRequestId != null } }
        assertEquals(UiText.of(R.string.newproject_working_note_project), working.note)
        assertEquals(listOf(Methods.ProjectOpen.name), env.engine.outbox.value.map { it.method })
        assertEquals(false, onMain { vm.back() }, "back leaves the flow (the screen closes)")

        onMain { vm.discardWork() }
        eventually(what = "back at the folder") { (vm.state.value.step as? NewProjectStep.Browse)?.takeIf { it.path == rootPath } }
        assertTrue(env.engine.outbox.value.isEmpty())
        env.engine.onNetworkAvailable()
        eventually(what = "online again") { env.engine.status.value.isOnline.takeIf { it } }
        assertTrue(env.requests(Methods.ProjectOpen.name).isEmpty(), "a request taken back is never sent")
    }

    /** Leaving while a change waits: it stays in the outbox and is sent once connected. */
    @Test
    fun aChangeLeftWaitingIsStillSent() = blockingTest {
        env.serve(null)
        env.answers[Methods.FsRoots.name] = { json(FsRootsResult.serializer(), roots) }
        env.answers[Methods.FsList.name] = ::listAnswer
        env.answers[Methods.ProjectOpen.name] = { json(ProjectResult.serializer(), ProjectResult(project)) }
        env.connect()
        val vm = viewModel()
        val rootPath = atTheRootThenOffline(vm)

        onMain { vm.openCurrentFolder() }
        eventually(what = "waiting, in the outbox") { (vm.state.value.step as? NewProjectStep.Working)?.clientRequestId }
        assertEquals(false, onMain { vm.back() })
        // The screen closes: its view model is cleared.
        onMain { vm.viewModelScope.cancel() }
        env.engine.onNetworkAvailable()
        val sent = eventually(what = "project/open at the daemon") { env.requests(Methods.ProjectOpen.name).singleOrNull() }
        assertEquals(rootPath, AasJson.decodeFromJsonElement(ProjectOpenParams.serializer(), sent.params!!).path)
        val outbox = eventually(what = "answered") { env.engine.outbox.value.takeIf { it.isEmpty() } }
        assertTrue(outbox.isEmpty())
    }

    @Test
    fun offlineTheBrowserSaysSo() = blockingTest {
        env.startOffline()
        val vm = viewModel()
        onMain { vm.chooseExisting() }
        eventually(what = "offline") { ((vm.state.value.step as? NewProjectStep.Browse)?.listing as? Listing.Offline) }
        // Back from the roots returns to the choice; back from the choice leaves the flow.
        assertTrue(onMain { vm.back() })
        assertEquals(NewProjectStep.Choose, vm.state.value.step)
        assertEquals(false, onMain { vm.back() })
    }
}

/** The new-thread sheet: starting values from the project, and `thread/create` with the first message. */
class NewThreadViewModelTest {
    @get:Rule
    val main = MainDispatcherRule()

    private val env = TestEngine()
    private val jobs = mutableListOf<Job>()
    private val viewModels = mutableListOf<NewThreadViewModel>()
    private val harness = TestEngine.fakeHarness()
    private val project = Samples.project("prj_1").copy(
        defaults = ProjectDefaults(harnessId = "fake", model = "large", effort = "high", permissionMode = "full"),
        git = dev.aas.android.protocol.GitInfo(isRepo = true, branch = "main"),
    )
    private val drafts = ComposerDrafts()
    private val messages = UserMessages()

    private companion object {
        /** Upper bound for a view model's message to be posted. */
        const val MESSAGE_TIMEOUT_MS = 10_000L
    }

    private suspend fun viewModel(route: NewThreadRoute = NewThreadRoute("prj_1")): NewThreadViewModel {
        val vm = onMain {
            NewThreadViewModel(route, env.engine.workspace, ThreadRepository(env.engine, env.reads, env.lists), ProjectRepository(env.engine, env.reads, env.lists), env.engine.status, messages, drafts, FakeUploader(), AppPolicy(), PromptTemplates("R", "I"), env.engine.outbox, HarnessRepository(env.engine))
        }
        viewModels += vm
        jobs += CoroutineScope(Dispatchers.Default).launch { vm.state.collect {} }
        eventually(what = "harness") { vm.state.value.harness }
        return vm
    }

    @After
    fun tearDown() {
        jobs.forEach { it.cancel() }
        dev.aas.android.testing.clearViewModels(viewModels)
        env.close()
    }

    /**
     * `harnessUnavailable` for `thread/create`: the creation waits (not retried on a timer) and
     * the sheet says for which harness and why; 再確認 probes it, and withdrawing the creation
     * brings the message back to the composer.
     */
    @Test
    fun aCreationForAnUnavailableHarnessWaitsVisiblyAndCanBeWithdrawn() = blockingTest {
        env.serve(null, harnesses = listOf(harness), projects = listOf(project))
        env.answers[Methods.ThreadCreate.name] = {
            dev.aas.android.protocol.RpcError(
                dev.aas.android.protocol.ErrorKind.HarnessUnavailable.code,
                "harness fake is unavailable",
                kotlinx.serialization.json.buildJsonObject {
                    put("kind", kotlinx.serialization.json.JsonPrimitive("harnessUnavailable"))
                    put("harnessId", kotlinx.serialization.json.JsonPrimitive("fake"))
                    put("reason", kotlinx.serialization.json.JsonPrimitive("not logged in"))
                },
            )
        }
        env.answers[Methods.HarnessRefresh.name] = {
            json(dev.aas.android.protocol.HarnessListResult.serializer(), dev.aas.android.protocol.HarnessListResult(listOf(harness.copy(available = false, unavailableReason = "not logged in"))))
        }
        env.connect()
        val vm = viewModel()
        onMain { vm.composer.setText("Build the thing") }
        eventually(what = "sendable") { vm.state.value.send.enabled.takeIf { it } }
        onMain { vm.create() }
        val wait = eventually(what = "waiting for the harness") { vm.state.value.waiting }
        assertEquals("fake", wait.harnessId)
        assertEquals("not logged in", wait.reason)
        assertEquals(1, env.requests(Methods.ThreadCreate.name).size)

        // 再確認 asks the server to probe it again.
        onMain { vm.refreshHarness(wait.harnessId) }
        eventually(what = "harness/refresh") { env.requests(Methods.HarnessRefresh.name).singleOrNull() }
        val refreshed = kotlinx.coroutines.withTimeout(MESSAGE_TIMEOUT_MS) { messages.messages.first() }
        assertEquals(UiText.of(R.string.harness_still_unavailable, harness.displayName, "not logged in"), refreshed.text)

        onMain { vm.discardCreation() }
        eventually(what = "the draft back") { vm.composer.state.value.value.text.takeIf { it == "Build the thing" } }
        eventually(what = "no longer waiting") { vm.state.value.takeIf { it.waiting == null && !it.creating } }
        assertEquals(1, env.requests(Methods.ThreadCreate.name).size, "never resent while the harness is unavailable")
    }

    /** `/resume` in the first message: the import of this project with the chosen harness, never a thread. */
    @Test
    fun resumeOpensTheImportInsteadOfCreatingAThread() = blockingTest {
        val importing = harness.copy(capabilities = harness.capabilities.copy(nativeSessions = true))
        env.serve(null, harnesses = listOf(importing), projects = listOf(project))
        env.connect()
        val vm = viewModel()
        val events = java.util.concurrent.CopyOnWriteArrayList<NewThreadEvent>()
        jobs += CoroutineScope(Dispatchers.Default).launch { vm.eventFlow.collect { events += it } }

        onMain { vm.composer.setText("/resu") }
        val entry = eventually(what = "/resume in the palette") { vm.composer.state.value.palette.firstOrNull { it.name == "resume" } }
        onMain { vm.choose(entry) }
        assertEquals(NewThreadEvent.OpenImport("prj_1", "fake"), eventually(what = "the import opened") { events.firstOrNull() })

        onMain {
            vm.composer.setText("/resume")
            vm.create()
        }
        eventually(what = "the import opened again") { events.takeIf { it.size == 2 } }
        assertEquals(NewThreadEvent.OpenImport("prj_1", "fake"), events[1])
        assertEquals("", vm.composer.textValue.text)
        assertTrue(env.requests(Methods.ThreadCreate.name).isEmpty())
        assertTrue(env.engine.outbox.value.isEmpty(), "${env.engine.outbox.value}")
    }

    @Test
    fun theSheetStartsFromTheProjectsLastChoicesAndCreatesWithTheFirstMessage() = blockingTest {
        env.serve(null, harnesses = listOf(harness), projects = listOf(project))
        val created = Samples.thread("thr_new")
        env.answers[Methods.ThreadCreate.name] = { json(ThreadCreateResult.serializer(), ThreadCreateResult(created, turnId = "trn_1", disposition = dev.aas.android.protocol.Disposition.Started)) }
        env.answers[Methods.ProjectUpdate.name] = { json(ProjectResult.serializer(), ProjectResult(project)) }
        env.connect()
        val vm = viewModel()
        val events = mutableListOf<NewThreadEvent>()
        jobs += CoroutineScope(Dispatchers.Default).launch { vm.eventFlow.collect { events += it } }

        assertEquals("fake", vm.state.value.harness?.id)
        eventually(what = "initial settings") { vm.state.value.choices.settings.takeIf { it == ThreadSettings("large", "high", "full") } }
        assertTrue(vm.state.value.worktreeAvailable)
        // A thread needs a message.
        assertEquals(false, vm.state.value.send.enabled)
        onMain {
            vm.setModel("small", null)
            vm.setWorktree(true)
            vm.setBranch("feature/x")
            vm.composer.setText("Build the thing")
        }
        eventually(what = "enabled") { vm.state.value.send.takeIf { it.enabled } }
        onMain { vm.create() }
        assertEquals(NewThreadEvent.Created("thr_new"), eventually(what = "created") { events.firstOrNull { it is NewThreadEvent.Created } })
        val params = AasJson.decodeFromJsonElement(ThreadCreateParams.serializer(), env.requests(Methods.ThreadCreate.name).single().params!!)
        assertEquals("prj_1", params.projectId)
        assertEquals("fake", params.harnessId)
        assertEquals(ThreadSettings("small", null, "full"), params.settings)
        assertEquals(WorkspaceSpec.Worktree(baseRef = null, branch = "feature/x"), params.workspace)
        assertEquals(listOf(InputPart.Text("Build the thing")), params.input)
        // The project remembers these choices for the next thread.
        val update = eventually(what = "project/update") { env.requests(Methods.ProjectUpdate.name).firstOrNull() }
        assertEquals(ProjectDefaults("fake", "small", null, "full"), AasJson.decodeFromJsonElement(ProjectUpdateParams.serializer(), update.params!!).defaults)
        // The draft is gone.
        assertEquals("", drafts.get(ComposerDrafts.newThreadKey("prj_1")).text)
    }

    @Test
    fun leavingWhileTheCreationIsQueuedStillQueuesTheProjectDefaults() = blockingTest {
        env.store.transaction { tx ->
            tx.setEpoch("e")
            tx.setCursor(WORKSPACE_STREAM, 0)
            tx.replaceHarnesses(listOf(harness))
            tx.upsertProject(project)
        }
        env.startOffline()
        val vm = viewModel()
        onMain {
            vm.setModel("small", null)
            vm.composer.setText("offline start")
        }
        eventually(what = "enabled") { vm.state.value.send.takeIf { it.enabled } }
        // Offline, the screen says the creation is queued; the user goes back at once.
        onMain {
            vm.create()
            vm.viewModelScope.cancel()
        }
        val methods = eventually(what = "both requests in the outbox") {
            env.engine.outbox.value.map { it.method }.takeIf { it.size == 2 }
        }
        assertEquals(listOf(Methods.ThreadCreate.name, Methods.ProjectUpdate.name), methods, "the defaults after the creation, in the project's lane")
        val update = AasJson.decodeFromJsonElement(ProjectUpdateParams.serializer(), env.engine.outbox.value[1].params)
        assertEquals(ProjectDefaults("fake", "small", null, "full"), update.defaults)
    }

    @Test
    fun aRefusedCreationBringsBackTheTextItsMentionsAndItsImages() = blockingTest {
        env.serve(null, harnesses = listOf(harness), projects = listOf(project))
        env.answers[Methods.ThreadCreate.name] = { dev.aas.android.sync.FakeServer.rpcError(dev.aas.android.protocol.ErrorKind.InvalidState, "the project folder is not a git repository") }
        env.answers[Methods.FsSearch.name] = { json(dev.aas.android.protocol.FsSearchResult.serializer(), dev.aas.android.protocol.FsSearchResult(listOf(dev.aas.android.protocol.SearchResult("src/a.rs", false)), "heuristic:H1")) }
        env.connect()
        val vm = viewModel()
        onMain { vm.composer.onValueChange(androidx.compose.ui.text.input.TextFieldValue("look at @sr", androidx.compose.ui.text.TextRange(11))) }
        eventually(what = "mention results") { (vm.composer.state.value.mentionSearch as? dev.aas.android.ui.composer.MentionSearch.Results)?.results?.takeIf { it.isNotEmpty() } }
        onMain {
            vm.composer.chooseMention("src/a.rs")
            vm.pickImages(listOf("content://media/1", "content://media/2"))
        }
        eventually(what = "uploaded") { vm.composer.state.value.uploaded.takeIf { it.size == 2 } }
        eventually(what = "enabled") { vm.state.value.send.takeIf { it.enabled } }
        onMain { vm.create() }
        val sent = eventually(what = "thread/create") { env.requests(Methods.ThreadCreate.name).firstOrNull() }
        val input = AasJson.decodeFromJsonElement(ThreadCreateParams.serializer(), sent.params!!).input!!
        assertEquals(listOf(InputPart.Text("look at "), InputPart.Mention("src/a.rs")), input.take(2))
        // Refused (definitive): everything comes back to edit, the images already uploaded.
        eventually(what = "restored") { vm.composer.state.value.takeIf { it.value.text == "look at @src/a.rs " } }
        assertEquals(setOf("src/a.rs"), vm.composer.state.value.mentions)
        val images = vm.composer.state.value.attachments
        assertEquals(listOf("content://media/1", "content://media/2"), images.map { it.localUri })
        assertTrue(images.all { it.state is dev.aas.android.ui.composer.Attachment.State.Uploaded })
        assertEquals(2, drafts.get(ComposerDrafts.newThreadKey("prj_1")).images.size, "the restored draft is kept")
        assertEquals(false, eventually(what = "not creating") { vm.state.value.takeIf { !it.creating } }.creating)
    }

    @Test
    fun switchingToAHarnessWithoutImagesBlocksTheAttachedOnes() = blockingTest {
        val textOnly = harness.copy(id = "pi", displayName = "pi", capabilities = harness.capabilities.copy(images = false))
        env.serve(null, harnesses = listOf(harness, textOnly), projects = listOf(project))
        env.connect()
        val vm = viewModel()
        onMain {
            vm.composer.setText("with a screenshot")
            vm.pickImages(listOf("content://media/1"))
        }
        eventually(what = "enabled") { vm.state.value.send.takeIf { it.enabled } }
        onMain { vm.selectHarness(textOnly) }
        eventually(what = "blocked") { vm.state.value.send.takeIf { it.blocked == dev.aas.android.domain.composer.SendBlock.ImagesUnsupported } }
        onMain { vm.create() }
        kotlinx.coroutines.delay(200)
        assertTrue(env.engine.outbox.value.isEmpty(), "nothing is sent the daemon would refuse")
        onMain { vm.composer.removeAttachment(vm.composer.state.value.attachments.single().id) }
        assertTrue(eventually(what = "enabled again") { vm.state.value.send.takeIf { it.enabled } }.enabled)
    }

    @Test
    fun anotherHarnessStartsFromItsOwnDefaultsAndOfflineTheRequestWaits() = blockingTest {
        env.store.transaction { tx ->
            tx.setEpoch("e")
            tx.setCursor(WORKSPACE_STREAM, 0)
            tx.replaceHarnesses(listOf(harness, harness.copy(id = "other", displayName = "Other")))
            tx.upsertProject(project)
        }
        env.startOffline()
        val vm = viewModel(NewThreadRoute("prj_1", harnessId = "other"))
        // The route's harness (from /new), with its own defaults: the project's are for "fake".
        assertEquals("other", vm.state.value.harness?.id)
        assertEquals(ThreadSettings(), vm.state.value.choices.settings)
        onMain { vm.selectHarness(harness) }
        eventually(what = "fake") { vm.state.value.choices.settings.takeIf { it.model == "large" } }
        onMain { vm.composer.setText("offline start") }
        eventually(what = "enabled") { vm.state.value.send.takeIf { it.enabled } }
        onMain { vm.create() }
        val entry = eventually(what = "thread/create in the outbox") { env.engine.outbox.value.firstOrNull { it.method == Methods.ThreadCreate.name } }
        val params = AasJson.decodeFromJsonElement(ThreadCreateParams.serializer(), entry.params)
        assertEquals(WorkspaceSpec.Local, params.workspace)
        assertTrue(vm.state.value.creating)
    }
}

/** The diff viewer's loading and shaping of `thread/diff`. */
class DiffViewModelTest {
    @get:Rule
    val main = MainDispatcherRule()

    @get:Rule
    val temp = TemporaryFolder()

    private val env = TestEngine()
    private val drafts = ComposerDrafts()
    private val patch = "diff --git a/a.kt b/a.kt\n--- a/a.kt\n+++ b/a.kt\n@@ -1,2 +1,2 @@\n x\n-y\n+z\n" +
        "diff --git a/img.png b/img.png\nBinary files a/img.png and b/img.png differ\n"
    private val files = listOf(DiffFile("a.kt", FileChangeKind.Update, 1, 1, false), DiffFile("img.png", FileChangeKind.Update, 0, 0, true), DiffFile("gone.kt", FileChangeKind.Delete, 0, 3, false))

    private suspend fun viewModel(policy: DiffViewPolicy = DiffViewPolicy(), turnId: String? = "trn_1"): DiffViewModel {
        val cache = BlobCache(temp.newFolder("blobs"), 1024 * 1024, Dispatchers.IO)
        cache.put("blb_patch", ("diff --git a/big.kt b/big.kt\n--- a/big.kt\n+++ b/big.kt\n@@ -1 +1,3 @@\n-a\n+b\n+c\n+d\n").toByteArray())
        val blobs = BlobRepository(dev.aas.android.sync.AasHttp(okhttp3.OkHttpClient()), { null }, cache) { _, e -> throw AssertionError(e) }
        return onMain { DiffViewModel(DiffRoute("thr_1", turnId), ThreadRepository(env.engine, env.reads, env.lists), blobs, policy, drafts, UserMessages(), env.engine.status) }
    }

    @After
    fun tearDown() = env.close()

    @Test
    fun theTurnsPatchIsSplitIntoTheDaemonsFiles() = blockingTest {
        env.serve(null)
        env.answers[Methods.ThreadDiff.name] = { msg ->
            val scope = (msg.params as JsonObject)["scope"] as JsonObject
            val result = if (scope["kind"].toString().contains("turn")) {
                ThreadDiffResult(DiffSummary(3, 1, 4), files, patch = patch)
            } else {
                ThreadDiffResult(DiffSummary(1, 3, 1), listOf(DiffFile("big.kt", FileChangeKind.Update, 3, 1, false)), patchBlobId = "blb_patch")
            }
            json(ThreadDiffResult.serializer(), result)
        }
        env.connect()
        val vm = viewModel()
        val diff = eventually(what = "loaded") { (vm.state.value.content as? DiffContent.Loaded)?.diff }
        assertEquals(listOf(FileNote.None, FileNote.Binary, FileNote.NotInPatch), diff.files.map { it.note })
        val lines = diff.files[0].patch!!.hunks.single().lines
        assertEquals(listOf(DiffLine.Kind.Context, DiffLine.Kind.Removed, DiffLine.Kind.Added), lines.map { it.kind })
        assertEquals(false, diff.oneFileAtATime)
        // Rows of the list: header, hunk header + 3 lines, end; then header, note, end.
        assertEquals(6, diff.firstRowOf(1))

        // The thread scope reads its large patch from the blob.
        onMain { vm.setScope(DiffScopeChoice.Thread) }
        val thread = eventually(what = "thread scope") { (vm.state.value.content as? DiffContent.Loaded)?.diff?.takeIf { it.files.single().path == "big.kt" } }
        assertEquals(3, thread.files.single().patch!!.added)

        // A line comment goes to the thread's composer.
        onMain { vm.comment(diff.files[0].patch!!, lines[2], "why z?") }
        assertEquals("> `a.kt:2`\n> `z`\nwhy z?", drafts.get(ComposerDrafts.threadKey("thr_1")).text)
    }

    @Test
    fun largeDiffsShowOneFileAtATime() = blockingTest {
        env.serve(null)
        env.answers[Methods.ThreadDiff.name] = { json(ThreadDiffResult.serializer(), ThreadDiffResult(DiffSummary(3, 1, 4), files, patch = patch)) }
        env.connect()
        val vm = viewModel(DiffViewPolicy(oneFileAtATimeLines = 2, maxFileLines = 100), turnId = null)
        assertEquals(DiffScopeChoice.Thread, vm.state.value.scope)
        val diff = eventually(what = "loaded") { (vm.state.value.content as? DiffContent.Loaded)?.diff }
        assertTrue(diff.oneFileAtATime)
        assertEquals(UiText.of(R.string.diff_one_file_at_a_time), diff.notice)
        assertEquals(listOf("a.kt"), diff.shownFiles.map { it.path })
        onMain { vm.select(5) }
        val selected = (vm.state.value.content as DiffContent.Loaded).diff
        assertEquals(2, selected.selected)
        assertEquals(listOf("gone.kt"), selected.shownFiles.map { it.path })
    }

    @Test
    fun filesTooLargeForAPhoneAndMissingPatchesAreNamed() {
        val result = ThreadDiffResult(DiffSummary(3, 1, 4), files, patch = patch)
        // a.kt has 4 rendered lines (hunk header included): above maxFileLines = 3.
        val small = DiffViewModel.build(result, dev.aas.android.domain.diff.UnifiedDiff.parse(patch), null, DiffViewPolicy(maxFileLines = 3))
        assertEquals(listOf(FileNote.FileTooLarge, FileNote.Binary, FileNote.NotInPatch), small.files.map { it.note })
        // No patch (above the download limit): every text file says so.
        val none = DiffViewModel.build(result, null, UiText.of(R.string.diff_patch_too_large), DiffViewPolicy())
        assertEquals(listOf(FileNote.PatchTooLarge, FileNote.Binary, FileNote.PatchTooLarge), none.files.map { it.note })
        assertEquals(UiText.of(R.string.diff_patch_too_large), none.notice)
    }

    @Test
    fun offlineTheDiffWaitsForTheConnection() = blockingTest {
        env.startOffline()
        val vm = viewModel()
        val content = eventually(what = "offline") { vm.state.value.content as? DiffContent.Offline }
        assertEquals(DiffContent.Offline, content)
    }
}
