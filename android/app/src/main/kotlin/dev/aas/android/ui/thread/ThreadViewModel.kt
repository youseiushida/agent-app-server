package dev.aas.android.ui.thread

import androidx.lifecycle.SavedStateHandle
import androidx.lifecycle.ViewModel
import androidx.lifecycle.viewModelScope
import dev.aas.android.AppPolicy
import dev.aas.android.R
import dev.aas.android.data.ComposerDrafts
import dev.aas.android.data.HarnessRepository
import dev.aas.android.data.ImageUploader
import dev.aas.android.data.InteractionRepository
import dev.aas.android.data.SentDrafts
import dev.aas.android.data.ThreadRepository
import dev.aas.android.domain.HarnessWait
import dev.aas.android.domain.InboxModel
import dev.aas.android.domain.NativeSessionHarnesses
import dev.aas.android.domain.ResultMessages
import dev.aas.android.domain.ThreadActivity
import dev.aas.android.domain.composer.ComposerText
import dev.aas.android.domain.composer.FollowUpDelivery
import dev.aas.android.domain.composer.LocalCommand
import dev.aas.android.domain.composer.Palette
import dev.aas.android.domain.composer.PaletteContext
import dev.aas.android.domain.composer.PaletteEntry
import dev.aas.android.domain.composer.SendAction
import dev.aas.android.domain.composer.SendLogic
import dev.aas.android.domain.composer.SendState
import dev.aas.android.domain.timeline.PendingInput
import dev.aas.android.domain.timeline.Timeline
import dev.aas.android.domain.timeline.TimelineRow
import dev.aas.android.notify.AppVisibility
import dev.aas.android.protocol.BackgroundTask
import dev.aas.android.protocol.BackgroundTaskId
import dev.aas.android.protocol.BackgroundTaskStatus
import dev.aas.android.protocol.CommandAction
import dev.aas.android.protocol.ContextUsage
import dev.aas.android.protocol.Harness
import dev.aas.android.protocol.InputPart
import dev.aas.android.protocol.Interaction
import dev.aas.android.protocol.InteractionResolution
import dev.aas.android.protocol.InteractionStatus
import dev.aas.android.protocol.Item
import dev.aas.android.protocol.JsonKeys
import dev.aas.android.protocol.Methods
import dev.aas.android.protocol.PickerKind
import dev.aas.android.protocol.Project
import dev.aas.android.protocol.QueuedInput
import dev.aas.android.protocol.RpcException
import dev.aas.android.protocol.SettingsOutcome
import dev.aas.android.protocol.ThreadSettings
import dev.aas.android.protocol.TurnStatus
import dev.aas.android.settings.AppSettings
import dev.aas.android.sync.NotConnectedException
import dev.aas.android.sync.OutboxClearedException
import dev.aas.android.sync.OutboxEntry
import dev.aas.android.sync.SyncStatus
import dev.aas.android.sync.ThreadState
import dev.aas.android.sync.WorkspaceState
import dev.aas.android.ui.common.HarnessRefresher
import dev.aas.android.ui.common.UiText
import dev.aas.android.ui.common.UserMessages
import dev.aas.android.ui.common.requestFailed
import dev.aas.android.ui.composer.CommandsState
import dev.aas.android.ui.composer.ComposerController
import dev.aas.android.ui.composer.ComposerUiState
import dev.aas.android.ui.composer.PaletteChoice
import dev.aas.android.ui.composer.PromptTemplates
import dev.aas.android.ui.navigation.ThreadRoute
import kotlinx.coroutines.CancellationException
import kotlinx.coroutines.CoroutineStart
import kotlinx.coroutines.NonCancellable
import kotlinx.coroutines.channels.Channel
import kotlinx.coroutines.flow.Flow
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.SharingStarted
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.flow.asStateFlow
import kotlinx.coroutines.flow.combine
import kotlinx.coroutines.flow.distinctUntilChanged
import kotlinx.coroutines.flow.map
import kotlinx.coroutines.flow.receiveAsFlow
import kotlinx.coroutines.flow.stateIn
import kotlinx.coroutines.flow.update
import kotlinx.coroutines.launch
import kotlinx.coroutines.withContext
import kotlinx.serialization.json.JsonPrimitive
import kotlinx.serialization.json.contentOrNull

