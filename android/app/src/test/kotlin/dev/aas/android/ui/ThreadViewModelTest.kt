package dev.aas.android.ui

import androidx.compose.ui.text.TextRange
import androidx.compose.ui.text.input.TextFieldValue
import androidx.lifecycle.SavedStateHandle
import dev.aas.android.AppPolicy
import dev.aas.android.data.ComposerDrafts
import dev.aas.android.data.HarnessRepository
import dev.aas.android.data.InteractionRepository
import dev.aas.android.data.SentDrafts
import dev.aas.android.data.ThreadRepository
import dev.aas.android.domain.composer.FollowUpDelivery
import dev.aas.android.domain.composer.PaletteAction
import dev.aas.android.domain.composer.SendAction
import dev.aas.android.domain.composer.SendBlock
import dev.aas.android.domain.timeline.TimelineRow
import dev.aas.android.notify.AppVisibility
import dev.aas.android.protocol.AasJson
import dev.aas.android.protocol.BackgroundTaskStatus
import dev.aas.android.protocol.BackgroundTaskStopParams
import dev.aas.android.protocol.CommandListResult
import dev.aas.android.protocol.Delivery
import dev.aas.android.protocol.ErrorKind
import dev.aas.android.protocol.FsSearchResult
import dev.aas.android.protocol.InputPart
import dev.aas.android.protocol.Methods
import dev.aas.android.protocol.PickerKind
import dev.aas.android.protocol.QueueRemoveParams
import dev.aas.android.protocol.QueueSteerParams
import dev.aas.android.protocol.QueueUpdateParams
import dev.aas.android.protocol.RpcMessage
import dev.aas.android.protocol.ThreadReadResult
import dev.aas.android.protocol.ThreadSettings
import dev.aas.android.protocol.ThreadUpdateParams
import dev.aas.android.protocol.TurnStartParams
import dev.aas.android.protocol.TurnStatus
import dev.aas.android.settings.AppSettings
import dev.aas.android.sync.FakeServer
import dev.aas.android.sync.OutboxEntry
import dev.aas.android.sync.eventually
import dev.aas.android.testing.FakeUploader
import dev.aas.android.testing.Fixtures
import dev.aas.android.testing.MainDispatcherRule
import dev.aas.android.testing.TestEngine
import dev.aas.android.testing.blockingTest
import dev.aas.android.ui.common.UserMessages
import dev.aas.android.ui.composer.Attachment
import dev.aas.android.ui.composer.CommandsState
import dev.aas.android.ui.composer.MentionSearch
import dev.aas.android.ui.composer.PromptTemplates
import dev.aas.android.ui.navigation.ThreadRoute
import dev.aas.android.ui.thread.BackgroundStop
import dev.aas.android.ui.thread.ThreadEvent
import dev.aas.android.ui.thread.ThreadViewModel
import androidx.lifecycle.viewModelScope
import kotlinx.coroutines.CompletableDeferred
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.Job
import kotlinx.coroutines.cancel
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.first
import kotlinx.coroutines.launch
import kotlinx.coroutines.withContext
import kotlinx.serialization.KSerializer
import org.junit.After
import org.junit.Rule
import org.junit.Test
import kotlin.test.assertEquals
import kotlin.test.assertIs
import kotlin.test.assertTrue

/**
 * The thread view model against a real sync engine: what the composer, the queue, the palette
 * and the settings put into the outbox (offline) or ask the daemon (online, scripted server).
 */
class ThreadViewModelTest {
    @get:Rule
    val main = MainDispatcherRule()

    private val env = TestEngine()
    private val settings = MutableStateFlow(AppSettings())
    private val messages = UserMessages()
    private val drafts = ComposerDrafts()
    private val sentDrafts = SentDrafts(env.scope, drafts)
    private val uploader = FakeUploader()
    private val jobs = mutableListOf<Job>()
    private val viewModels = mutableListOf<ThreadViewModel>()

    /** The fixture thread with the claude harness of the fixture (steer capable) and [running] state. */
    private fun read(running: Boolean = true, paused: Boolean = false): ThreadReadResult {
        val r = Fixtures.threadRead
        val lastTurn = r.thread.lastTurn!!.copy(status = if (running) TurnStatus.Running else TurnStatus.Completed)
        return r.copy(thread = r.thread.copy(lastTurn = lastTurn, queuePaused = paused))
    }

