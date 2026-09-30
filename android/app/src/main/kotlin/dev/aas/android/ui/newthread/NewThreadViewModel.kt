package dev.aas.android.ui.newthread

import androidx.lifecycle.ViewModel
import androidx.lifecycle.viewModelScope
import dev.aas.android.AppPolicy
import dev.aas.android.R
import dev.aas.android.data.ComposerDrafts
import dev.aas.android.data.HarnessRepository
import dev.aas.android.data.ImageUploader
import dev.aas.android.data.ProjectRepository
import dev.aas.android.data.SentDrafts
import dev.aas.android.data.ThreadRepository
import dev.aas.android.domain.ErrorTexts
import dev.aas.android.domain.HarnessWait
import dev.aas.android.domain.NativeSessionHarnesses
import dev.aas.android.domain.ResultMessages
import dev.aas.android.domain.composer.ComposerText
import dev.aas.android.domain.composer.HarnessSettings
import dev.aas.android.domain.composer.LocalCommand
import dev.aas.android.domain.composer.Palette
import dev.aas.android.domain.composer.PaletteAction
import dev.aas.android.domain.composer.PaletteContext
import dev.aas.android.domain.composer.PaletteEntry
import dev.aas.android.domain.composer.SendAction
import dev.aas.android.domain.composer.SendBlock
import dev.aas.android.domain.composer.SendLogic
import dev.aas.android.domain.composer.SendState
import dev.aas.android.domain.composer.TypedCommand
import dev.aas.android.domain.composer.TypedCommands
import dev.aas.android.protocol.Harness
import dev.aas.android.protocol.InputPart
import dev.aas.android.protocol.PickerKind
import dev.aas.android.protocol.Project
import dev.aas.android.protocol.ProjectDefaults
import dev.aas.android.protocol.RpcException
import dev.aas.android.protocol.ThreadSettings
import dev.aas.android.protocol.WorkspaceSpec
import dev.aas.android.sync.NotConnectedException
import dev.aas.android.sync.OutboxClearedException
import dev.aas.android.sync.OutboxEntry
import dev.aas.android.sync.SyncStatus
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
import dev.aas.android.ui.navigation.NewThreadRoute
import kotlinx.coroutines.CancellationException
import kotlinx.coroutines.CoroutineStart
import kotlinx.coroutines.NonCancellable
import kotlinx.coroutines.channels.Channel
import kotlinx.coroutines.flow.Flow
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.SharingStarted
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.flow.combine
import kotlinx.coroutines.flow.distinctUntilChanged
import kotlinx.coroutines.flow.map
import kotlinx.coroutines.flow.receiveAsFlow
import kotlinx.coroutines.flow.stateIn
import kotlinx.coroutines.launch
import kotlinx.coroutines.withContext

/** The choices of the new-thread sheet. */
data class NewThreadChoices(
    /** `null` until chosen (then the route's harness, the project's last one, or the first available). */
    val harnessId: String?,
    val settings: ThreadSettings,
    val worktree: Boolean,
    val branch: String,
    val baseRef: String,
)

data class NewThreadUiState(
    val project: Project?,
    val harnesses: List<Harness>,
    val harness: Harness?,
    val choices: NewThreadChoices,
    val creating: Boolean,
    val online: Boolean,
    val send: SendState,
    /** Harnesses this app is probing right now (再確認). */
    val probing: Set<String> = emptySet(),
    /** The creation waits for its harness (the server answered `harnessUnavailable`). */
    val waiting: HarnessWait? = null,
) {
    /** A worktree needs a git repository (protocol.md `thread/create`). */
    val worktreeAvailable: Boolean get() = project?.git?.isRepo == true

    /**
     * The chosen harness asks per project whether it may load the project's own resources
     * (`features.projectTrust`) and the user has not decided for this project yet.
     */
    val trustUndecided: Boolean
        get() = harness?.features?.projectTrust == true && project != null && harness.id !in project.harnessTrust
}

sealed interface NewThreadEvent {
    data class Created(val threadId: String) : NewThreadEvent

