package dev.aas.android.ui

import androidx.lifecycle.SavedStateHandle
import dev.aas.android.AppPolicy
import dev.aas.android.R
import dev.aas.android.data.ComposerDrafts
import dev.aas.android.data.HarnessRepository
import dev.aas.android.data.InteractionRepository
import dev.aas.android.data.SentDrafts
import dev.aas.android.data.ThreadRepository
import dev.aas.android.domain.ErrorTexts
import dev.aas.android.domain.composer.SendAction
import dev.aas.android.domain.composer.SendConfirmation
import dev.aas.android.notify.AppVisibility
import dev.aas.android.protocol.AasJson
import dev.aas.android.protocol.ErrorKind
import dev.aas.android.protocol.Event
import dev.aas.android.protocol.HarnessFeatures
import dev.aas.android.protocol.InputPart
import dev.aas.android.protocol.Item
import dev.aas.android.protocol.ItemMoveToBackgroundParams
import dev.aas.android.protocol.Methods
import dev.aas.android.protocol.NativeRename
import dev.aas.android.protocol.NativeRenameStatus
import dev.aas.android.protocol.PickerKind
import dev.aas.android.protocol.PlanModeFeature
import dev.aas.android.protocol.RpcError
import dev.aas.android.protocol.StatusRow
import dev.aas.android.protocol.StatusSection
import dev.aas.android.protocol.ThreadForkParams
import dev.aas.android.protocol.ThreadHarnessStatusResult
import dev.aas.android.protocol.ThreadModes
import dev.aas.android.protocol.ThreadModesUpdate
import dev.aas.android.protocol.ThreadReadResult
import dev.aas.android.protocol.ThreadResult
import dev.aas.android.protocol.ThreadSettings
import dev.aas.android.protocol.ThreadSideQuestionResult
import dev.aas.android.protocol.ThreadUpdateParams
import dev.aas.android.protocol.ThreadUpdateResult
import dev.aas.android.protocol.TurnError
import dev.aas.android.protocol.TurnStartParams
import dev.aas.android.protocol.TurnStatus
import dev.aas.android.protocol.threadStream
import dev.aas.android.settings.AppSettings
import dev.aas.android.sync.FakeServer
import dev.aas.android.sync.ThreadSync
import dev.aas.android.sync.eventually
import dev.aas.android.testing.FakeUploader
import dev.aas.android.testing.Fixtures
import dev.aas.android.testing.MainDispatcherRule
import dev.aas.android.testing.TestEngine
import dev.aas.android.testing.blockingTest
import dev.aas.android.ui.common.UiText
import dev.aas.android.ui.common.UserMessages
import dev.aas.android.ui.composer.PromptTemplates
import dev.aas.android.ui.navigation.ThreadRoute
import dev.aas.android.ui.thread.HarnessStatusState
import dev.aas.android.ui.thread.SideAnswer
import dev.aas.android.ui.thread.ThreadEvent
import dev.aas.android.ui.thread.ThreadViewModel
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.Job
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.first
import kotlinx.coroutines.launch
import kotlinx.coroutines.withContext
import kotlinx.coroutines.withTimeout
import kotlinx.serialization.KSerializer
import org.junit.After
import org.junit.Rule
import org.junit.Test
import kotlin.test.assertEquals
import kotlin.test.assertNull
import kotlin.test.assertTrue

/**
 * The thread view model with the harnesses' own features (docs/android.md 31): the app's
 * commands typed as the first word, `/plan` and a proposed plan's implementation, forks at a
 * turn, a failed resume's retry, moving work to the background, side questions, the harness's
 * status, text for the composer and renames.
 */
class ThreadFeaturesViewModelTest {
    @get:Rule
    val main = MainDispatcherRule()

    private val env = TestEngine()
    private val messages = UserMessages()
    private val drafts = ComposerDrafts()
    private val jobs = mutableListOf<Job>()
    private val viewModels = mutableListOf<ThreadViewModel>()
    private val events = java.util.concurrent.CopyOnWriteArrayList<ThreadEvent>()

    private val harness = TestEngine.fakeHarness().copy(
        id = "claude",
        features = HarnessFeatures(
            forkAtTurn = true,
            forkWhileHeld = true,
            rename = true,
            sideQuestion = true,
            moveToBackground = true,
            status = true,
            planMode = PlanModeFeature(implementPrompt = "Implement the plan.", newThreadPreamble = "Implement this plan:"),
            fastModeModels = listOf("large"),
        ),
    )