/** Everything the thread screen shows. */
data class ThreadUiState(
    val thread: ThreadState,
    val harness: Harness?,
    val project: Project?,
    val activity: ThreadActivity?,
    /** Interactions with an `interaction/respond` still in the outbox. */
    val answering: Set<String>,
    val loadingOlder: Boolean,
    val olderError: UiText?,
    val online: Boolean,
    /** The conversation, top to bottom. */
    val rows: List<TimelineRow>,
    val send: SendState,
    /** Context-window use (`ctx NN%`), only when the harness reported it (protocol.md §3.1). */
    val context: ContextUsage?,
    /** The plan of the running turn (the pill above the composer). */
    val plan: Item.Plan?,
    /** Requests for this thread in the outbox other than messages and interrupts. */
    val otherPending: Int,
    val followUp: FollowUpDelivery,
    /** Harnesses this app is probing right now (再確認). */
    val probing: Set<String> = emptySet(),
    /**
     * Requests of this thread other than messages that wait for their harness (a fork, a queued
     * message sent now…); messages show it in their own pending row.
     */
    val harnessWaits: List<HarnessWait> = emptyList(),
    /** Background tasks with a `backgroundTask/stop` still in the outbox. */
    val stopQueued: Set<BackgroundTaskId> = emptySet(),
) {
    val pendingInteractions: List<Interaction> get() = thread.interactions.filter { it.status == InteractionStatus.Pending }

    /** The thread's background tasks by id (a backgrounded item's chip, a background approval's title). */
    val backgroundTasks: Map<BackgroundTaskId, BackgroundTask> get() = thread.backgroundTasks.associateBy { it.id }

    /** Background tasks that run now (they stop with the agent's process: the stop and archive dialogs name them). */
    val runningTasks: List<BackgroundTask> get() = thread.backgroundTasks.filter { it.status == BackgroundTaskStatus.Running }

    /** The harness can stop single background tasks (`capabilities.backgroundStop`). */
    val canStopBackground: Boolean get() = harness?.capabilities?.backgroundStop == true

    /** The harness reports background work, so interrupting a turn leaves it running (the stop button's hint). */
    val keepsBackgroundOnInterrupt: Boolean get() = harness?.capabilities?.backgroundTasks == true

    /** How 停止 of [task] behaves now. */
    fun stopOf(task: BackgroundTask): BackgroundStop = BackgroundStop.of(task, canStopBackground, task.id in stopQueued)
    val queued: List<QueuedInput> get() = thread.queued
    val settings: ThreadSettings get() = thread.thread?.settings ?: ThreadSettings()

    /** "今すぐ反映" is possible for queued messages (see [SendLogic.canSendQueuedNow]). */
    val canSendQueuedNow: Boolean get() = SendLogic.canSendQueuedNow(thread.thread, harness?.capabilities)
}

/** One-off requests of the view model to the screen. */
sealed interface ThreadEvent {
    data class OpenThread(val threadId: String) : ThreadEvent

    data class OpenDiff(val turnId: String?) : ThreadEvent

    data class OpenNewThread(val projectId: String, val harnessId: String) : ThreadEvent

    /** `/resume`: 「PC のセッションを取り込む」 of [projectId] with [harnessId] preselected. */
    data class OpenImport(val projectId: String, val harnessId: String) : ThreadEvent

    data class OpenPicker(val kind: PickerKind) : ThreadEvent

    data object ShowStatus : ThreadEvent

    data object ShowRename : ThreadEvent

    data object ConfirmArchive : ThreadEvent

    /** The thread was archived: leave it. */
    data object Leave : ThreadEvent

    /** A message was sent: show the bottom of the conversation. */
    data object ScrollToBottom : ThreadEvent

    /** Show the row with [key] (a background task's card, once its section is open). */
    data class ScrollToRow(val key: String) : ThreadEvent
}

/**
 * The thread screen's state and actions: the open thread (opened while the screen is
 * subscribed, closed [AppPolicy.uiStopTimeoutMs] after it left), marking it read while visible,
 * the composer (send / queue / steer / stop exactly as `turn/start` and `turn/interrupt` define
 * them), the daemon's queue (edit, remove, send now, resume), settings (`thread/update`),
 * commands, interactions and older pages.
 */