    /** Opens a picker; [model]: the model sheet opens with that model chosen (`/model` of one the mode does not fit). */
    data class OpenPicker(val kind: PickerKind, val model: String? = null) : NewThreadEvent

    /** `/resume`: 「PC のセッションを取り込む」 of [projectId], listing [harnessId]'s sessions first. */
    data class OpenImport(val projectId: String, val harnessId: String?) : NewThreadEvent
}

/**
 * A new thread (docs/ux/codex-desktop.md §8.2 スレッド作成時の指定): harness, model, effort,
 * permission and workspace (local or a new worktree) on one sheet, then the first message.
 * `thread/create` with `input` starts the first turn at once, and the project's `defaults`
 * become these choices (`project/update`, "前回値を引き継ぐ"): both are committed to the outbox
 * together, in this order, before the answer is awaited, so leaving the screen (offline, the
 * creation is only queued) loses neither. `/plan <request>` commits plan mode and the request
 * with the creation the same way (a chain that takes the created thread's id).
 */
class NewThreadViewModel(
    private val route: NewThreadRoute,
    private val workspace: StateFlow<WorkspaceState>,
    private val threads: ThreadRepository,
    private val projects: ProjectRepository,
    private val status: StateFlow<SyncStatus>,
    private val messages: UserMessages,
    drafts: ComposerDrafts,
    uploader: ImageUploader,
    policy: AppPolicy,
    private val templates: PromptTemplates,
    outbox: StateFlow<List<OutboxEntry>>,
    harnesses: HarnessRepository,
    private val sentDrafts: SentDrafts,
) : ViewModel() {
    private val harnessRepository = harnesses
    private val choices = MutableStateFlow(NewThreadChoices(null, ThreadSettings(), worktree = false, branch = "", baseRef = ""))
    private val creating = MutableStateFlow(false)

    /** The `clientRequestId` of the `thread/create` being awaited. */
    private val creatingId = MutableStateFlow<String?>(null)
    private val refresher = HarnessRefresher(harnesses, messages)
    private val events = Channel<NewThreadEvent>(Channel.BUFFERED)
    private var commandsFor: String? = null

    val eventFlow: Flow<NewThreadEvent> = events.receiveAsFlow()

    val composer = ComposerController(
        scope = viewModelScope,
        uploader = uploader,
        search = { query -> projects.search(route.projectId, query) },
        policy = policy,
        drafts = drafts,
        draftKey = ComposerDrafts.newThreadKey(route.projectId),
        templates = templates,
        paletteContext = paletteContext(workspace.value),
    )

    val state: StateFlow<NewThreadUiState> = combine(
        workspace,
        choices,
        combine(creating, creatingId) { busy, id -> busy to id },
        combine(status, composer.state) { st, comp -> st to comp },
        combine(outbox, harnesses.refreshing) { out, probing -> out to probing },
    ) { ws, c, (busy, id), (st, comp), (out, probing) ->
        val waiting = id?.let { crid -> out.firstOrNull { it.clientRequestId == crid } }?.let { HarnessWait.of(it, ws.harnesses) }
        build(ws, c, busy, st, comp).copy(probing = probing, waiting = waiting)
    }.stateIn(viewModelScope, SharingStarted.WhileSubscribed(policy.uiStopTimeoutMs), build(workspace.value, choices.value, false, status.value, composer.state.value))

    private fun build(ws: WorkspaceState, c: NewThreadChoices, busy: Boolean, st: SyncStatus, comp: ComposerUiState): NewThreadUiState {
        val project = ws.projects.firstOrNull { it.id == route.projectId }
        val harness = harnessOf(ws, c, project)
        val blocked = when {
            harness == null || !harness.available -> SendBlock.NotLoaded
            busy -> SendBlock.Interrupting
            // The model and mode would be refused together (the harness's lists changed since they were chosen).
            !HarnessSettings.offersPermission(harness, HarnessSettings.model(harness, c.settings)?.id, c.settings) -> SendBlock.PermissionUnavailable
            // Uploads, and images for a harness that takes none (switched after attaching them).
            else -> SendLogic.attachmentsBlock(harness.capabilities, comp.uploading, comp.uploadFailed, comp.hasImages)
                // A thread starts with a message (the first turn); an image alone is a message too.
                ?: if (!comp.hasContent) SendBlock.Empty else null
        }
        return NewThreadUiState(project, ws.harnesses, harness, c.copy(harnessId = harness?.id), busy, st.isOnline, SendState(SendAction.Start, null, blocked))
    }

    /** The chosen harness, else the initial one; its settings start from the project's defaults. */
    private fun harnessOf(ws: WorkspaceState, c: NewThreadChoices, project: Project?): Harness? {
        c.harnessId?.let { id -> return ws.harnesses.firstOrNull { it.id == id } }
        val initial = route.harnessId?.let { id -> ws.harnesses.firstOrNull { it.id == id && it.available } }
            ?: HarnessSettings.initialHarness(ws.harnesses, project?.defaults ?: ProjectDefaults())
            ?: return null
        // First sight of the harness: take it with its starting settings.
        choices.compareAndSet(c, c.copy(harnessId = initial.id, settings = HarnessSettings.initial(initial, project?.defaults ?: ProjectDefaults())))
        return initial
    }

    init {
        // `/resume` is offered while some harness can list its sessions; `/plan` where the chosen
        // harness offers the app's plan mode.
        viewModelScope.launch {
            combine(workspace, choices) { ws, c -> paletteContext(ws, c.harnessId) }.distinctUntilChanged().collect { composer.setPaletteContext(it) }
        }
    }

    private fun paletteContext(ws: WorkspaceState, harnessId: String? = null) = PaletteContext(
        inThread = false,
        canImport = NativeSessionHarnesses.canImport(ws.harnesses),
        planMode = harnessId?.let { id -> ws.harnesses.firstOrNull { it.id == id } }?.features?.planMode != null,
    )

    /**
     * The user's decision whether the chosen harness may load this project's own resources
     * (`features.projectTrust`): asked here, never decided by the app.
     */
    fun setTrust(trusted: Boolean) {
        val ui = state.value
        val harness = ui.harness ?: return
        viewModelScope.launch {
            try {
                harnessRepository.setTrust(route.projectId, harness.id, trusted)
                messages.show(UiText.of(if (trusted) R.string.trust_saved_yes else R.string.trust_saved_no, harness.displayName))
            } catch (e: CancellationException) {
                throw e
            } catch (e: Exception) {
                messages.show(UiText.of(R.string.error_local_store, e.message ?: e.javaClass.simpleName))
            }
        }
    }

    /** 再確認: `harness/refresh` of one harness, or of all when [harnessId] is `null`. */
    fun refreshHarness(harnessId: String?) = refresher.refresh(viewModelScope, harnessId)

    /**
     * Withdraws a creation that waits for its harness: it leaves the outbox (never sent again)
     * and the message comes back to the composer ([create] sees the discard).
     */
    fun discardCreation() {
        val crid = creatingId.value ?: return
        viewModelScope.launch {
            try {
                messages.show(ResultMessages.discarded(threads.discardPending(crid)))
            } catch (e: CancellationException) {
                throw e
            } catch (e: Exception) {
                messages.show(UiText.of(R.string.error_local_store, e.message ?: e.javaClass.simpleName))
            }
        }
    }

    fun selectHarness(harness: Harness) {
        if (!harness.available) return
        val project = state.value.project
        choices.value = choices.value.copy(harnessId = harness.id, settings = HarnessSettings.initial(harness, project?.defaults ?: ProjectDefaults()))
        composer.setCommands(CommandsState.Idle)
        commandsFor = null
    }

    /**
     * The model sheet's choice (`null` effort: the harness default). [permissionMode]: the mode
     * chosen for a model that does not run in the one in effect (`null`: the mode stays).
     */
    fun setModel(model: String?, effort: String?, permissionMode: String? = null) {
        val settings = choices.value.settings
        choices.value = choices.value.copy(settings = settings.copy(model = model, effort = effort, permissionMode = permissionMode ?: settings.permissionMode))
    }

    fun setPermission(permissionMode: String) {
        choices.value = choices.value.copy(settings = choices.value.settings.copy(permissionMode = permissionMode))
    }

    fun setWorktree(worktree: Boolean) {
        choices.value = choices.value.copy(worktree = worktree)
    }

    fun setBranch(branch: String) {
        choices.value = choices.value.copy(branch = branch)
    }

    fun setBaseRef(baseRef: String) {
        choices.value = choices.value.copy(baseRef = baseRef)
    }

    fun pickImages(uris: List<String>) {
        val dropped = composer.addImages(uris)
        if (dropped > 0) messages.show(UiText.of(R.string.composer_too_many_images, dropped))
    }

    /** `command/list { projectId, harnessId }` for the palette. */
    fun loadCommands() {
        val harnessId = state.value.harness?.id ?: return
        if (commandsFor == harnessId && composer.state.value.commands is CommandsState.Loaded) return
        if (!status.value.isOnline) {
            composer.setCommands(CommandsState.Offline)
            return
        }
        composer.setCommands(CommandsState.Loading)
        viewModelScope.launch {
            val result = try {
                CommandsState.Loaded(projects.commands(route.projectId, harnessId)).also { commandsFor = harnessId }
            } catch (e: CancellationException) {
                throw e
            } catch (e: NotConnectedException) {
                CommandsState.Offline
            } catch (e: RpcException) {
                CommandsState.Failed(ErrorTexts.server(e.error))
            } catch (e: Exception) {
                CommandsState.Failed(requestFailed(e))
            }
            if (state.value.harness?.id == harnessId) composer.setCommands(result)
        }
    }

    fun choose(entry: PaletteEntry) {
        when (val choice = composer.choose(entry)) {
            PaletteChoice.Inserted -> Unit
            is PaletteChoice.Picker -> viewModelScope.launch { events.send(NewThreadEvent.OpenPicker(choice.kind)) }
            // Thread commands do not exist before the thread (the daemon does not list them here).
            is PaletteChoice.Method -> messages.show(UiText.of(R.string.palette_needs_thread))
            is PaletteChoice.Local -> when (choice.command) {
                // Inserted by the composer (sending runs `/plan` with its request).
                LocalCommand.Review, LocalCommand.Init, LocalCommand.Plan -> Unit
                LocalCommand.Resume -> resume()
                LocalCommand.New, LocalCommand.Status, LocalCommand.Rename, LocalCommand.Pin, LocalCommand.Btw ->
                    messages.show(UiText.of(R.string.palette_needs_thread))
            }
            is PaletteChoice.Unsupported -> messages.show(UiText.of(R.string.palette_unsupported_type, choice.type))
        }
    }

    /**
     * `thread/create` with the first message, and the choices as the project's defaults. The
     * draft leaves the composer at once (the requests are in the outbox); offline, the thread is
     * created when the connection is back. It comes back (text, mentions and images) when the
     * request cannot be queued or the daemon refuses it — to this composer; a `/plan` request
     * refused after the thread exists comes back in that thread's composer.
     */
    fun create() {
        val text = composer.textValue.text
        if (Palette.isResume(text)) {
            // Typed out instead of chosen: the app's `/resume`, never sent to the harness.
            composer.setText("")
            resume()
            return
        }
        TypedCommands.split(text)?.let { (name, args) ->
            if (name in LocalCommand.New.names) {
                // `/new`, `/clear`, `/reset`: this is a new conversation already; what follows stays.
                composer.setText(args)
                messages.show(UiText.of(R.string.newthread_already_new, name))
                return
            }
        }
        val ui = state.value
        val harness = ui.harness ?: return
        if (!ui.send.enabled || creating.value) return
        val typed = composer.typedCommand(Palette.protocolAppCommands(harness, inThread = false))
        if (typed != null) {
            runTyped(typed)
            return
        }
        val input = composer.input()
        if (input.isEmpty()) return
        start(input, composer.sentDraft(), plan = false)
    }

    /**
     * The app's command the first message starts with (docs/android.md 25章 「打ったコマンド」):
     * `/plan <request>` creates the thread in plan mode with the request as its first message, the
     * prompt templates start it with their text, `/model` and `/effort` choose from the harness's
     * lists (the picker without an argument or with one the lists do not have), the permission
     * picker opens; the thread's own commands wait for the thread.
     */
    private fun runTyped(typed: TypedCommand) {
        val args = typed.args
        when (val action = typed.entry.action) {
            is PaletteAction.Local -> when (action.command) {
                LocalCommand.Plan -> {
                    val draft = composer.draftWith(args)
                    val input = ComposerText.input(args, draft.mentions, draft.images.map { it.image.blobId })
                    if (input.isEmpty()) {
                        messages.show(UiText.of(R.string.newthread_plan_needs_request))
                        return
                    }
                    start(input, composer.sentDraft(), plan = true)
                }
                LocalCommand.Review, LocalCommand.Init -> {
                    val template = if (action.command == LocalCommand.Review) templates.review else templates.init
                    val text = if (args.isEmpty()) template else ComposerText.appendParagraph(template, args)
                    val draft = composer.draftWith(text)
                    start(ComposerText.input(text, draft.mentions, draft.images.map { it.image.blobId }), composer.sentDraft(), plan = false)
                }
                LocalCommand.Resume -> resume()
                LocalCommand.New, LocalCommand.Status, LocalCommand.Rename, LocalCommand.Pin, LocalCommand.Btw ->
                    messages.show(UiText.of(R.string.palette_needs_thread))
            }
            is PaletteAction.Picker -> {
                composer.setText("")
                pick(action.kind, args)
            }
            // Thread commands do not exist before the thread (the daemon does not list them here).
            is PaletteAction.Method -> messages.show(UiText.of(R.string.palette_needs_thread))
            is PaletteAction.Insert -> composer.setText(action.text)
            is PaletteAction.Unsupported -> messages.show(UiText.of(R.string.palette_unsupported_type, action.type))
        }
    }

    /** `/model <id>` and `/effort <level>` of the new thread (see [runTyped]). */
    private fun pick(kind: PickerKind, args: String) {
        val harness = state.value.harness
        if (args.isEmpty() || harness == null) {
            viewModelScope.launch { events.send(NewThreadEvent.OpenPicker(kind)) }
            return
        }
        val settings = choices.value.settings
        when (kind) {
            PickerKind.Model -> {
                val model = harness.models.firstOrNull { it.id == args || it.displayName.equals(args, ignoreCase = true) }
                if (model == null) {
                    messages.show(UiText.of(R.string.typed_model_unknown, args))
                    viewModelScope.launch { events.send(NewThreadEvent.OpenPicker(kind)) }
                } else if (!HarnessSettings.offersPermission(harness, model.id, settings)) {
                    // As the model sheet does: that model with a mode it runs in, chosen there.
                    val mode = HarnessSettings.permission(harness, settings)?.label ?: settings.permissionMode.orEmpty()
                    messages.show(UiText.of(R.string.typed_model_permission_unavailable, model.displayName, mode))
                    viewModelScope.launch { events.send(NewThreadEvent.OpenPicker(kind, model = model.id)) }
                } else {
                    // An effort the new model does not offer is dropped (the harness default applies).
                    val effort = settings.effort?.takeIf { e -> HarnessSettings.effortLevels(harness, model.id).any { it.id == e } }
                    setModel(model.id, effort)
                }
            }
            PickerKind.Effort -> {
                val level = HarnessSettings.effortLevels(harness, HarnessSettings.model(harness, settings)?.id)
                    .firstOrNull { it.id == args || it.label.equals(args, ignoreCase = true) }
                if (level == null) {
                    messages.show(UiText.of(R.string.typed_effort_unknown, args))
                    viewModelScope.launch { events.send(NewThreadEvent.OpenPicker(kind)) }
                } else {
                    setModel(settings.model, level.id)
                }
            }
            PickerKind.PermissionMode, PickerKind.Unknown -> viewModelScope.launch { events.send(NewThreadEvent.OpenPicker(kind)) }
        }
    }

    /**
     * `thread/create` with [input] as the first message and the choices as the project's
     * defaults. With [plan] (`/plan`), the thread is created without input and plan mode
     * (`thread/update { modes }`) and the request (its first `turn/start`) follow it as one chain
     * ([ThreadRepository.createInPlanMode]; `thread/create` takes no modes). Everything is
     * committed before anything is awaited, so leaving the screen while the creation waits (or
     * the process ending) loses none of it. [draft] comes back to the composer when the creation
     * cannot be queued, is refused ([SentDrafts.followCreation], also after this screen is gone)
     * or is taken back ([discardCreation]).
     */
    private fun start(input: List<InputPart>, draft: SentDraft, plan: Boolean) {
        val ui = state.value
        val harness = ui.harness ?: return
        val c = ui.choices
        val settings = c.settings.takeIf { it != ThreadSettings() }
        val workspace = if (c.worktree && ui.worktreeAvailable) {
            WorkspaceSpec.Worktree(baseRef = c.baseRef.trim().ifEmpty { null }, branch = c.branch.trim().ifEmpty { null })
        } else {
            WorkspaceSpec.Local
        }
        val defaults = ProjectDefaults(harness.id, c.settings.model, c.settings.effort, c.settings.permissionMode)
        creating.value = true
        if (!status.value.isOnline) messages.show(UiText.of(R.string.newthread_queued))
        composer.clear()
        // Undispatched: the commits start at once, so leaving the screen right after sending
        // (the scope is cancelled) cannot keep them from happening.
        viewModelScope.launch(start = CoroutineStart.UNDISPATCHED) {
            try {
                // Committed without cancellation (a local transaction each): leaving the screen
                // right after sending must not keep the defaults from being queued behind it.
                val creation = try {
                    withContext(NonCancellable) {
                        val (pending, request) = if (plan) {
                            threads.createInPlanMode(route.projectId, harness.id, settings, workspace, input).let { it.creation to it.request }
                        } else {
                            threads.create(route.projectId, harness.id, settings, workspace, input) to null
                        }
                        creatingId.value = pending.clientRequestId
                        sentDrafts.followCreation(ComposerDrafts.newThreadKey(route.projectId), pending, request, draft.toDraft())
                        queueDefaults(defaults)
                        pending
                    }
                } catch (e: CancellationException) {
                    throw e
                } catch (e: Exception) {
                    // Not even in the outbox: the draft comes back.
                    composer.restore(draft)
                    messages.show(UiText.of(R.string.error_local_store, e.message ?: e.javaClass.simpleName))
                    return@launch
                }
                try {
                    events.send(NewThreadEvent.Created(creation.await().thread.id))
                } catch (e: RpcException) {
                    // The shell reports the definitive failure; the message comes back to edit
                    // through SentDrafts.followCreation.
                } catch (e: OutboxClearedException) {
                    // Taken back (discardCreation), or unpaired meanwhile.
                    composer.restore(draft)
                } catch (e: IllegalArgumentException) {
                    // An answer of another shape than protocol.md's (SerializationException is
                    // one): the thread exists, but this screen cannot tell which one to open.
                    messages.show(requestFailed(e))
                }
            } finally {
                creating.value = false
                creatingId.value = null
            }
        }
    }

    /**
     * `/resume`: continue a session of the PC instead of starting a new one — 「PC のセッションを
     * 取り込む」 of this project, listing the chosen harness's sessions first.
     */
    fun resume() {
        if (!NativeSessionHarnesses.canImport(workspace.value.harnesses)) {
            messages.show(UiText.of(R.string.import_no_harness))
            return
        }
        viewModelScope.launch { events.send(NewThreadEvent.OpenImport(route.projectId, choices.value.harnessId)) }
    }

    /** `project/update { defaults }` behind the `thread/create` in the project's lane. */
    private suspend fun queueDefaults(defaults: ProjectDefaults) {
        try {
            projects.setDefaults(route.projectId, defaults)
        } catch (e: CancellationException) {
            throw e
        } catch (e: Exception) {
            // The thread is queued; only the defaults were not (a local store failure).
            messages.show(UiText.of(R.string.error_local_store, e.message ?: e.javaClass.simpleName))
        }
    }
}