    /** The fixture thread, its last turn ended (no turn runs). */
    private fun read(modes: ThreadModes = ThreadModes(), lastTurnStatus: TurnStatus = TurnStatus.Completed): ThreadReadResult {
        val r = Fixtures.threadRead
        return r.copy(thread = r.thread.copy(lastTurn = r.thread.lastTurn!!.copy(status = lastTurnStatus), queuePaused = false, modes = modes))
    }

    private suspend fun viewModel(read: ThreadReadResult): ThreadViewModel {
        val vm = withContext(Dispatchers.Main) {
            ThreadViewModel(
                route = ThreadRoute(read.thread.id),
                threads = ThreadRepository(env.engine, env.reads, env.lists),
                interactions = InteractionRepository(env.engine),
                workspace = env.engine.workspace,
                outbox = env.engine.outbox,
                status = env.engine.status,
                settings = MutableStateFlow(AppSettings()),
                visibility = AppVisibility {},
                messages = messages,
                drafts = drafts,
                sentDrafts = SentDrafts(env.scope, drafts),
                uploader = FakeUploader(),
                policy = AppPolicy(mentionSearchDebounceMs = 0),
                templates = PromptTemplates("REVIEW", "INIT"),
                saved = SavedStateHandle(),
                harnesses = HarnessRepository(env.engine),
            )
        }
        viewModels += vm
        jobs += CoroutineScope(Dispatchers.Default).launch { vm.state.collect {} }
        jobs += CoroutineScope(Dispatchers.Default).launch { vm.eventFlow.collect { events += it } }
        eventually(what = "the thread") { vm.state.value.thread.thread }
        eventually(what = "the harness") { vm.state.value.harness }
        return vm
    }

    private suspend fun offline(read: ThreadReadResult = read()): ThreadViewModel {
        env.seed(read, harnesses = listOf(harness))
        env.startOffline()
        eventually(what = "the workspace") { env.engine.workspace.value.harnesses.takeIf { it.isNotEmpty() } }
        return viewModel(read)
    }

    private suspend fun online(read: ThreadReadResult = read()): ThreadViewModel {
        env.serve(read, harnesses = listOf(harness))
        env.connect()
        val vm = viewModel(read)
        eventually(what = "the thread live") { vm.state.value.thread.takeIf { it.sync == ThreadSync.Live } }
        return vm
    }

    private suspend fun main(block: () -> Unit) = withContext(Dispatchers.Main) { block() }

    private suspend fun type(vm: ThreadViewModel, text: String) = main {
        vm.composer.setText(text)
        vm.send(SendAction.Start)
    }

    private suspend fun <T> awaitOutbox(method: String, serializer: KSerializer<T>, count: Int = 1): List<T> =
        eventually(what = "$count × $method in the outbox") {
            env.engine.outbox.value.filter { it.method == method }.takeIf { it.size >= count }
        }.map { AasJson.decodeFromJsonElement(serializer, it.params) }

    private suspend fun awaitMessage(text: UiText) = withTimeout(MESSAGE_TIMEOUT_MS) { messages.messages.first { it.text == text } }

    private fun noTurnStart() = assertTrue(env.engine.outbox.value.none { it.method == Methods.TurnStart.name }, "${env.engine.outbox.value}")

    @After
    fun tearDown() {
        jobs.forEach { it.cancel() }
        dev.aas.android.testing.clearViewModels(viewModels)
        env.close()
    }