    private val harness = TestEngine.fakeHarness().copy(id = "claude")

    private companion object {
        /** Upper bound for a view model's message to be posted. */
        const val MESSAGE_TIMEOUT_MS = 10_000L
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
                settings = settings,
                visibility = AppVisibility {},
                messages = messages,
                drafts = drafts,
                sentDrafts = sentDrafts,
                uploader = uploader,
                policy = AppPolicy(mentionSearchDebounceMs = 0),
                templates = PromptTemplates("REVIEW", "INIT"),
                saved = SavedStateHandle(),
                harnesses = HarnessRepository(env.engine),
            )
        }
        viewModels += vm
        jobs += CoroutineScope(Dispatchers.Default).launch { vm.state.collect {} }
        eventually(what = "the thread") { vm.state.value.thread.thread }
        return vm
    }

    private suspend fun offline(read: ThreadReadResult): ThreadViewModel {
        env.seed(read, harnesses = listOf(harness))
        env.startOffline()
        eventually(what = "the workspace") { env.engine.workspace.value.harnesses.takeIf { it.isNotEmpty() } }
        return viewModel(read)
    }

    private suspend fun main(block: () -> Unit) = withContext(Dispatchers.Main) { block() }

    private suspend fun <T> awaitOutbox(method: String, serializer: KSerializer<T>, count: Int = 1): List<T> =
        eventually(what = "$count × $method in the outbox") {
            env.engine.outbox.value.filter { it.method == method }.takeIf { it.size >= count }
        }.map { entry: OutboxEntry -> AasJson.decodeFromJsonElement(serializer, entry.params) }

    @After
    fun tearDown() {
        jobs.forEach { it.cancel() }
        dev.aas.android.testing.clearViewModels(viewModels)
        env.close()
    }

    /**
     * The バックグラウンド section of a harness that reports background work: the running task open
     * with 停止, the ended ones folded; 停止 puts `backgroundTask/stop { threadId, taskId }` in the
     * outbox (the card then says it waits to be sent); a backgrounded item's chip opens the section
     * where its task is and scrolls to it; the stop button's hint says background work goes on.
     */
    @Test
    fun backgroundTasksAreListedStoppedAndOpenedFromTheirItem() = blockingTest {
        val read = read(running = false)
        env.seed(read, harnesses = listOf(harness.copy(capabilities = TestEngine.fakeHarness(background = true).capabilities)))
        env.startOffline()
        eventually(what = "the workspace") { env.engine.workspace.value.harnesses.takeIf { it.isNotEmpty() } }
        val vm = viewModel(read)
        val events = mutableListOf<ThreadEvent>()
        jobs += CoroutineScope(Dispatchers.Default).launch { vm.eventFlow.collect { events += it } }

        val agent = read.backgroundTasks.single { it.status == BackgroundTaskStatus.Running }
        val ui = eventually(what = "the section") { vm.state.value.takeIf { it.rows.any { r -> r is TimelineRow.BackgroundHeader } } }
        assertEquals(TimelineRow.BackgroundHeader(running = 1, ambient = 0, ended = 2, expanded = true), ui.rows.filterIsInstance<TimelineRow.BackgroundHeader>().single())
        assertEquals(listOf(agent.id), ui.rows.filterIsInstance<TimelineRow.BackgroundTaskRow>().map { it.task.id }, "ended tasks are folded")
        assertEquals(BackgroundStop.Available, ui.stopOf(agent))
        assertEquals(listOf(agent), ui.runningTasks)
        assertTrue(ui.keepsBackgroundOnInterrupt)
        // The approval the background agent asked names it.
        val asked = ui.pendingInteractions.single { it.backgroundTaskId != null }
        assertEquals("Review the reconnect logic", ui.backgroundTasks[asked.backgroundTaskId]?.title)

        main { vm.stopBackgroundTask(agent) }
        val stop = awaitOutbox(Methods.BackgroundTaskStop.name, BackgroundTaskStopParams.serializer()).single()
        assertEquals(read.thread.id, stop.threadId)
        assertEquals(agent.id, stop.taskId)
        eventually(what = "queued stop") { vm.state.value.takeIf { it.stopOf(agent) == BackgroundStop.Queued } }
        kotlinx.coroutines.withTimeout(MESSAGE_TIMEOUT_MS) {
            messages.messages.first { it.text == dev.aas.android.ui.common.UiText.of(dev.aas.android.R.string.bg_stop_offline, agent.title) }
        }
        assertEquals(0, vm.state.value.otherPending, "a stop shows on its card, not as another pending change")

        // An ended task opened from its item: the section and its ended tasks open, the list scrolls to it.
        val ended = read.backgroundTasks.single { it.status == BackgroundTaskStatus.Completed }
        main { vm.toggleGroup(dev.aas.android.domain.timeline.Timeline.BACKGROUND_SECTION, expandedNow = true) }
        eventually(what = "folded") { vm.state.value.rows.takeIf { rows -> rows.none { it is TimelineRow.BackgroundTaskRow } } }
        main { vm.openBackgroundTask(ended.id) }
        eventually(what = "the ended task shown") {
            vm.state.value.rows.filterIsInstance<TimelineRow.BackgroundTaskRow>().firstOrNull { it.task.id == ended.id }
        }
        val scroll = eventually(what = "scroll event") { events.filterIsInstance<ThreadEvent.ScrollToRow>().firstOrNull() }
        assertEquals(TimelineRow.backgroundTaskKey(ended.id), scroll.key)
        // A task that is not known: nothing happens.
        main { vm.openBackgroundTask("bgt_unknown") }
        assertEquals(1, events.filterIsInstance<ThreadEvent.ScrollToRow>().size)
    }

    /** Without the harness's capability there is no 停止 (the daemon would refuse `capabilityUnsupported`). */
    @Test
    fun aHarnessThatCannotStopOneTaskOffersNoStop() = blockingTest {
        val read = read(running = false)
        val vm = offline(read)
        val agent = read.backgroundTasks.single { it.status == BackgroundTaskStatus.Running }
        val ui = eventually(what = "the section") { vm.state.value.takeIf { it.rows.any { r -> r is TimelineRow.BackgroundTaskRow } } }
        assertEquals(BackgroundStop.None, ui.stopOf(agent))
        assertTrue(!ui.keepsBackgroundOnInterrupt)
        // A task that did not confirm its last stop can be asked again; one being stopped cannot.
        val capable = ui.copy(harness = harness.copy(capabilities = TestEngine.fakeHarness(background = true).capabilities))
        assertEquals(BackgroundStop.Requested, capable.stopOf(agent.copy(stopRequestedAt = 5)))
        assertEquals(BackgroundStop.Unconfirmed, capable.stopOf(agent.copy(stopUnconfirmedAt = 9)))
        assertEquals(BackgroundStop.None, capable.stopOf(agent.copy(stoppable = false)))
        assertEquals(BackgroundStop.None, capable.stopOf(agent.copy(status = BackgroundTaskStatus.Stopped)))
    }

    @Test
    fun aWaitingMessageCanBeTakenBack() = blockingTest {
        val vm = offline(read(running = false))
        main { vm.composer.setText("never mind") }
        eventually(what = "enabled") { vm.state.value.send.takeIf { it.enabled } }
        main { vm.send(SendAction.Start) }
        val pending = eventually(what = "pending row") { vm.state.value.rows.filterIsInstance<TimelineRow.Pending>().singleOrNull() }
        main { vm.discardPending(pending.input.clientRequestId) }
        eventually(what = "outbox empty") { env.engine.outbox.value.takeIf { it.isEmpty() } }
        eventually(what = "no pending row") { vm.state.value.rows.takeIf { rows -> rows.none { it is TimelineRow.Pending } } }
        assertEquals(dev.aas.android.ui.common.UiText.of(dev.aas.android.R.string.outbox_discarded), messages.messages.first().text)
    }

    /**
     * The daemon refuses a message definitively after it left the composer (here `invalidState`,
     * as for the first message of a fork whose source thread moved on): it leaves the outbox, and
     * its text, mentions and images come back to the composer, before what was typed since.
     */
    @Test
    fun aMessageTheDaemonRefusesComesBackToTheComposer() = blockingTest {
        val read = read(running = false)
        env.serve(read, harnesses = listOf(harness))
        env.answers[Methods.FsSearch.name] = { AasJson.encodeToJsonElement(FsSearchResult.serializer(), Fixtures.result("fs_search", FsSearchResult.serializer())) }
        val held = CompletableDeferred<RpcMessage>()
        env.server.intercept = { _, msg ->
            // Answered below, once the user typed something new.
            (msg.method == Methods.TurnStart.name).also { taken -> if (taken) held.complete(msg) }
        }
        env.connect()
        val vm = viewModel(read)
        main { vm.composer.onValueChange(TextFieldValue("fix @rec", TextRange(8))) }
        val path = eventually(what = "mention results") { (vm.composer.state.value.mentionSearch as? MentionSearch.Results)?.results?.singleOrNull()?.path }
        main {
            vm.composer.chooseMention(path)
            vm.pickImages(listOf("content://media/1"))
        }
        eventually(what = "upload") { vm.composer.state.value.uploaded.takeIf { it.size == 1 } }
        eventually(what = "enabled") { vm.state.value.send.takeIf { it.enabled } }
        main { vm.send(SendAction.Start) }
        val request = held.await()
        eventually(what = "cleared") { vm.composer.textValue.text.takeIf { it.isEmpty() } }
        main { vm.composer.setText("and one more thing") }

        env.server.lastConnection.send(RpcMessage(id = request.id, error = FakeServer.rpcError(ErrorKind.InvalidState, "the source thread moved on: fork again")))
        val restored = eventually(what = "the draft back") { vm.composer.state.value.takeIf { it.value.text.startsWith("fix ") } }
        assertEquals("fix @$path\n\nand one more thing", restored.value.text)
        assertEquals(setOf(path), restored.mentions)
        assertEquals(listOf("content://media/1"), restored.attachments.map { it.localUri })
        assertTrue(restored.attachments.all { it.state is Attachment.State.Uploaded }, "the image is uploaded already")
        eventually(what = "outbox empty") { env.engine.outbox.value.takeIf { it.isEmpty() } }
        val rows = eventually(what = "no pending row") { vm.state.value.rows.takeIf { rows -> rows.none { it is TimelineRow.Pending } } }
        assertTrue(rows.isNotEmpty(), "the conversation is still there")
    }

    /**
     * The refusal arrives after the thread screen was left (its view model cleared): the message
     * is back in the composer when the thread opens again. It used to be gone for good.
     */
    @Test
    fun aMessageRefusedAfterTheScreenWasLeftIsBackWhenItOpensAgain() = blockingTest {
        val read = read(running = false)
        env.serve(read, harnesses = listOf(harness))
        env.answers[Methods.TurnStart.name] = { FakeServer.rpcError(ErrorKind.InvalidState, "the thread is archived") }
        env.connect()
        val vm = viewModel(read)
        main { vm.composer.setText("a long prompt worth keeping") }
        eventually(what = "enabled") { vm.state.value.send.takeIf { it.enabled } }
        main {
            vm.send(SendAction.Start)
            // Left right after sending: the screen's view model is cleared.
            vm.viewModelScope.cancel()
        }
        eventually(what = "turn/start at the server") { env.requests(Methods.TurnStart.name).singleOrNull() }
        eventually(what = "refused: out of the outbox") { env.engine.outbox.value.takeIf { it.isEmpty() } }

        val again = viewModel(read)
        eventually(what = "the message back") { again.composer.textValue.text.takeIf { it == "a long prompt worth keeping" } }
        assertTrue(drafts.takeReturned(ComposerDrafts.threadKey(read.thread.id)).isEmpty(), "given back once")
    }

    @Test
    fun imagesOfADraftForAHarnessWithoutImagesBlockSending() = blockingTest {
        // A draft with an image (kept from before the harness lost the capability).
        val read = read(running = false)
        drafts.set(
            ComposerDrafts.threadKey(read.thread.id),
            dev.aas.android.data.Draft("look", images = listOf(dev.aas.android.data.DraftImage("content://media/9", dev.aas.android.data.UploadedImage("blb_9", "image/png", 1)))),
        )
        env.seed(read, harnesses = listOf(harness.copy(capabilities = harness.capabilities.copy(images = false))))
        env.startOffline()
        eventually(what = "the workspace") { env.engine.workspace.value.harnesses.takeIf { it.isNotEmpty() } }
        val vm = viewModel(read)
        eventually(what = "blocked") { vm.state.value.send.takeIf { it.blocked == SendBlock.ImagesUnsupported } }
        main { vm.composer.removeAttachment(vm.composer.state.value.attachments.single().id) }
        assertTrue(eventually(what = "enabled") { vm.state.value.send.takeIf { it.enabled } }.enabled)
    }

    @Test
    fun followUpsQueueOrSteerAndAnEmptyComposerStops() = blockingTest {
        val vm = offline(read(running = true))
        assertEquals(SendAction.Interrupt, vm.state.value.send.primary)

        main { vm.composer.setText("  queued please ") }
        eventually(what = "queue") { vm.state.value.send.takeIf { it.primary == SendAction.Queue && it.alternate == SendAction.Steer } }
        main { vm.send(SendAction.Queue) }
        val queued = awaitOutbox(Methods.TurnStart.name, TurnStartParams.serializer()).single()
        assertEquals(listOf(InputPart.Text("queued please")), queued.input)
        assertEquals(Delivery.Queue, queued.delivery)
        // The composer is empty again, and the message shows as waiting.
        eventually(what = "cleared") { vm.composer.textValue.text.takeIf { it.isEmpty() } }
        eventually(what = "pending row") { vm.state.value.rows.filterIsInstance<TimelineRow.Pending>().takeIf { it.size == 1 } }

        // Steering first when preferred; the long-press alternative is the queue.
        settings.value = AppSettings(followUp = FollowUpDelivery.Steer)
        main { vm.composer.setText("now") }
        eventually(what = "steer") { vm.state.value.send.takeIf { it.primary == SendAction.Steer } }
        main { vm.send(SendAction.Steer) }
        assertEquals(Delivery.Steer, awaitOutbox(Methods.TurnStart.name, TurnStartParams.serializer(), 2)[1].delivery)

        eventually(what = "stop") { vm.state.value.send.takeIf { it.primary == SendAction.Interrupt } }
        main { vm.send(SendAction.Interrupt) }
        awaitOutbox(Methods.TurnInterrupt.name, dev.aas.android.protocol.TurnInterruptParams.serializer())
        // While the interrupt waits for its answer the stop button says so.
        val stopping = eventually(what = "interrupting") { vm.state.value.send.takeIf { it.blocked == SendBlock.Interrupting } }
        assertEquals(SendAction.Interrupt, stopping.primary)
    }

    @Test
    fun anIdleThreadStartsATurnAndAPausedQueueAsksFirst() = blockingTest {
        val vm = offline(read(running = false, paused = true))
        main { vm.composer.setText("start") }
        eventually(what = "start") { vm.state.value.send.takeIf { it.primary == SendAction.Start && it.enabled } }
        assertTrue(vm.needsPausedQueueConfirmation(SendAction.Start))
        main { vm.send(SendAction.Start, clearQueueFirst = true) }
        // The queue is cleared first (same thread lane, in order), then the message starts a turn.
        val removed = awaitOutbox(Methods.QueueRemove.name, QueueRemoveParams.serializer()).single()
        assertEquals(Fixtures.threadRead.queued.single().id, removed.queuedId)
        assertEquals(Delivery.Auto, awaitOutbox(Methods.TurnStart.name, TurnStartParams.serializer()).single().delivery)
        val order = env.engine.outbox.value.map { it.method }
        assertEquals(listOf(Methods.QueueRemove.name, Methods.TurnStart.name), order)
    }

    @Test
    fun queuedMessagesAreEditedSentNowRemovedAndResumed() = blockingTest {
        val vm = offline(read(running = true))
        val item = vm.state.value.queued.single()
        assertTrue(vm.state.value.canSendQueuedNow)
        main {
            vm.editQueued(item, "Also update the README")
            vm.steerQueued(item)
            vm.removeQueued(item)
            vm.resumeQueue()
        }
        assertEquals(listOf(InputPart.Text("Also update the README")), awaitOutbox(Methods.QueueUpdate.name, QueueUpdateParams.serializer()).single().input)
        assertEquals(item.id, awaitOutbox(Methods.QueueSteer.name, QueueSteerParams.serializer()).single().queuedId)
        assertEquals(item.id, awaitOutbox(Methods.QueueRemove.name, QueueRemoveParams.serializer()).single().queuedId)
        awaitOutbox(Methods.QueueResume.name, dev.aas.android.protocol.QueueResumeParams.serializer())
    }

    @Test
    fun settingsPinAndThePaletteCommands() = blockingTest {
        val vm = offline(read(running = true))
        val events = mutableListOf<ThreadEvent>()
        jobs += CoroutineScope(Dispatchers.Default).launch { vm.eventFlow.collect { events += it } }
        main {
            vm.applySettings(ThreadSettings(model = "large"))
            vm.togglePin()
        }
        val updates = awaitOutbox(Methods.ThreadUpdate.name, ThreadUpdateParams.serializer(), 2)
        assertEquals(ThreadSettings(model = "large"), updates.single { it.settings != null }.settings)
        // The fixture thread is pinned: the toggle unpins.
        assertEquals(false, updates.single { it.pinned != null }.pinned)

        // Palette: method actions the app knows open their screen or confirmation.
        val entries = dev.aas.android.domain.composer.Palette.entries(
            Fixtures.result("command_list", CommandListResult.serializer()).commands + listOf(
                dev.aas.android.protocol.Command("diff", null, dev.aas.android.protocol.CommandSource.App, action = dev.aas.android.protocol.CommandAction.Method("thread/diff")),
                dev.aas.android.protocol.Command("archive", null, dev.aas.android.protocol.CommandSource.App, action = dev.aas.android.protocol.CommandAction.Method("thread/archive")),
            ),
            dev.aas.android.domain.composer.PaletteContext(inThread = true),
        )
        main {
            vm.composer.setText("/")
            vm.choose(entries.first { it.name == "diff" })
            vm.choose(entries.first { it.name == "archive" })
            vm.choose(entries.first { it.name == "model" })
            vm.choose(entries.first { it.name == "status" })
            vm.choose(entries.first { it.name == "new" })
        }
        eventually(what = "events") { events.takeIf { it.size >= 5 } }
        assertEquals(ThreadEvent.OpenDiff(null), events[0])
        assertEquals(ThreadEvent.ConfirmArchive, events[1])
        assertEquals(ThreadEvent.OpenPicker(PickerKind.Model), events[2])
        assertEquals(ThreadEvent.ShowStatus, events[3])
        assertEquals(ThreadEvent.OpenNewThread(Fixtures.threadRead.thread.projectId, "claude"), events[4])
        // The "/" token is gone after a command that is not text.
        assertEquals("", vm.composer.textValue.text)
        // Insert-text commands and the local templates go into the composer.
        main { vm.choose(entries.first { it.name == "compact" }) }
        assertEquals("/compact ", vm.composer.textValue.text)
        main {
            vm.composer.setText("/rev")
            vm.choose(entries.first { it.action == PaletteAction.Local(dev.aas.android.domain.composer.LocalCommand.Review) })
        }
        assertEquals("REVIEW", vm.composer.textValue.text)
    }

    /**
     * `/resume` is the app's: from the palette or typed out and sent, it opens 「PC のセッションを
     * 取り込む」 for the thread's project with the thread's harness, and nothing reaches the harness.
     */
    @Test
    fun resumeOpensTheImportForTheThreadsProjectAndHarnessAndIsNeverSent() = blockingTest {
        val read = read(running = false)
        env.seed(read, harnesses = listOf(harness.copy(capabilities = harness.capabilities.copy(nativeSessions = true))))
        env.startOffline()
        eventually(what = "the workspace") { env.engine.workspace.value.harnesses.takeIf { it.isNotEmpty() } }
        val vm = viewModel(read)
        val events = java.util.concurrent.CopyOnWriteArrayList<ThreadEvent>()
        jobs += CoroutineScope(Dispatchers.Default).launch { vm.eventFlow.collect { events += it } }
        val expected = ThreadEvent.OpenImport(read.thread.projectId, "claude")

        main { vm.composer.setText("/res") }
        val entry = eventually(what = "/resume in the palette") { vm.composer.state.value.palette.firstOrNull { it.name == "resume" } }
        assertEquals(PaletteAction.Local(dev.aas.android.domain.composer.LocalCommand.Resume), entry.action)
        main { vm.choose(entry) }
        assertEquals(expected, eventually(what = "the import opened") { events.firstOrNull() })
        assertEquals("", vm.composer.textValue.text)

        // Typed out and sent: the same, and no message is queued for the harness.
        main {
            vm.composer.setText("/resume")
            vm.send(SendAction.Start)
        }
        eventually(what = "the import opened again") { events.takeIf { it.size == 2 } }
        assertEquals(expected, events[1])
        assertEquals("", vm.composer.textValue.text)
        assertTrue(env.engine.outbox.value.none { it.method == Methods.TurnStart.name }, "${env.engine.outbox.value}")
    }

    @Test
    fun withoutAHarnessThatCanListSessionsResumeIsHiddenAndTypedOutStillNotSent() = blockingTest {
        val vm = offline(read(running = false))
        main { vm.composer.setText("/re") }
        val palette = eventually(what = "the palette") { vm.composer.state.value.palette.takeIf { entries -> entries.any { it.name == "review" } } }
        assertTrue(palette.none { it.name == "resume" }, palette.map { it.name }.toString())

        main {
            vm.composer.setText("/resume")
            vm.send(SendAction.Start)
        }
        val message = kotlinx.coroutines.withTimeout(MESSAGE_TIMEOUT_MS) { messages.messages.first() }
        assertEquals(dev.aas.android.ui.common.UiText.of(dev.aas.android.R.string.import_no_harness), message.text)
        assertTrue(env.engine.outbox.value.none { it.method == Methods.TurnStart.name }, "${env.engine.outbox.value}")
    }

    @Test
    fun activityGroupsFoldAndUnfold() = blockingTest {
        val vm = offline(read(running = false))
        val group = vm.state.value.rows.filterIsInstance<TimelineRow.ActivityGroup>().single()
        assertEquals(false, group.expanded)
        main { vm.toggleGroup(group.groupKey, group.expanded) }
        eventually(what = "expanded") { vm.state.value.rows.filterIsInstance<TimelineRow.ActivityGroup>().single().takeIf { it.expanded } }
        assertEquals(4, vm.state.value.rows.count { it is TimelineRow.GroupedItem })
    }

    @Test
    fun onlineTheComposerLoadsCommandsSearchesMentionsAndSendsImages() = blockingTest {
        val read = read(running = false)
        env.serve(read, harnesses = listOf(harness))
        env.answers[Methods.CommandList.name] = { Fixtures.json("responses", "command_list").let { (it as kotlinx.serialization.json.JsonObject)["result"] } }
        env.answers[Methods.FsSearch.name] = { AasJson.encodeToJsonElement(FsSearchResult.serializer(), Fixtures.result("fs_search", FsSearchResult.serializer())) }
        env.answers[Methods.TurnStart.name] = { AasJson.parseToJsonElement("""{"disposition":"started","turnId":"trn_2"}""") }
        env.connect()
        val vm = viewModel(read)

        main {
            vm.composer.setText("/")
            vm.loadCommands()
        }
        val commands = eventually(what = "commands") { (vm.composer.state.value.commands as? CommandsState.Loaded)?.commands }
        assertEquals(listOf("model", "fork", "compact"), commands.map { it.name })
        assertEquals(read.thread.id, (env.requests(Methods.CommandList.name).single().params as kotlinx.serialization.json.JsonObject)["threadId"]!!.let { (it as kotlinx.serialization.json.JsonPrimitive).content })
        assertTrue(vm.composer.state.value.palette.any { it.name == "compact" })

        main { vm.composer.onValueChange(TextFieldValue("fix @rec", TextRange(8))) }
        val results = eventually(what = "mention results") { (vm.composer.state.value.mentionSearch as? MentionSearch.Results)?.results }
        assertEquals("crates/aas-server/tests/reconnect.rs", results.single().path)
        main { vm.composer.chooseMention(results.single().path) }
        assertEquals("fix @crates/aas-server/tests/reconnect.rs ", vm.composer.textValue.text)

        main { vm.pickImages(listOf("content://media/1")) }
        eventually(what = "upload") { vm.composer.state.value.uploaded.takeIf { it.size == 1 } }
        eventually(what = "enabled") { vm.state.value.send.takeIf { it.enabled && it.primary == SendAction.Start } }
        main { vm.send(SendAction.Start) }
        val sent = eventually(what = "turn/start at the server") { env.requests(Methods.TurnStart.name).firstOrNull() }
        val params = AasJson.decodeFromJsonElement(TurnStartParams.serializer(), sent.params!!)
        assertEquals(
            // The token becomes the mention part (the daemon writes it as "@path"): once, not twice.
            listOf(
                InputPart.Text("fix "),
                InputPart.Mention("crates/aas-server/tests/reconnect.rs"),
                InputPart.Image(uploader.let { "blb_${"content://media/1".hashCode().toUInt()}" }),
            ),
            params.input,
        )
        // The answered request leaves the outbox.
        eventually(what = "outbox empty") { env.engine.outbox.value.takeIf { it.isEmpty() } }
        assertIs<CommandsState.Loaded>(vm.composer.state.value.commands)
        env.engine.status.first { it.isOnline }
    }
}
