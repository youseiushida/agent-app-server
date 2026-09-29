package dev.aas.android.ui.thread

import androidx.lifecycle.SavedStateHandle
import androidx.lifecycle.ViewModel
import androidx.lifecycle.viewModelScope
import dev.aas.android.AppPolicy
import dev.aas.android.R
import dev.aas.android.data.ComposerDrafts
import dev.aas.android.data.Draft
import dev.aas.android.data.HarnessRepository
import dev.aas.android.data.ImageUploader
import dev.aas.android.data.InteractionRepository
import dev.aas.android.data.SentDrafts
import dev.aas.android.data.ThreadRepository
import dev.aas.android.domain.ErrorTexts
import dev.aas.android.domain.ForkChoices
import dev.aas.android.domain.HarnessWait
import dev.aas.android.domain.InboxModel
import dev.aas.android.domain.NativeSessionHarnesses
import dev.aas.android.domain.PlanChoices
import dev.aas.android.domain.ResultMessages
import dev.aas.android.domain.ResumeFailedChoices
import dev.aas.android.domain.ThreadActions
import dev.aas.android.domain.ThreadActivity
import dev.aas.android.domain.composer.ComposerText
import dev.aas.android.domain.composer.FollowUpDelivery
import dev.aas.android.domain.composer.HarnessSettings
import dev.aas.android.domain.composer.LocalCommand
import dev.aas.android.domain.composer.Palette
import dev.aas.android.domain.composer.PaletteAction
import dev.aas.android.domain.composer.PaletteContext
import dev.aas.android.domain.composer.PaletteEntry
import dev.aas.android.domain.composer.SendAction
import dev.aas.android.domain.composer.SendConfirmation
import dev.aas.android.domain.composer.SendLogic
import dev.aas.android.domain.composer.SendState
import dev.aas.android.domain.composer.TypedCommand
import dev.aas.android.domain.timeline.PendingInput
import dev.aas.android.domain.timeline.Timeline
import dev.aas.android.domain.timeline.TimelineRow
import dev.aas.android.notify.AppVisibility
import dev.aas.android.protocol.BackgroundTask
import dev.aas.android.protocol.BackgroundTaskId
import dev.aas.android.protocol.BackgroundTaskStatus
import dev.aas.android.protocol.CommandAction
import dev.aas.android.protocol.ContextUsage
import dev.aas.android.protocol.Delivery
import dev.aas.android.protocol.ErrorKind
import dev.aas.android.protocol.Harness
import dev.aas.android.protocol.HarnessFeatures
import dev.aas.android.protocol.InputPart
import dev.aas.android.protocol.Interaction
import dev.aas.android.protocol.InteractionResolution
import dev.aas.android.protocol.InteractionStatus
import dev.aas.android.protocol.Item
import dev.aas.android.protocol.ItemId
import dev.aas.android.protocol.JsonKeys
import dev.aas.android.protocol.Methods
import dev.aas.android.protocol.PickerKind
import dev.aas.android.protocol.Project
import dev.aas.android.protocol.QueuedInput
import dev.aas.android.protocol.RpcException
import dev.aas.android.protocol.SettingsOutcome
import dev.aas.android.protocol.ThreadHarnessStatusResult
import dev.aas.android.protocol.ThreadModes
import dev.aas.android.protocol.ThreadModesUpdate
import dev.aas.android.protocol.ThreadSettings
import dev.aas.android.protocol.Turn
import dev.aas.android.protocol.TurnStartResult
import dev.aas.android.protocol.TurnStatus
import dev.aas.android.protocol.WorkspaceSpec
import dev.aas.android.settings.AppSettings
import dev.aas.android.sync.NotConnectedException
import dev.aas.android.sync.OutboxClearedException
import dev.aas.android.sync.OutboxEntry
import dev.aas.android.sync.PendingMutation
import dev.aas.android.sync.SyncSignal
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
import dev.aas.android.ui.composer.SentDraft
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
    /** Items with an `item/moveToBackground` still in the outbox (裏に回す). */
    val moveQueued: Set<ItemId> = emptySet(),
    /**
     * Text the harness asked to put into the composer (`composer/insert`) that waits for the
     * user: it arrived while the composer had text, during the catch-up, or while the screen was
     * not shown.
     */
    val insertOffer: String? = null,
) {
    /** What the harness offers beyond its capabilities (all off while unknown). */
    val features: HarnessFeatures get() = harness?.features ?: HarnessFeatures()

    /**
     * The harness asks per project whether it may load the project's own resources
     * (`features.projectTrust`) and the user has not decided for this project yet.
     */
    val trustUndecided: Boolean
        get() = features.projectTrust && project != null && harness != null && harness.id !in project.harnessTrust

    /** The user's trust decision for this thread's harness in its project (`null`: not decided). */
    val trust: Boolean? get() = harness?.let { h -> project?.harnessTrust?.get(h.id) }

    /** The fork choices of [turn] (the turn menu). */
    fun forkChoices(turn: Turn): ForkChoices =
        ThreadActions.fork(thread.thread, harness, turn, hasPrompt = ThreadActions.promptOf(turn, thread.items) != null)

    /** 再試行 / 新しいスレッドに分岐 after a failed resume, on the thread's latest turn. */
    fun resumeFailedChoices(turn: Turn): ResumeFailedChoices? =
        ThreadActions.resumeFailed(thread.thread, harness, turn, hasPrompt = ThreadActions.promptOf(turn, thread.items) != null)

    /** 実装する / 新しいスレッドで実装 of a proposed plan. */
    fun planChoices(plan: Item.ProposedPlan): PlanChoices = ThreadActions.proposedPlan(thread.thread, harness, plan)

    /** 裏に回す on [item]: `null` when it does not apply, else whether the request waits in the outbox. */
    fun moveToBackground(item: Item): Boolean? = if (ThreadActions.canMoveToBackground(harness, item)) item.id in moveQueued else null

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

/** A `/btw` question and its answer (the sheet). */
data class SideQuestionState(val question: String, val answer: SideAnswer)

sealed interface SideAnswer {
    data object Waiting : SideAnswer

    /** [answer] verbatim (`null`: the harness gave none); [synthetic]: not the model's answer. */
    data class Answered(val answer: String?, val synthetic: Boolean) : SideAnswer

    data class Failed(val message: UiText) : SideAnswer
}

/** The harness's own status in the status sheet (`thread/harnessStatus`). */
sealed interface HarnessStatusState {
    /** Not loaded (or the harness reports none: `features.status` off). */
    data object Idle : HarnessStatusState

    data object Loading : HarnessStatusState

    data class Loaded(val result: ThreadHarnessStatusResult) : HarnessStatusState

    data object Offline : HarnessStatusState

    data class Failed(val message: UiText) : HarnessStatusState
}

/** One-off requests of the view model to the screen. */
sealed interface ThreadEvent {
    data class OpenThread(val threadId: String) : ThreadEvent

    data class OpenDiff(val turnId: String?) : ThreadEvent

    data class OpenNewThread(val projectId: String, val harnessId: String) : ThreadEvent

    /** `/resume`: 「PC のセッションを取り込む」 of [projectId] with [harnessId] preselected. */
    data class OpenImport(val projectId: String, val harnessId: String) : ThreadEvent

    /** Opens the picker of [kind]; [model]: the model sheet opens with that model chosen (`/model <id>`). */
    data class OpenPicker(val kind: PickerKind, val model: String? = null) : ThreadEvent

    data object ShowStatus : ThreadEvent

    data object ShowRename : ThreadEvent

    data object ConfirmArchive : ThreadEvent

    /** The thread was archived: leave it. */
    data object Leave : ThreadEvent

    /** `/stop` (typed or chosen): confirm stopping the agent's process first. */
    data object ConfirmStop : ThreadEvent

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
    private val drafts: ComposerDrafts,
    private val sentDrafts: SentDrafts,
    uploader: ImageUploader,
    policy: AppPolicy,
    private val templates: PromptTemplates,
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
    private val harnessRepository = harnesses
    private val insertOffer = MutableStateFlow<String?>(null)
    private val _sideQuestion = MutableStateFlow<SideQuestionState?>(null)
    private val _harnessStatus = MutableStateFlow<HarnessStatusState>(HarnessStatusState.Idle)

    /** The screen is shown (resumed): text from the harness goes into the composer only then. */
    @Volatile
    private var visible = false

    /** `/btw`: the question beside the conversation and its answer (a sheet; never in the history). */
    val sideQuestion: StateFlow<SideQuestionState?> = _sideQuestion.asStateFlow()

    /** The harness's own status for the status sheet (`thread/harnessStatus`). */
    val harnessStatus: StateFlow<HarnessStatusState> = _harnessStatus.asStateFlow()

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

    private val local = combine(expanded, loadingOlder, olderError, harnesses.refreshing, insertOffer) { e, l, o, p, i -> LocalState(e, l, o, p, i) }

    private data class LocalState(
        val expanded: Map<String, Boolean>,
        val loadingOlder: Boolean,
        val olderError: UiText?,
        val probing: Set<String>,
        val insertOffer: String?,
    )

    val state: StateFlow<ThreadUiState> = combine(
        threads.observe(threadId),
        workspace,
        outbox,
        status,
        combine(settings, local, composer.state) { s, l, c -> Triple(s, l, c) },
    ) { thread, ws, out, st, (appSettings, loc, comp) ->
        build(thread, ws, out, st, appSettings, loc.expanded, loc.loadingOlder, loc.olderError, comp).copy(probing = loc.probing, insertOffer = loc.insertOffer)
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
        // What the harness relays to the open thread without storing it.
        viewModelScope.launch {
            threads.relayed(threadId).collect { signal ->
                when (signal) {
                    is SyncSignal.ComposerInsert -> onComposerInsert(signal)
                    is SyncSignal.NativeSessionChanged -> if (visible) messages.show(UiText.of(R.string.native_session_changed))
                    else -> Unit
                }
            }
        }
    }

    private fun paletteContext(ws: WorkspaceState): PaletteContext {
        val thread = ws.threads.firstOrNull { it.thread.id == threadId }?.thread
        val features = thread?.let { t -> ws.harnesses.firstOrNull { it.id == t.harnessId } }?.features
        return PaletteContext(
            inThread = true,
            pinned = thread?.pinned == true,
            canImport = NativeSessionHarnesses.canImport(ws.harnesses),
            planMode = features?.planMode != null,
            sideQuestion = features?.sideQuestion == true,
        )
    }

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
        val quiet = setOf(
            Methods.TurnStart.name,
            Methods.TurnInterrupt.name,
            Methods.InteractionRespond.name,
            Methods.BackgroundTaskStop.name,
            Methods.ItemMoveToBackground.name,
        )
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
            moveQueued = state.pending
                .filter { it.method == Methods.ItemMoveToBackground.name }
                .mapNotNull { (it.params[JsonKeys.ITEM_ID] as? JsonPrimitive)?.contentOrNull }
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
        visible = true
        visibility.threadShown(threadId)
        markViewed()
    }

    fun onHidden() {
        visible = false
        visibility.threadHidden(threadId)
    }

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
        // A typed command of the app runs instead of starting a turn (unless it sends a message).
        val typed = typedCommand()
        if (typed != null && !sendsAMessage(typed)) return false
        return SendLogic.needsPausedQueueConfirmation(ui.thread.thread, ui.queued, action)
    }

    /**
     * What the screen asks before [send] with [action] (`null`: send at once): `/plan <request>`
     * that would put messages sent before it into plan mode ([SendConfirmation.PlanAhead]), else
     * a new turn while the queue is paused ([SendConfirmation.PausedQueue]).
     */
    fun sendConfirmation(action: SendAction): SendConfirmation? {
        if (action == SendAction.Interrupt) return null
        val typed = typedCommand()
        if (typed != null && (typed.entry.action as? PaletteAction.Local)?.command == LocalCommand.Plan && sendsAMessage(typed)) {
            planAhead()?.let { return it }
        }
        return if (needsPausedQueueConfirmation(action)) SendConfirmation.PausedQueue(state.value.queued.size) else null
    }

    /**
     * Plan mode turned on now would apply to messages that start before a `/plan` request
     * ([SendLogic.messagesStartingBefore]): the daemon's queue and this thread's messages still in
     * the outbox. `null` when none would, or plan mode is on already (nothing changes for them).
     */
    private fun planAhead(): SendConfirmation.PlanAhead? {
        val ui = state.value
        val thread = ui.thread.thread ?: return null
        if (thread.modes.plan) return null
        val pending = PendingInput.of(ui.thread.pending).map { it.delivery }
        val ahead = SendLogic.messagesStartingBefore(thread, ui.queued, pending)
        return if (ahead > 0) SendConfirmation.PlanAhead(ahead, canClear = pending.isEmpty()) else null
    }

    /** The app's command the draft starts with ([TypedCommands]), or `null`. */
    private fun typedCommand(): TypedCommand? = composer.typedCommand(Palette.protocolAppCommands(state.value.harness, inThread = true))

    /** Typed commands that start a turn (the paused queue's confirmation applies to them). */
    private fun sendsAMessage(typed: TypedCommand): Boolean = when ((typed.entry.action as? PaletteAction.Local)?.command) {
        LocalCommand.Plan -> requestOf(typed.args).isNotEmpty()
        LocalCommand.Review, LocalCommand.Init -> true
        else -> false
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
        // The app's own commands typed as the first word run here, with their argument.
        typedCommand()?.let { typed ->
            runTyped(typed, action, clearQueueFirst)
            return
        }
        val input = composer.input()
        if (input.isEmpty()) return
        val draft = composer.sentDraft()
        composer.clear()
        commit(input, SendLogic.delivery(action), draft, clearQueueFirst)
    }

    /**
     * Commits a message to the outbox (after removing the queue's messages when
     * [clearQueueFirst]); [draft] comes back to the composer when it cannot be committed, and
     * when the daemon refuses it later ([SentDrafts]). The commit is not cancelled when the
     * screen goes away right after. [request] commits the message (with what must precede it).
     */
    private fun commit(
        input: List<InputPart>,
        delivery: Delivery,
        draft: SentDraft,
        clearQueueFirst: Boolean = false,
        request: suspend () -> PendingMutation<TurnStartResult> = { threads.send(threadId, input, delivery) },
    ) {
        val queued = state.value.queued
        // The draft left the composer: its request is committed even when the screen goes away
        // right after (started at once, and not cancellable while committing).
        viewModelScope.launch(start = CoroutineStart.UNDISPATCHED) {
            try {
                withContext(NonCancellable) {
                    if (clearQueueFirst) queued.forEach { threads.removeQueued(threadId, it.id) }
                    sentDrafts.follow(ComposerDrafts.threadKey(threadId), request(), draft.toDraft())
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

    // ----- the app's commands typed as the first word ------------------------------------------

    /**
     * Runs the app's command the draft starts with (docs/android.md 25章 「打ったコマンド」): the
     * text is not sent; what follows the command is its argument. `/stop` and `/archive` ask
     * first, as from the menu.
     */
    private fun runTyped(typed: TypedCommand, action: SendAction, clearQueueFirst: Boolean) {
        val args = typed.args
        when (val a = typed.entry.action) {
            is PaletteAction.Local -> when (a.command) {
                LocalCommand.New -> {
                    composer.setText("")
                    val thread = state.value.thread.thread ?: return
                    // What follows `/new` is the new thread's first message.
                    if (args.isNotEmpty()) drafts.appendParagraph(ComposerDrafts.newThreadKey(thread.projectId), args)
                    emit(ThreadEvent.OpenNewThread(thread.projectId, thread.harnessId))
                }
                LocalCommand.Status -> {
                    composer.setText("")
                    emit(ThreadEvent.ShowStatus)
                }
                LocalCommand.Rename -> {
                    composer.setText("")
                    if (args.isEmpty()) emit(ThreadEvent.ShowRename) else rename(args)
                }
                LocalCommand.Pin -> {
                    composer.setText("")
                    togglePin()
                }
                LocalCommand.Resume -> {
                    composer.setText("")
                    resume()
                }
                LocalCommand.Plan -> plan(args, clearQueueFirst)
                LocalCommand.Btw -> if (args.isEmpty()) {
                    messages.show(UiText.of(R.string.btw_needs_question))
                } else {
                    composer.setText("")
                    askSideQuestion(args)
                }
                LocalCommand.Review, LocalCommand.Init -> {
                    val template = if (a.command == LocalCommand.Review) templates.review else templates.init
                    val text = if (args.isEmpty()) template else ComposerText.appendParagraph(template, args)
                    sendDraftAs(text, SendLogic.delivery(action), clearQueueFirst)
                }
            }
            is PaletteAction.Picker -> {
                composer.setText("")
                pick(a.kind, args)
            }
            is PaletteAction.Method -> {
                composer.setText("")
                runMethod(a.action)
            }
            // The daemon's app commands do not insert text (protocol.md §4 `command/list`).
            is PaletteAction.Insert -> composer.setText(a.text)
            is PaletteAction.Unsupported -> messages.show(UiText.of(R.string.palette_unsupported_type, a.type))
        }
    }

    /** Sends [text] in place of the draft's text (its mentions and images go with it). */
    private fun sendDraftAs(text: String, delivery: Delivery, clearQueueFirst: Boolean) {
        val original = composer.sentDraft()
        val draft = composer.draftWith(text)
        val input = ComposerText.input(text, draft.mentions, draft.images.map { it.image.blobId })
        if (input.isEmpty()) return
        composer.clear()
        commit(input, delivery, original, clearQueueFirst)
    }

    /**
     * `/model <id>` and `/effort <level>` apply a value from the harness's lists at once (by id,
     * or by the name shown for it); without an argument, or with one the lists do not have, the
     * picker opens. A model that does not offer the thread's effort opens the model picker with
     * that model chosen, where one of its levels is picked (as when the model is chosen there).
     * `/permissions` always opens its picker (it confirms modes other than the default with
     * their description).
     */
    private fun pick(kind: PickerKind, args: String) {
        val harness = state.value.harness
        val thread = state.value.thread.thread
        if (args.isEmpty() || harness == null || thread == null) {
            emit(ThreadEvent.OpenPicker(kind))
            return
        }
        when (kind) {
            PickerKind.Model -> {
                val model = harness.models.firstOrNull { it.id == args || it.displayName.equals(args, ignoreCase = true) }
                val effort = thread.settings.effort
                when {
                    model == null -> {
                        messages.show(UiText.of(R.string.typed_model_unknown, args))
                        emit(ThreadEvent.OpenPicker(kind))
                    }
                    !HarnessSettings.offersEffort(harness, model.id, effort) -> {
                        // As the model sheet does: `thread/update` cannot go back to the harness
                        // default, so the user picks one of the new model's levels there.
                        val label = HarnessSettings.effort(harness, thread.settings)?.label ?: effort.orEmpty()
                        messages.show(UiText.of(R.string.typed_model_effort_unavailable, model.displayName, label))
                        emit(ThreadEvent.OpenPicker(kind, model = model.id))
                    }
                    else -> applySettings(settingsChange(harness, thread.settings, model.id, null))
                }
            }
            PickerKind.Effort -> {
                val levels = HarnessSettings.effortLevels(harness, HarnessSettings.model(harness, thread.settings)?.id)
                val level = levels.firstOrNull { it.id == args || it.label.equals(args, ignoreCase = true) }
                if (level == null) {
                    messages.show(UiText.of(R.string.typed_effort_unknown, args))
                    emit(ThreadEvent.OpenPicker(kind))
                } else {
                    applySettings(ThreadSettings(effort = level.id.takeIf { it != thread.settings.effort }))
                }
            }
            PickerKind.PermissionMode, PickerKind.Unknown -> emit(ThreadEvent.OpenPicker(kind))
        }
    }

    // ----- plan mode -------------------------------------------------------------------------

    /**
     * `/plan [request]` (harnesses with `features.planMode`): plan mode on
     * (`thread/update { modes: { plan: true } }`), then the request as a message — one chain
     * ([ThreadRepository.sendInPlanMode]), so the request is never sent without plan mode. While
     * a turn runs the request waits in the queue; plan mode applies from the next turn, which is
     * why the screen asks first when messages would start before the request
     * ([sendConfirmation]).
     */
    private fun plan(args: String, clearQueueFirst: Boolean) {
        val thread = state.value.thread.thread ?: return
        val original = composer.sentDraft()
        val input = requestOf(args)
        composer.clear()
        val needsMode = !thread.modes.plan
        if (input.isEmpty()) {
            viewModelScope.launch {
                runLocal {
                    if (needsMode) threads.setModes(threadId, ThreadModesUpdate(plan = true))
                    messages.show(UiText.of(if (needsMode) R.string.plan_mode_on else R.string.plan_mode_already))
                }
            }
            return
        }
        val delivery = if (SendLogic.turnActive(thread)) Delivery.Queue else Delivery.Auto
        commit(input, delivery, original, clearQueueFirst) {
            if (needsMode) threads.sendInPlanMode(threadId, input, delivery) else threads.send(threadId, input, delivery)
        }
    }

    /** The message of `/plan <request>`: the request with the draft's mentions and images. */
    private fun requestOf(args: String): List<InputPart> {
        val draft = composer.draftWith(args)
        return ComposerText.input(args, draft.mentions, draft.images.map { it.image.blobId })
    }

    /** Leaves plan mode (the composer's プラン chip). */
    fun exitPlanMode() {
        viewModelScope.launch {
            runLocal {
                threads.setModes(threadId, ThreadModesUpdate(plan = false))
                messages.show(UiText.of(R.string.plan_mode_off_done))
            }
        }
    }

    /**
     * 「実装する」 on a proposed plan: plan mode off, then the harness's own text for it
     * (`features.planMode.implementPrompt`), in the thread's lane in this order.
     */
    fun implementPlan(plan: Item.ProposedPlan) {
        val ui = state.value
        val thread = ui.thread.thread ?: return
        if (!ui.planChoices(plan).implement) return
        val prompt = ui.features.planMode?.implementPrompt ?: return
        val input = listOf(InputPart.Text(prompt))
        commit(input, Delivery.Auto, SentDraft(prompt, emptySet(), emptyList())) {
            if (thread.modes.plan) threads.setModes(threadId, ThreadModesUpdate(plan = false))
            threads.send(threadId, input, Delivery.Auto)
        }
        messages.show(UiText.of(R.string.proposed_plan_implementing))
    }

    /**
     * 「新しいスレッドで実装」: a new thread of the same project, harness and settings (without plan
     * mode) whose first message is the harness's preamble, a blank line and the plan. It opens
     * once the daemon created it.
     */
    fun implementPlanInNewThread(plan: Item.ProposedPlan) {
        val ui = state.value
        val thread = ui.thread.thread ?: return
        if (!ui.planChoices(plan).newThread) return
        val preamble = ui.features.planMode?.newThreadPreamble ?: return
        val input = listOf(InputPart.Text(ThreadActions.newThreadInput(preamble, plan)))
        messages.show(UiText.of(R.string.proposed_plan_new_thread_creating))
        viewModelScope.launch {
            try {
                val pending = threads.create(thread.projectId, thread.harnessId, thread.settings.takeIf { it != ThreadSettings() }, WorkspaceSpec.Local, input)
                events.send(ThreadEvent.OpenThread(pending.await().thread.id))
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

    // ----- side questions, the harness's status, trust ------------------------------------------

    /**
     * `/btw <question>`: asks the running agent beside the conversation (`thread/sideQuestion`).
     * The answer shows in a sheet and never enters the history.
     */
    fun askSideQuestion(question: String) {
        _sideQuestion.value = SideQuestionState(question, SideAnswer.Waiting)
        viewModelScope.launch {
            val answer = try {
                val result = threads.sideQuestion(threadId, question)
                SideAnswer.Answered(result.answer, result.synthetic)
            } catch (e: CancellationException) {
                throw e
            } catch (e: NotConnectedException) {
                SideAnswer.Failed(UiText.of(R.string.btw_offline))
            } catch (e: RpcException) {
                SideAnswer.Failed(if (e.kind == ErrorKind.InvalidState) UiText.of(R.string.btw_not_running) else UiText.of(R.string.btw_failed, ErrorTexts.server(e.error)))
            } catch (e: Exception) {
                SideAnswer.Failed(UiText.of(R.string.btw_failed, requestFailed(e)))
            }
            _sideQuestion.update { current -> if (current?.question == question) current.copy(answer = answer) else current }
        }
    }

    fun dismissSideQuestion() {
        _sideQuestion.value = null
    }

    /** Loads the harness's own status for the status sheet (only with `features.status`). */
    fun loadHarnessStatus() {
        if (!state.value.features.status) {
            _harnessStatus.value = HarnessStatusState.Idle
            return
        }
        _harnessStatus.value = HarnessStatusState.Loading
        viewModelScope.launch {
            _harnessStatus.value = try {
                HarnessStatusState.Loaded(threads.harnessStatus(threadId))
            } catch (e: CancellationException) {
                throw e
            } catch (e: NotConnectedException) {
                HarnessStatusState.Offline
            } catch (e: RpcException) {
                HarnessStatusState.Failed(ErrorTexts.server(e.error))
            } catch (e: Exception) {
                HarnessStatusState.Failed(requestFailed(e))
            }
        }
    }

    /**
     * The user's decision whether the thread's harness may load this project's own resources
     * (`features.projectTrust`): asked in the thread, never decided by the app.
     */
    fun setTrust(trusted: Boolean) {
        val ui = state.value
        val project = ui.project ?: return
        val harness = ui.harness ?: return
        viewModelScope.launch {
            runLocal {
                harnessRepository.setTrust(project.id, harness.id, trusted)
                messages.show(UiText.of(if (trusted) R.string.trust_saved_yes else R.string.trust_saved_no, harness.displayName))
            }
        }
    }

    // ----- text from the harness for the composer -------------------------------------------------

    /**
     * `composer/insert`: happening now while the screen is shown and the composer is empty, the
     * text goes in at once (the user sees it and sends it or not); otherwise it waits as an offer
     * above the composer, so neither a draft nor an old request is overwritten.
     */
    private fun onComposerInsert(signal: SyncSignal.ComposerInsert) {
        if (signal.live && visible && composer.textValue.text.isBlank()) {
            composer.insertFromHarness(signal.text, replace = true)
            messages.show(UiText.of(R.string.composer_inserted))
        } else {
            insertOffer.value = signal.text
        }
    }

    /** Takes the waiting offer: in place of the composer's text ([replace]) or after it. */
    fun acceptInsert(replace: Boolean) {
        val text = insertOffer.value ?: return
        insertOffer.value = null
        composer.insertFromHarness(text, replace)
    }

    fun dismissInsert() {
        insertOffer.value = null
    }

    // ----- turns: forks, failed resumes; items: moving to the background ------------------------

    /**
     * 「ここから分岐」 ([before] `false`: up to and including [turn]) or 「このプロンプトを編集」
     * ([before] `true`: up to right before it, and its prompt goes into the new thread's
     * composer). The new thread opens when the daemon created it.
     */
    fun forkAt(turn: Turn, before: Boolean) {
        forkInto(turn, before, if (before) ThreadActions.promptOf(turn, state.value.thread.items) else null)
    }

    /**
     * 新しいスレッドに分岐 after the failed resume of [failed]: at the last turn that ran before it
     * when the harness can ([ThreadActions.resumeFailedForkPoint]), with [failed]'s prompt waiting
     * in the new thread's composer; otherwise a fork of the whole session.
     */
    fun forkAfterFailedResume(failed: Turn) {
        val ui = state.value
        val at = ThreadActions.resumeFailedForkPoint(ui.harness, ui.thread.turns, failed)
        if (at == null) fork() else forkInto(at, before = false, prompt = ThreadActions.promptOf(failed, ui.thread.items))
    }

    /** `thread/fork { atTurnId: [turn], before }`; [prompt] goes into the new thread's composer. */
    private fun forkInto(turn: Turn, before: Boolean, prompt: Draft?) {
        if (!status.value.isOnline) messages.show(UiText.of(R.string.fork_queued))
        viewModelScope.launch {
            try {
                val thread = threads.fork(threadId, turn.id, before)
                if (prompt != null) {
                    drafts.set(ComposerDrafts.threadKey(thread.id), prompt)
                    messages.show(UiText.of(R.string.fork_edit_opened))
                }
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

    /** 再試行 after a failed resume: the turn's prompt again (a refusal gives it back to the composer). */
    fun retryTurn(turn: Turn) {
        val prompt = ThreadActions.promptOf(turn, state.value.thread.items) ?: return
        val input = ComposerText.input(prompt.text, prompt.mentions, prompt.images.map { it.image.blobId })
        if (input.isEmpty()) return
        commit(input, Delivery.Auto, SentDraft.of(prompt))
    }

    /**
     * 裏に回す (`item/moveToBackground`): the harness moves the running item's work to the
     * background, where it goes on as a background task.
     */
    fun moveToBackground(item: Item) {
        viewModelScope.launch {
            runLocal {
                threads.moveToBackground(threadId, item.id)
                messages.show(UiText.of(if (status.value.isOnline) R.string.move_to_background_requested else R.string.settings_change_queued))
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
                CommandsState.Failed(ErrorTexts.server(e.error))
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
                // Inserted by the composer (the argument follows; sending runs them).
                LocalCommand.Review, LocalCommand.Init, LocalCommand.Plan, LocalCommand.Btw -> Unit
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
            // Confirmed first, as from the menu.
            Methods.ThreadStop.name -> emit(ThreadEvent.ConfirmStop)
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
                    messages.show(ErrorTexts.server(e.error))
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
    fun applySettings(changes: ThreadSettings, modes: ThreadModesUpdate? = null) {
        if (changes == ThreadSettings() && (modes == null || modes == ThreadModesUpdate())) return
        if (!status.value.isOnline) messages.show(UiText.of(R.string.settings_change_queued))
        viewModelScope.launch {
            try {
                val result = threads.updateSettings(threadId, changes, modes?.takeIf { it != ThreadModesUpdate() })
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

    /** `thread/update { title }`; says what happened to the native session's name when the harness has one. */
    fun rename(title: String) {
        val trimmed = title.trim()
        if (trimmed.isEmpty()) return
        viewModelScope.launch {
            runLocal {
                val pending = threads.rename(threadId, trimmed)
                ResultMessages.awaitNativeRename(pending)?.let { messages.show(it) }
            }
        }
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

    /** `thread/fork` of the whole session; the new thread opens when the daemon created it. */
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
            model = model.takeIf { it != null && it != HarnessSettings.model(harness, current)?.id },
            effort = effort.takeIf { it != null && it != current.effort },
        )

        /** The `thread/update` modes for the picker's fast-mode switch: only a change (`null`: unchanged, or not offered). */
        fun fastChange(current: ThreadModes, fast: Boolean?): ThreadModesUpdate? = fast?.takeIf { it != current.fast }?.let { ThreadModesUpdate(fast = it) }
    }
}