    /**
     * The app's commands typed as the first word run with their argument and are never sent:
     * `/rename <title>`, `/clear <message>` (the app's `/new`, the message for the new thread),
     * `/stop` (confirmed first), `/model <name>` (from the harness's list), `/effort` with a value
     * the list lacks (the picker, and why), `/btw` without a question. A harness command (Codex's
     * `/goal`) is sent as typed.
     */
    @Test
    fun typedAppCommandsRunWithTheirArgumentsAndHarnessCommandsAreSent() = blockingTest {
        val vm = offline()
        val thread = vm.state.value.thread.thread!!

        type(vm, "/rename   Login work ")
        assertEquals("Login work", awaitOutbox(Methods.ThreadUpdate.name, ThreadUpdateParams.serializer()).single().title)

        type(vm, "/clear start over here")
        assertEquals(ThreadEvent.OpenNewThread(thread.projectId, "claude"), eventually(what = "the new thread") { events.lastOrNull() as? ThreadEvent.OpenNewThread })
        assertEquals("start over here", drafts.get(ComposerDrafts.newThreadKey(thread.projectId)).text)

        type(vm, "/stop")
        eventually(what = "the stop's confirmation") { events.lastOrNull() as? ThreadEvent.ConfirmStop }
        assertTrue(env.engine.outbox.value.none { it.method == Methods.ThreadStop.name }, "stopping waits for the confirmation")

        type(vm, "/model Large")
        eventually(what = "the model change") {
            env.engine.outbox.value.map { AasJson.decodeFromJsonElement(ThreadUpdateParams.serializer(), it.params) }
                .firstOrNull { it.settings == ThreadSettings(model = "large") }
        }

        type(vm, "/effort extreme")
        awaitMessage(UiText.of(R.string.typed_effort_unknown, "extreme"))
        assertEquals(ThreadEvent.OpenPicker(PickerKind.Effort), eventually(what = "the picker") { events.lastOrNull() as? ThreadEvent.OpenPicker })

        main {
            vm.composer.setText("/btw")
            vm.send(SendAction.Start)
        }
        awaitMessage(UiText.of(R.string.btw_needs_question))
        assertEquals("/btw", vm.composer.textValue.text, "the command stays for the question")
        noTurnStart()

        type(vm, "/goal ship the release")
        val sent = awaitOutbox(Methods.TurnStart.name, TurnStartParams.serializer()).single()
        assertEquals(listOf(InputPart.Text("/goal ship the release")), sent.input)
    }

    /** `/plan <request>`: plan mode on, then the request, in this order in the thread's lane; `/plan` alone only switches. */
    @Test
    fun planSwitchesPlanModeOnThenSendsTheRequest() = blockingTest {
        val vm = offline()
        type(vm, "/plan Add a login page")
        eventually(what = "both requests") { env.engine.outbox.value.takeIf { it.size >= 2 } }
        val outbox = env.engine.outbox.value
        assertEquals(listOf(Methods.ThreadUpdate.name, Methods.TurnStart.name), outbox.map { it.method })
        assertEquals(ThreadModesUpdate(plan = true), AasJson.decodeFromJsonElement(ThreadUpdateParams.serializer(), outbox[0].params).modes)
        assertEquals(listOf(InputPart.Text("Add a login page")), AasJson.decodeFromJsonElement(TurnStartParams.serializer(), outbox[1].params).input)
        assertEquals("", vm.composer.textValue.text)

        type(vm, "/plan")
        awaitMessage(UiText.of(R.string.plan_mode_on))
        assertEquals(2, env.engine.outbox.value.count { it.method == Methods.ThreadUpdate.name })
    }

    /** 実装する: plan mode off, then the harness's own text; the proposed plan of the latest turn offers it. */
    @Test
    fun implementingAPlanTurnsPlanModeOffAndSendsTheHarnessesText() = blockingTest {
        val vm = offline(read(modes = ThreadModes(plan = true)))
        val plan = vm.state.value.thread.items.filterIsInstance<Item.ProposedPlan>().single()
        val choices = vm.state.value.planChoices(plan)
        assertTrue(choices.implement && choices.newThread, "$choices")
        main { vm.implementPlan(plan) }
        eventually(what = "both requests") { env.engine.outbox.value.takeIf { it.size >= 2 } }
        val outbox = env.engine.outbox.value
        assertEquals(ThreadModesUpdate(plan = false), AasJson.decodeFromJsonElement(ThreadUpdateParams.serializer(), outbox[0].params).modes)
        assertEquals(listOf(InputPart.Text("Implement the plan.")), AasJson.decodeFromJsonElement(TurnStartParams.serializer(), outbox[1].params).input)
    }