class ThreadViewModel(
    val route: ThreadRoute,
    private val threads: ThreadRepository,
    private val interactions: InteractionRepository,
    private val workspace: StateFlow<WorkspaceState>,
    outbox: StateFlow<List<OutboxEntry>>,
    private val status: StateFlow<SyncStatus>,
    settings: Flow<AppSettings>,
    private val visibility: AppVisibility,
    private val messages: UserMessages,
    drafts: ComposerDrafts,
    private val sentDrafts: SentDrafts,
    uploader: ImageUploader,
    policy: AppPolicy,
    templates: PromptTemplates,
    private val saved: SavedStateHandle,
    harnesses: HarnessRepository,
) : ViewModel() {
    private val refresher = HarnessRefresher(harnesses, messages)
    val threadId: String = route.threadId
    private val loadingOlder = MutableStateFlow(false)
    private val olderError = MutableStateFlow<UiText?>(null)
    private val expanded = MutableStateFlow<Map<String, Boolean>>(emptyMap())
    private val events = Channel<ThreadEvent>(Channel.BUFFERED)
    private var commandsLoadedFor: Int? = null

    /** One-off requests (navigation, sheets). */
    val eventFlow: Flow<ThreadEvent> = events.receiveAsFlow()

    val composer = ComposerController(
        scope = viewModelScope,
        uploader = uploader,
        search = { query -> threads.search(threadId, query) },
        policy = policy,
        drafts = drafts.also { restoreSavedDraft(it) },
        draftKey = ComposerDrafts.threadKey(threadId),
        templates = templates,
        paletteContext = paletteContext(workspace.value),
    )

    private val local = combine(expanded, loadingOlder, olderError, harnesses.refreshing) { e, l, o, p -> LocalState(e, l, o, p) }

    private data class LocalState(val expanded: Map<String, Boolean>, val loadingOlder: Boolean, val olderError: UiText?, val probing: Set<String>)

    val state: StateFlow<ThreadUiState> = combine(
        threads.observe(threadId),
        workspace,
        outbox,
        status,
        combine(settings, local, composer.state) { s, l, c -> Triple(s, l, c) },
    ) { thread, ws, out, st, (appSettings, loc, comp) ->
        build(thread, ws, out, st, appSettings, loc.expanded, loc.loadingOlder, loc.olderError, comp).copy(probing = loc.probing)
    }.stateIn(viewModelScope, SharingStarted.WhileSubscribed(policy.uiStopTimeoutMs), initialState())

    private val _focusHandled = MutableStateFlow(route.interactionId == null)

    /** The route's interaction focus was applied once (scrolled to / sheet opened). */
    val focusHandled: StateFlow<Boolean> = _focusHandled.asStateFlow()

    init {
        // The draft text also lives in the saved state (it survives the process being killed).
        viewModelScope.launch { composer.state.collect { saved[SAVED_DRAFT] = it.value.text } }
        // What the palette offers follows the workspace: `/pin` or unpin, and `/resume` while
        // some harness can list its sessions.
        viewModelScope.launch {
            workspace
                .map { ws -> paletteContext(ws) }
                .distinctUntilChanged()
                .collect { composer.setPaletteContext(it) }
        }
    }

    private fun paletteContext(ws: WorkspaceState) = PaletteContext(
        inThread = true,
        pinned = ws.threads.firstOrNull { it.thread.id == threadId }?.thread?.pinned == true,
        canImport = NativeSessionHarnesses.canImport(ws.harnesses),
    )

    private fun restoreSavedDraft(drafts: ComposerDrafts) {
        val key = ComposerDrafts.threadKey(route.threadId)
        val savedText = saved.get<String>(SAVED_DRAFT)
        if (!savedText.isNullOrEmpty() && drafts.get(key).text.isEmpty()) drafts.set(key, drafts.get(key).copy(text = savedText))
    }

    private fun initialState() = ThreadUiState(
        thread = ThreadState.empty(threadId), harness = null, project = null, activity = null, answering = emptySet(),
        loadingOlder = false, olderError = null, online = status.value.isOnline, rows = emptyList(),
        send = SendLogic.state(null, null, false, false, false, FollowUpDelivery.Queue, false), context = null, plan = null,
        otherPending = 0, followUp = FollowUpDelivery.Queue,
    )

    private fun build(
        state: ThreadState,
        ws: WorkspaceState,
        outbox: List<OutboxEntry>,
        st: SyncStatus,
        appSettings: AppSettings,
        expandedGroups: Map<String, Boolean>,
        older: Boolean,
        olderFailure: UiText?,
        comp: ComposerUiState,
    ): ThreadUiState {
        val thread = state.thread
        val harness = thread?.let { t -> ws.harnesses.firstOrNull { it.id == t.harnessId } }
        val pendingInputs = PendingInput.of(state.pending)
        val interruptPending = state.pending.any { it.method == Methods.TurnInterrupt.name }
        val send = SendLogic.state(thread, harness?.capabilities, comp.hasContent, comp.uploading, comp.uploadFailed, appSettings.followUp, interruptPending, comp.hasImages)
        val runningTurn = thread?.lastTurn?.takeIf { it.status == TurnStatus.Running }
        val context = runningTurn?.let { lt -> state.turns.firstOrNull { it.id == lt.id }?.usage?.context } ?: thread?.usage?.context
        val lastTurn = state.turns.lastOrNull()
        val plan = lastTurn?.takeIf { it.status == TurnStatus.Running }?.let { t -> state.items.lastOrNull { it.turnId == t.id && it is Item.Plan } as? Item.Plan }
        // Shown where they apply: messages in the conversation, stops on their task's card, answers on their card.
        val quiet = setOf(Methods.TurnStart.name, Methods.TurnInterrupt.name, Methods.InteractionRespond.name, Methods.BackgroundTaskStop.name)
        return ThreadUiState(
            thread = state,
            harness = harness,
            project = thread?.let { t -> ws.projects.firstOrNull { it.id == t.projectId } },
            activity = thread?.let { ThreadActivity.of(it, state.interactions) },
            answering = InboxModel.respondingInteractionIds(outbox),
            loadingOlder = older,
            olderError = olderFailure,
            online = st.isOnline,
            rows = Timeline.build(state, pendingInputs, expandedGroups),
            send = send,
            context = context,
            plan = plan,
            otherPending = state.pending.count { it.method !in quiet },
            followUp = appSettings.followUp,
            harnessWaits = HarnessWait.all(state.pending.filter { it.method != Methods.TurnStart.name }, ws.harnesses),
            stopQueued = state.pending
                .filter { it.method == Methods.BackgroundTaskStop.name }
                .mapNotNull { (it.params[JsonKeys.TASK_ID] as? JsonPrimitive)?.contentOrNull }
                .toSet(),
        )
    }

    /** 再確認: `harness/refresh` of the harness a request waits for. */
    fun refreshHarness(harnessId: String) = refresher.refresh(viewModelScope, harnessId)

    fun focusApplied() {
        _focusHandled.value = true
    }

    // ----- visibility ----------------------------------------------------------------------------

    /** The screen is resumed: notifications of this thread go quiet and it is marked read. */
    fun onVisible() {
        visibility.threadShown(threadId)
        markViewed()
    }

    fun onHidden() = visibility.threadHidden(threadId)

    /** Marks everything up to the current summary as read (called again when it changes while visible). */
    fun markViewed() {
        viewModelScope.launch { runLocal { threads.markViewed(threadId) } }
    }

    fun markUnread() {
        viewModelScope.launch {
            runLocal {
                threads.markUnread(threadId)
                messages.show(UiText.of(R.string.thread_marked_unread))
            }
        }
    }

    // ----- sending -------------------------------------------------------------------------------

    /** The daemon starts a new turn while the queue is paused: the user decides about the queue. */
    fun needsPausedQueueConfirmation(action: SendAction): Boolean {
        val ui = state.value
        return SendLogic.needsPausedQueueConfirmation(ui.thread.thread, ui.queued, action)
    }

    /**
     * Sends the draft ([SendAction.Start] / [SendAction.Queue] / [SendAction.Steer]) or stops the
     * turn ([SendAction.Interrupt]). [clearQueueFirst]: remove the paused queue's messages first
     * (they are processed before the new message, in order: one lane per thread).
     *
     * The draft leaves the composer at once. It comes back (text, mentions, images) when the
     * request cannot be put in the outbox, and when the daemon refuses it definitively later
     * ([SentDrafts], also after this screen is gone).
     */
    fun send(action: SendAction, clearQueueFirst: Boolean = false) {
        if (action == SendAction.Interrupt) {
            viewModelScope.launch { runLocal { threads.interrupt(threadId) } }
            return
        }
        if (Palette.isResume(composer.textValue.text)) {
            // Typed out instead of chosen: the app's `/resume`, never sent to the harness.
            composer.setText("")
            resume()
            return
        }
        val input = composer.input()
        if (input.isEmpty()) return
        val draft = composer.sentDraft()
        val queued = state.value.queued
        composer.clear()
        // The draft left the composer: its request is committed even when the screen goes away
        // right after (started at once, and not cancellable while committing).
        viewModelScope.launch(start = CoroutineStart.UNDISPATCHED) {
            try {
                withContext(NonCancellable) {
                    if (clearQueueFirst) queued.forEach { threads.removeQueued(threadId, it.id) }
                    val request = threads.send(threadId, input, SendLogic.delivery(action))
                    sentDrafts.follow(ComposerDrafts.threadKey(threadId), request, draft.toDraft())
                }
                events.send(ThreadEvent.ScrollToBottom)
            } catch (e: CancellationException) {
                throw e
            } catch (e: Exception) {
                // Not even in the outbox: the draft comes back (with its images).
                composer.restore(draft)
                messages.show(UiText.of(R.string.error_local_store, e.message ?: e.javaClass.simpleName))
            }
        }
    }

    /**
     * Takes back a message (or another request of this thread) waiting in the outbox: it is never
     * sent. One on the wire right now stays; the snackbar says so.
     */
    fun discardPending(clientRequestId: String) {
        viewModelScope.launch { runLocal { messages.show(ResultMessages.discarded(threads.discardPending(clientRequestId))) } }
    }

    // ----- background tasks ----------------------------------------------------------------------

    /**
     * `backgroundTask/stop` (after the screen's confirmation): asks the harness to stop [task].
     * It shows 停止中… until the harness reports the end; offline the request waits in the outbox.
     */
    fun stopBackgroundTask(task: BackgroundTask) {
        viewModelScope.launch {
            runLocal {
                threads.stopBackgroundTask(threadId, task.id)
                messages.show(UiText.of(if (status.value.isOnline) R.string.bg_stop_requested else R.string.bg_stop_offline, task.title))
            }
        }
    }

    /**
     * Shows a background task in the バックグラウンド section (from a backgrounded item's chip):
     * opens the section (and the ended tasks when it ended) and scrolls to its card.
     */
    fun openBackgroundTask(taskId: BackgroundTaskId) {
        val task = state.value.thread.backgroundTasks.firstOrNull { it.id == taskId } ?: return
        expanded.update { current ->
            current + (Timeline.BACKGROUND_SECTION to true) +
                if (task.status != BackgroundTaskStatus.Running) mapOf(Timeline.BACKGROUND_ENDED to true) else emptyMap()
        }
        emit(ThreadEvent.ScrollToRow(TimelineRow.backgroundTaskKey(taskId)))
    }

    /** Loads the thread again after it failed to load. */
    fun retryLoad() = threads.retryLoad(threadId)

    /** `thread/stop`: the agent's process tree (the next message resumes it). */
    fun stopProcess() {
        viewModelScope.launch {
            runLocal {
                threads.stop(threadId)
                messages.show(UiText.of(R.string.thread_stopping))
            }
        }
    }

    fun resumeQueue() {
        viewModelScope.launch { runLocal { threads.resumeQueue(threadId) } }
    }

    fun removeQueued(queued: QueuedInput) {
        viewModelScope.launch { runLocal { threads.removeQueued(threadId, queued.id) } }
    }

    fun steerQueued(queued: QueuedInput) {
        viewModelScope.launch { runLocal { threads.steerQueued(threadId, queued.id) } }
    }

    /**
     * Replaces a queued message's text (`queue/update`). Its images stay; its mentions stay as
     * long as their `@path` is still in the text.
     */
    fun editQueued(queued: QueuedInput, text: String) {
        val mentions = queued.input.filterIsInstance<InputPart.Mention>().map { it.path }
        val images = queued.input.filterIsInstance<InputPart.Image>().map { it.blobId }
        val input = ComposerText.input(text, mentions, images)
        if (input.isEmpty()) {
            messages.show(UiText.of(R.string.queue_edit_empty))
            return
        }
        viewModelScope.launch { runLocal { threads.updateQueued(threadId, queued.id, input) } }
    }

    fun pickImages(uris: List<String>) {
        val dropped = composer.addImages(uris)
        if (dropped > 0) messages.show(UiText.of(R.string.composer_too_many_images, dropped))
    }

    // ----- palette and commands --------------------------------------------------------------

    /** Loads `command/list` (again when the harness changed its commands, or after being offline). */
    fun loadCommands() {
        val version = state.value.thread.commandsVersion
        val current = composer.state.value.commands
        if (commandsLoadedFor == version && current is CommandsState.Loaded) return
        if (!status.value.isOnline) {
            composer.setCommands(CommandsState.Offline)
            return
        }
        composer.setCommands(CommandsState.Loading)
        viewModelScope.launch {
            val result = try {
                CommandsState.Loaded(threads.commands(threadId)).also { commandsLoadedFor = version }
            } catch (e: CancellationException) {
                throw e
            } catch (e: NotConnectedException) {
                CommandsState.Offline
            } catch (e: RpcException) {
                CommandsState.Failed(UiText.of(R.string.error_server, e.error.message))
            } catch (e: Exception) {
                CommandsState.Failed(requestFailed(e))
            }
            composer.setCommands(result)
        }
    }

    fun choose(entry: PaletteEntry) {
        when (val choice = composer.choose(entry)) {
            PaletteChoice.Inserted -> Unit
            is PaletteChoice.Method -> runMethod(choice.action)
            is PaletteChoice.Picker -> emit(ThreadEvent.OpenPicker(choice.kind))
            is PaletteChoice.Local -> when (choice.command) {
                LocalCommand.New -> state.value.thread.thread?.let { emit(ThreadEvent.OpenNewThread(it.projectId, it.harnessId)) }
                LocalCommand.Status -> emit(ThreadEvent.ShowStatus)
                LocalCommand.Rename -> emit(ThreadEvent.ShowRename)
                LocalCommand.Pin -> togglePin()
                LocalCommand.Resume -> resume()
                // Inserted by the composer.
                LocalCommand.Review, LocalCommand.Init -> Unit
            }
            is PaletteChoice.Unsupported -> messages.show(UiText.of(R.string.palette_unsupported_type, choice.type))
        }
    }

    /**
     * `/resume`: 「PC のセッションを取り込む」 for this thread's project with its harness
     * preselected; choosing a session there imports it and opens its thread.
     */
    fun resume() {
        val ws = workspace.value
        val thread = ws.threads.firstOrNull { it.thread.id == threadId }?.thread ?: state.value.thread.thread ?: return
        if (!NativeSessionHarnesses.canImport(ws.harnesses)) {
            messages.show(UiText.of(R.string.import_no_harness))
            return
        }
        emit(ThreadEvent.OpenImport(thread.projectId, thread.harnessId))
    }

    /** A command's `method` action: the ones the app knows get their own screen or confirmation. */
    private fun runMethod(action: CommandAction.Method) {
        when (action.method) {
            Methods.ThreadDiff.name -> emit(ThreadEvent.OpenDiff(null))
            Methods.ThreadFork.name -> fork()
            Methods.ThreadArchive.name -> emit(ThreadEvent.ConfirmArchive)
            Methods.ThreadStop.name -> stopProcess()
            Methods.QueueResume.name -> resumeQueue()
            else -> viewModelScope.launch {
                try {
                    threads.runCommand(action, threadId)
                    messages.show(UiText.of(R.string.command_done, action.method))
                } catch (e: CancellationException) {
                    throw e
                } catch (e: NotConnectedException) {
                    messages.show(UiText.of(R.string.error_not_connected))
                } catch (e: RpcException) {
                    messages.show(UiText.of(R.string.error_server, e.error.message))
                } catch (e: Exception) {
                    messages.show(requestFailed(e))
                }
            }
        }
    }

    // ----- thread settings and management ---------------------------------------------------

    /**
     * `thread/update { settings }`: says whether it applies now or from the next turn
     * (`settingsOutcome`). Offline, the change waits in the outbox.
     */
    fun applySettings(changes: ThreadSettings) {
        if (changes == ThreadSettings()) return
        if (!status.value.isOnline) messages.show(UiText.of(R.string.settings_change_queued))
        viewModelScope.launch {
            try {
                val result = threads.updateSettings(threadId, changes)
                when (result.settingsOutcome) {
                    SettingsOutcome.AppliesNextTurn -> messages.show(UiText.of(R.string.settings_applies_next_turn))
                    SettingsOutcome.AppliedLive -> messages.show(UiText.of(R.string.settings_applied_live))
                    SettingsOutcome.Unknown, null -> messages.show(UiText.of(R.string.settings_applied))
                }
            } catch (e: CancellationException) {
                throw e
            } catch (e: RpcException) {
                // The shell already reports definitive failures of queued requests.
            } catch (e: OutboxClearedException) {
                // Unpaired meanwhile: the request was dropped with everything else.
            } catch (e: Exception) {
                messages.show(UiText.of(R.string.error_local_store, e.message ?: e.javaClass.simpleName))
            }
        }
    }

    fun rename(title: String) {
        val trimmed = title.trim()
        if (trimmed.isEmpty()) return
        viewModelScope.launch { runLocal { threads.rename(threadId, trimmed) } }
    }

    fun togglePin() {
        val pinned = state.value.thread.thread?.pinned ?: return
        viewModelScope.launch {
            runLocal {
                threads.setPinned(threadId, !pinned)
                messages.show(UiText.of(if (pinned) R.string.thread_unpinned else R.string.thread_pinned))
            }
        }
    }

    /** Archives (stopping the process first on the daemon) and leaves the thread. */
    fun archive() {
        viewModelScope.launch {
            runLocal {
                threads.archive(threadId, archived = true)
                events.send(ThreadEvent.Leave)
            }
        }
    }

    fun unarchive() {
        viewModelScope.launch { runLocal { threads.archive(threadId, archived = false) } }
    }

    /** `thread/fork`; the new thread opens when the daemon created it. */
    fun fork() {
        if (!status.value.isOnline) messages.show(UiText.of(R.string.fork_queued))
        viewModelScope.launch {
            try {
                val thread = threads.fork(threadId)
                events.send(ThreadEvent.OpenThread(thread.id))
            } catch (e: CancellationException) {
                throw e
            } catch (e: RpcException) {
                // Reported by the shell (definitive failure of a queued request).
            } catch (e: OutboxClearedException) {
                // Unpaired meanwhile: the request was dropped with everything else.
            } catch (e: Exception) {
                messages.show(UiText.of(R.string.error_local_store, e.message ?: e.javaClass.simpleName))
            }
        }
    }

    fun toggleGroup(groupKey: String, expandedNow: Boolean) {
        expanded.update { it + (groupKey to !expandedNow) }
    }

    fun respond(interaction: Interaction, resolution: InteractionResolution) {
        viewModelScope.launch { runLocal { interactions.respond(interaction, resolution) } }
    }

    fun loadOlder() {
        if (loadingOlder.value) return
        loadingOlder.value = true
        olderError.value = null
        viewModelScope.launch {
            try {
                threads.loadOlder(threadId)
            } catch (e: CancellationException) {
                throw e
            } catch (e: NotConnectedException) {
                olderError.value = UiText.of(R.string.thread_older_offline)
            } catch (e: Exception) {
                olderError.value = UiText.of(R.string.thread_older_failed, e.message ?: e.javaClass.simpleName)
            } finally {
                loadingOlder.value = false
            }
        }
    }

    override fun onCleared() {
        visibility.threadHidden(threadId)
    }

    private fun emit(event: ThreadEvent) {
        viewModelScope.launch { events.send(event) }
    }

    /** Runs a local step (outbox commit, read state); a failure of the device's store is shown. */
    private suspend fun runLocal(block: suspend () -> Unit) {
        try {
            block()
        } catch (e: CancellationException) {
            throw e
        } catch (e: Exception) {
            messages.show(UiText.of(R.string.error_local_store, e.message ?: e.javaClass.simpleName))
        }
    }

    companion object {
        private const val SAVED_DRAFT = "draft"

        /** The `thread/update` settings for a picker choice: only what changed. */
        fun settingsChange(harness: Harness, current: ThreadSettings, model: String?, effort: String?): ThreadSettings = ThreadSettings(
            model = model.takeIf { it != null && it != dev.aas.android.domain.composer.HarnessSettings.model(harness, current)?.id },
            effort = effort.takeIf { it != null && it != current.effort },
        )
    }
}