    /** 「新しいスレッドで実装」: `thread/create` with the preamble, a blank line and the plan; the new thread opens. */
    @Test
    fun implementingAPlanInANewThreadStartsItWithThePreambleAndThePlan() = blockingTest {
        val read = read()
        env.answers[Methods.ThreadCreate.name] = { msg ->
            val params = AasJson.decodeFromJsonElement(Methods.ThreadCreate.params, msg.params!!)
            AasJson.encodeToJsonElement(Methods.ThreadCreate.result, dev.aas.android.protocol.ThreadCreateResult(read.thread.copy(id = "thr_impl", title = (params.input!!.single() as InputPart.Text).text)))
        }
        val vm = online(read)
        val plan = vm.state.value.thread.items.filterIsInstance<Item.ProposedPlan>().single()
        main { vm.implementPlanInNewThread(plan) }
        assertEquals(ThreadEvent.OpenThread("thr_impl"), eventually(what = "the new thread") { events.lastOrNull() as? ThreadEvent.OpenThread })
        val create = AasJson.decodeFromJsonElement(Methods.ThreadCreate.params, env.requests(Methods.ThreadCreate.name).single().params!!)
        assertEquals(listOf(InputPart.Text("Implement this plan:\n\n" + plan.text)), create.input)
        assertEquals("claude", create.harnessId)
    }

    /**
     * 「このプロンプトを編集」: `thread/fork { atTurnId, before: true }`, and the turn's prompt waits
     * in the new thread's composer; 「ここから分岐」 includes the turn.
     */
    @Test
    fun forksAtATurnPutThePromptIntoTheNewThreadsComposer() = blockingTest {
        val read = read()
        var forks = 0
        env.answers[Methods.ThreadFork.name] = { AasJson.encodeToJsonElement(ThreadResult.serializer(), ThreadResult(read.thread.copy(id = "thr_fork${++forks}"))) }
        val vm = online(read)
        val turn = vm.state.value.thread.turns.first()
        val prompt = vm.state.value.thread.items.filterIsInstance<Item.UserMessage>().first { it.turnId == turn.id }
        assertTrue(vm.state.value.forkChoices(turn).editPrompt)

        main { vm.forkAt(turn, before = true) }
        assertEquals(ThreadEvent.OpenThread("thr_fork1"), eventually(what = "the fork") { events.lastOrNull() as? ThreadEvent.OpenThread })
        val params = AasJson.decodeFromJsonElement(ThreadForkParams.serializer(), env.requests(Methods.ThreadFork.name).single().params!!)
        assertEquals(turn.id, params.atTurnId)
        assertTrue(params.before)
        val draft = drafts.get(ComposerDrafts.threadKey("thr_fork1"))
        assertEquals(prompt.text, draft.text)
        assertEquals(prompt.mentions.map { it.path }.toSet(), draft.mentions)

        main { vm.forkAt(turn, before = false) }
        eventually(what = "the second fork") { events.lastOrNull()?.takeIf { it == ThreadEvent.OpenThread("thr_fork2") } }
        val second = AasJson.decodeFromJsonElement(ThreadForkParams.serializer(), env.requests(Methods.ThreadFork.name)[1].params!!)
        assertEquals(false, second.before)
        assertTrue(drafts.get(ComposerDrafts.threadKey("thr_fork2")).isEmpty)
    }

    /** After a failed resume 再試行 sends the turn's prompt again; 裏に回す queues `item/moveToBackground`. */
    @Test
    fun aFailedResumeIsRetriedAndRunningWorkMovesToTheBackground() = blockingTest {
        val base = read()
        val failedTurn = base.turns.first().copy(status = TurnStatus.Failed, error = TurnError("held by another process", ErrorTexts.RESUME_FAILED))
        val read = base.copy(turns = listOf(failedTurn) + base.turns.drop(1))
        val vm = offline(read)
        val choices = vm.state.value.resumeFailedChoices(failedTurn)
        assertEquals(dev.aas.android.domain.ResumeFailedChoices(retry = true, fork = true), choices)
        main { vm.retryTurn(failedTurn) }
        val prompt = read.items.filterIsInstance<Item.UserMessage>().first { it.turnId == failedTurn.id }
        val retried = awaitOutbox(Methods.TurnStart.name, TurnStartParams.serializer()).single()
        assertEquals(prompt.text, dev.aas.android.domain.composer.ComposerText.textOf(retried.input))

        val running = vm.state.value.thread.items.single { it.backgroundable }
        assertEquals(false, vm.state.value.moveToBackground(running), "offered, nothing queued yet")
        main { vm.moveToBackground(running) }
        val move = awaitOutbox(Methods.ItemMoveToBackground.name, ItemMoveToBackgroundParams.serializer()).single()
        assertEquals(running.id, move.itemId)
        eventually(what = "queued move") { vm.state.value.takeIf { it.moveToBackground(running) == true } }
        assertEquals(0, vm.state.value.otherPending, "the move shows on its item")
    }

    /**
     * 新しいスレッドに分岐 after a failed resume: at the last turn the agent ran (its anchor was
     * recorded), and the prompt that never reached the agent waits in the new thread's composer.
     */
    @Test
    fun aForkAfterAFailedResumeBranchesAtTheLastTurnThatRanWithThePromptInTheComposer() = blockingTest {
        val base = read()
        val ran = base.turns.first()
        val failed = base.turns[1].copy(status = TurnStatus.Failed, forkable = false, error = TurnError("held by another process", ErrorTexts.RESUME_FAILED))
        val prompt = Item.UserMessage("itm_failed", base.thread.id, failed.id, dev.aas.android.protocol.ItemStatus.Completed, failed.startedAt, text = "and now the docs")
        val read = base.copy(
            thread = base.thread.copy(lastTurn = dev.aas.android.protocol.TurnSummary(failed.id, failed.index, TurnStatus.Failed, failed.startedAt)),
            turns = listOf(ran, failed),
            items = base.items + prompt,
        )
        env.answers[Methods.ThreadFork.name] = { AasJson.encodeToJsonElement(ThreadResult.serializer(), ThreadResult(read.thread.copy(id = "thr_branch"))) }
        val vm = online(read)
        val failedRow = eventually(what = "the failed turn") { vm.state.value.thread.turns.firstOrNull { it.id == failed.id && it.error != null } }
        assertEquals(dev.aas.android.domain.ResumeFailedChoices(retry = true, fork = true), vm.state.value.resumeFailedChoices(failedRow))

        main { vm.forkAfterFailedResume(failedRow) }
        assertEquals(ThreadEvent.OpenThread("thr_branch"), eventually(what = "the fork") { events.lastOrNull() as? ThreadEvent.OpenThread })
        val params = AasJson.decodeFromJsonElement(ThreadForkParams.serializer(), env.requests(Methods.ThreadFork.name).single().params!!)
        assertEquals(ran.id, params.atTurnId)
        assertEquals(false, params.before)
        assertEquals("and now the docs", drafts.get(ComposerDrafts.threadKey("thr_branch")).text)
    }

    /** `/btw <question>`: answered in the sheet's state; a thread without a running agent says so. */
    @Test
    fun sideQuestionsAreAnsweredBesideTheConversation() = blockingTest {
        var answer: Any = AasJson.encodeToJsonElement(ThreadSideQuestionResult.serializer(), ThreadSideQuestionResult("In conn.rs.", synthetic = false))
        env.answers[Methods.ThreadSideQuestion.name] = { answer }
        val vm = online()
        type(vm, "/btw where is the timer?")
        val answered = eventually(what = "the answer") { vm.sideQuestion.value?.answer as? SideAnswer.Answered }
        assertEquals("In conn.rs.", answered.answer)
        assertEquals("where is the timer?", vm.sideQuestion.value!!.question)
        noTurnStart()

        answer = RpcError(dev.aas.android.protocol.ErrorKind.InvalidState.code, "no agent", AasJson.parseToJsonElement("""{"kind":"invalidState"}"""))
        main { vm.askSideQuestion("again?") }
        val failed = eventually(what = "the refusal") { vm.sideQuestion.value?.answer as? SideAnswer.Failed }
        assertEquals(UiText.of(R.string.btw_not_running), failed.message)
        main { vm.dismissSideQuestion() }
        assertNull(vm.sideQuestion.value)
    }

    /** The status sheet's harness section: `thread/harnessStatus` in the harness's words. */
    @Test
    fun theHarnessesStatusLoadsForTheSheet() = blockingTest {
        val result = ThreadHarnessStatusResult(listOf(StatusSection("Usage limits", listOf(StatusRow("5-hour", "13% used")))), live = true)
        env.answers[Methods.ThreadHarnessStatus.name] = { AasJson.encodeToJsonElement(ThreadHarnessStatusResult.serializer(), result) }
        val vm = online()
        main { vm.loadHarnessStatus() }
        assertEquals(result, eventually(what = "the status") { (vm.harnessStatus.value as? HarnessStatusState.Loaded)?.result })
    }

    /**
     * `composer/insert` happening now, with the screen shown and the composer empty, goes into the
     * composer; with text in the composer it waits as an offer, taken after the text or dismissed.
     */
    @Test
    fun textForTheComposerGoesInOrWaitsAsAnOffer() = blockingTest {
        val read = read()
        val vm = online(read)
        main { vm.onVisible() }
        val stream = threadStream(read.thread.id)
        env.server.append(stream, Event.ComposerInsert("summarize the diff"), seq = read.head + 1)
        env.server.lastConnection.pushNew(stream)
        eventually(what = "the text in the composer") { vm.composer.textValue.text.takeIf { it == "summarize the diff" } }
        awaitMessage(UiText.of(R.string.composer_inserted))

        main { vm.composer.setText("my draft") }
        env.server.append(stream, Event.ComposerInsert("and run the tests"), seq = read.head + 2)
        env.server.lastConnection.pushNew(stream)
        assertEquals("and run the tests", eventually(what = "the offer") { vm.state.value.insertOffer })
        assertEquals("my draft", vm.composer.textValue.text, "the draft is not overwritten")
        main { vm.acceptInsert(replace = false) }
        assertEquals("my draft\n\nand run the tests", vm.composer.textValue.text)
        eventually(what = "the offer gone") { vm.state.value.takeIf { it.insertOffer == null } }

        // The agent moving to another session is said while the screen is shown.
        env.server.append(stream, Event.NativeSessionChanged("ses-a", "ses-b"), seq = read.head + 3)
        env.server.lastConnection.pushNew(stream)
        awaitMessage(UiText.of(R.string.native_session_changed))
    }

    /** A rename says what happened to the native session's name (`nativeRename`). */
    @Test
    fun aRenameSaysWhetherTheNativeSessionTookTheName() = blockingTest {
        val read = read()
        env.answers[Methods.ThreadUpdate.name] = {
            AasJson.encodeToJsonElement(
                ThreadUpdateResult.serializer(),
                ThreadUpdateResult(read.thread.copy(title = "Login work"), nativeRename = NativeRename(NativeRenameStatus.Failed, "name too long")),
            )
        }
        val vm = online(read)
        main { vm.rename("Login work") }
        awaitMessage(UiText.of(R.string.rename_native_failed, "name too long"))
    }

    /** The fast-mode switch sends only a change of the mode, next to the settings. */
    @Test
    fun theFastModeSwitchSendsItsChange() = blockingTest {
        assertEquals(ThreadModesUpdate(fast = true), ThreadViewModel.fastChange(ThreadModes(), true))
        assertNull(ThreadViewModel.fastChange(ThreadModes(fast = true), true))
        assertNull(ThreadViewModel.fastChange(ThreadModes(), null))
        val vm = offline()
        main { vm.applySettings(ThreadSettings(model = "large"), ThreadModesUpdate(fast = true)) }
        val update = awaitOutbox(Methods.ThreadUpdate.name, ThreadUpdateParams.serializer()).single()
        assertEquals(ThreadSettings(model = "large"), update.settings)
        assertEquals(ThreadModesUpdate(fast = true), update.modes)
    }

    /**
     * `/plan <request>` while a turn runs and a message waits in the queue: plan mode applies
     * from the next turn, which is the queued message, so the screen asks first. Clearing the
     * queue makes the request the next turn: `queue/remove`, plan mode, then the request, in the
     * thread's lane in this order. Nothing is asked when no message would start before the
     * request, for `/plan` alone, or when plan mode is on already.
     */
    @Test
    fun planWithMessagesAheadAsksFirstAndCanClearTheQueue() = blockingTest {
        val read = read(lastTurnStatus = TurnStatus.Running)
        val queuedId = read.queued.single().id
        val vm = offline(read)
        main { vm.composer.setText("/plan design the cache layer") }
        assertEquals(SendConfirmation.PlanAhead(ahead = 1, canClear = true), vm.sendConfirmation(SendAction.Queue))
        main { vm.composer.setText("design the cache layer") }
        assertNull(vm.sendConfirmation(SendAction.Queue), "a plain message changes no mode")
        main { vm.composer.setText("/plan") }
        assertNull(vm.sendConfirmation(SendAction.Queue), "only the mode: nothing is sent after it")

        main {
            vm.composer.setText("/plan design the cache layer")
            vm.send(SendAction.Queue, clearQueueFirst = true)
        }
        eventually(what = "three requests") { env.engine.outbox.value.takeIf { it.size >= 3 } }
        val outbox = env.engine.outbox.value
        assertEquals(listOf(Methods.QueueRemove.name, Methods.ThreadUpdate.name, Methods.TurnStart.name), outbox.map { it.method })
        assertEquals(queuedId, AasJson.decodeFromJsonElement(Methods.QueueRemove.params, outbox[0].params).queuedId)
        assertEquals(ThreadModesUpdate(plan = true), AasJson.decodeFromJsonElement(ThreadUpdateParams.serializer(), outbox[1].params).modes)
        val request = AasJson.decodeFromJsonElement(TurnStartParams.serializer(), outbox[2].params)
        assertEquals(listOf(InputPart.Text("design the cache layer")), request.input)
        assertEquals(outbox[1].clientRequestId, outbox[2].after, "the request waits for plan mode")
    }

    /** Plan mode on already: a `/plan` request changes nothing for the messages before it. */
    @Test
    fun planInPlanModeAsksNothing() = blockingTest {
        val vm = offline(read(modes = ThreadModes(plan = true), lastTurnStatus = TurnStatus.Running))
        main { vm.composer.setText("/plan and the invalidation") }
        assertNull(vm.sendConfirmation(SendAction.Queue))
        main { vm.send(SendAction.Queue) }
        val request = awaitOutbox(Methods.TurnStart.name, TurnStartParams.serializer()).single()
        assertEquals(listOf(InputPart.Text("and the invalidation")), request.input)
        assertTrue(env.engine.outbox.value.none { it.method == Methods.ThreadUpdate.name }, "no mode change")
    }

    /**
     * Messages still in the outbox start before the request too (they reach the daemon first):
     * the screen asks, without offering to clear the queue (they are not in it yet).
     */
    @Test
    fun planAfterUnsentMessagesAsksWithoutClearing() = blockingTest {
        val read = read(lastTurnStatus = TurnStatus.Running).let { it.copy(queued = emptyList(), thread = it.thread.copy(queuedInputs = 0)) }
        val vm = offline(read)
        main { vm.composer.setText("/plan design the cache layer") }
        assertNull(vm.sendConfirmation(SendAction.Queue), "nothing waits before it")
        main {
            vm.composer.setText("run the tests and fix the failures")
            vm.send(SendAction.Queue)
        }
        awaitOutbox(Methods.TurnStart.name, TurnStartParams.serializer())
        main { vm.composer.setText("/plan design the cache layer") }
        val asked = eventually(what = "the question") { vm.sendConfirmation(SendAction.Queue) }
        assertEquals(SendConfirmation.PlanAhead(ahead = 1, canClear = false), asked)
    }

    /**
     * A refused mode change never lets the request go without it (they are one chain): the
     * request is dropped unsent and comes back to the composer as it was typed.
     */
    @Test
    fun aRefusedPlanModeKeepsTheRequestUnsentAndGivesItBack() = blockingTest {
        env.answers[Methods.ThreadUpdate.name] = { FakeServer.rpcError(ErrorKind.CapabilityUnsupported, "planMode") }
        val vm = online()
        type(vm, "/plan Add a login page")
        eventually(what = "the request back") { vm.composer.textValue.text.takeIf { it == "/plan Add a login page" } }
        assertEquals(1, env.requests(Methods.ThreadUpdate.name).size)
        assertTrue(env.requests(Methods.TurnStart.name).isEmpty(), "the request was never sent without plan mode")
        assertTrue(env.engine.outbox.value.isEmpty(), "${env.engine.outbox.value}")
    }

    /**
     * `/model <id>` whose model does not offer the thread's effort: nothing is changed; the
     * model sheet opens with that model chosen, where one of its levels is picked (as when it is
     * chosen there), and the snackbar says why.
     */
    @Test
    fun typedModelWithoutTheThreadsEffortOpensThePickerForIt() = blockingTest {
        val read = read().let { it.copy(thread = it.thread.copy(settings = ThreadSettings(model = "large", effort = "high"))) }
        val vm = offline(read)
        type(vm, "/model Small")
        awaitMessage(UiText.of(R.string.typed_model_effort_unavailable, "Small", "High"))
        assertEquals(ThreadEvent.OpenPicker(PickerKind.Model, model = "small"), eventually(what = "the picker") { events.lastOrNull() as? ThreadEvent.OpenPicker })
        assertTrue(env.engine.outbox.value.none { it.method == Methods.ThreadUpdate.name }, "no model without an effort it offers")
        assertEquals("", vm.composer.textValue.text)
    }

    private companion object {
        /** Upper bound for a view model's message to be posted. */
        const val MESSAGE_TIMEOUT_MS = 10_000L
    }
}
