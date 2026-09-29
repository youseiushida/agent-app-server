package dev.aas.android.ui.newproject

import androidx.lifecycle.ViewModel
import androidx.lifecycle.viewModelScope
import dev.aas.android.R
import dev.aas.android.data.ProjectRepository
import dev.aas.android.domain.ErrorTexts
import dev.aas.android.domain.NameProblem
import dev.aas.android.domain.ProjectNames
import dev.aas.android.domain.ResultMessages
import dev.aas.android.domain.ServerPaths
import dev.aas.android.protocol.ErrorKind
import dev.aas.android.protocol.FsEntry
import dev.aas.android.protocol.FsRoot
import dev.aas.android.protocol.Operation
import dev.aas.android.protocol.OperationStatus
import dev.aas.android.protocol.ProjectInit
import dev.aas.android.protocol.RpcException
import dev.aas.android.sync.NotConnectedException
import dev.aas.android.sync.OutboxClearedException
import dev.aas.android.sync.OutboxDiscard
import dev.aas.android.sync.PendingMutation
import dev.aas.android.sync.WorkspaceState
import dev.aas.android.ui.common.UiText
import dev.aas.android.ui.common.requestFailed
import kotlinx.coroutines.CancellationException
import kotlinx.coroutines.Job
import kotlinx.coroutines.channels.Channel
import kotlinx.coroutines.flow.Flow
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.flow.asStateFlow
import kotlinx.coroutines.flow.receiveAsFlow
import kotlinx.coroutines.flow.update
import kotlinx.coroutines.launch

/** How a new project's folder starts (protocol.md `project/create` `init`). */
enum class InitKind { Empty, GitInit, GitClone }

/** What the folder browser is choosing. */
enum class BrowsePurpose {
    /** An existing folder to register (`project/open`). */
    Existing,

    /** The folder the new project's folder is created in (`project/create` `parentPath`). */
    Location,
}

/** A folder listing (`fs/roots` when [path] is null, else `fs/list`). */
sealed interface Listing {
    data object Loading : Listing

    data object Offline : Listing

    data class Failed(val message: UiText) : Listing

    data class Loaded(val entries: List<FsEntry>) : Listing
}

/** The steps of the new-project flow (docs/ux/codex-desktop.md §8.2 新規プロジェクト). */
sealed interface NewProjectStep {
    /** 既存のフォルダーを使用 / 最初から始める. */
    data object Choose : NewProjectStep

    /** 最初から始める: the folder name and how it starts. */
    data class Details(val name: String, val kind: InitKind, val url: String, val nameProblem: NameProblem?, val nameEdited: Boolean) : NewProjectStep {
        val urlMissing: Boolean get() = kind == InitKind.GitClone && url.isBlank()
        val urlLooksWrong: Boolean get() = kind == InitKind.GitClone && url.isNotBlank() && !ProjectNames.looksLikeCloneUrl(url)
        val canContinue: Boolean get() = nameProblem == null && !urlMissing
    }

    /** Walking the PC's folders under the allowed roots. [path] `null`: the roots themselves. */
    data class Browse(val purpose: BrowsePurpose, val path: String?, val listing: Listing, val details: Details?) : NewProjectStep

    /**
     * Waiting for `fs/mkdir` / `project/open` / `project/create` ([message]; [note] says what
     * happens when the screen is left meanwhile). [clientRequestId]: the request, once it is in
     * the outbox (送信を取り消す takes it back while it waits there).
     */
    data class Working(val message: UiText, val note: UiText, val previous: NewProjectStep, val clientRequestId: String? = null) : NewProjectStep

    /** A clone runs: its latest progress line; 取り消す cancels it. */
    data class Cloning(val operation: Operation) : NewProjectStep

    /** The clone ended without a project (failed or cancelled). */
    data class CloneEnded(val operation: Operation, val details: Details, val parentPath: String) : NewProjectStep
}

data class NewProjectUiState(val step: NewProjectStep, val roots: List<FsRoot>, val error: UiText?)

/** Where the flow goes when it is done. */
sealed interface NewProjectEvent {
    /** The project exists: open it, and the new-thread sheet when [startThread]. */
    data class Created(val projectId: String, val startThread: Boolean) : NewProjectEvent

    /** A clone succeeded but its operation named no project: back to the project list. */
    data object Done : NewProjectEvent
}

/**
 * The new-project flow: an existing folder through `fs/roots` + `fs/list` (or a typed path) and
 * `project/open`; or a new folder (empty, `git init`, `git clone <url>`) placed with the same
 * browser (`fs/mkdir` for a new parent folder) and created with `project/create`. A clone is an
 * Operation: its progress comes from the workspace (`operation/updated`), 取り消す is
 * `operation/cancel`. Browsing needs the connection; the flow says so while offline. The
 * changes wait in the outbox while they cannot be sent: the flow can be left meanwhile (they
 * are still sent), or the waiting request taken back.
 */
class NewProjectViewModel(
    private val projects: ProjectRepository,
    private val workspace: StateFlow<WorkspaceState>,
) : ViewModel() {
    private val _state = MutableStateFlow(NewProjectUiState(NewProjectStep.Choose, emptyList(), null))
    val state: StateFlow<NewProjectUiState> = _state.asStateFlow()
    private val events = Channel<NewProjectEvent>(Channel.BUFFERED)
    val eventFlow: Flow<NewProjectEvent> = events.receiveAsFlow()
    private var listJob: Job? = null
    private var cloneJob: Job? = null

    fun chooseExisting() = browse(BrowsePurpose.Existing, null, null)

    fun chooseNew() = set(NewProjectStep.Details("", InitKind.Empty, "", ProjectNames.validate(""), nameEdited = false))

    // ----- details --------------------------------------------------------------------------

    fun setName(name: String) = updateDetails { it.copy(name = name, nameProblem = ProjectNames.validate(name), nameEdited = true) }

    fun setKind(kind: InitKind) = updateDetails { it.copy(kind = kind) }

    /** The URL; until the user typed a name, the name follows the URL (as `git clone` would name it). */
    fun setUrl(url: String) = updateDetails { d ->
        val name = if (d.nameEdited) d.name else ProjectNames.fromCloneUrl(url) ?: d.name
        d.copy(url = url, name = name, nameProblem = ProjectNames.validate(name))
    }

    /** Details are complete: choose where the folder goes. */
    fun toLocation() {
        val details = _state.value.step as? NewProjectStep.Details ?: return
        if (!details.canContinue) return
        browse(BrowsePurpose.Location, null, details)
    }

    // ----- browsing -------------------------------------------------------------------------

    /** Opens [path] (`null`: the roots) in the current browser. */
    fun open(path: String?) {
        val browse = _state.value.step as? NewProjectStep.Browse ?: return
        browse(browse.purpose, path, browse.details)
    }

    /** The folder above, or the roots at a root. */
    fun up() {
        val browse = _state.value.step as? NewProjectStep.Browse ?: return
        val path = browse.path ?: return
        browse(browse.purpose, ServerPaths.parent(path, _state.value.roots), browse.details)
    }

    fun retry() {
        val browse = _state.value.step as? NewProjectStep.Browse ?: return
        browse(browse.purpose, browse.path, browse.details)
    }

    /** A typed path (the daemon checks that it is under a root). */
    fun openTyped(path: String) {
        val trimmed = path.trim()
        if (trimmed.isEmpty()) return
        open(trimmed)
    }

    /** Creates a folder in the current folder and opens it (`fs/mkdir`). */
    fun mkdir(name: String) {
        val browse = _state.value.step as? NewProjectStep.Browse ?: return
        val parent = browse.path ?: return
        val problem = ProjectNames.validate(name.trim())
        if (problem != null) {
            _state.update { it.copy(error = UiText.of(nameProblemText(problem))) }
            return
        }
        val target = ServerPaths.child(parent, name.trim())
        work(UiText.of(R.string.newproject_creating_folder), UiText.of(R.string.newproject_working_note_folder), { projects.mkdir(target) }) { created ->
            browse(browse.purpose, created.path, browse.details)
        }
    }

    /** Existing folder: registers the folder shown (`project/open`). */
    fun openCurrentFolder() {
        val browse = _state.value.step as? NewProjectStep.Browse ?: return
        val path = browse.path ?: return
        work(UiText.of(R.string.newproject_opening), UiText.of(R.string.newproject_working_note_project), { projects.open(path) }) { opened ->
            val project = opened.project
            // A folder registered before comes back with its threads; a new one goes on to its
            // first thread (UX §8.2 作成すると最初のスレッドの作成画面へ進む).
            val hasThreads = workspace.value.threads.any { it.thread.projectId == project.id && !it.thread.archived }
            events.send(NewProjectEvent.Created(project.id, startThread = !hasThreads))
        }
    }

    /** New folder: creates it in the folder shown (`project/create`). */
    fun createHere() {
        val browse = _state.value.step as? NewProjectStep.Browse ?: return
        val parent = browse.path ?: return
        val details = browse.details ?: return
        create(parent, details)
    }

    private fun create(parent: String, details: NewProjectStep.Details) {
        val init = when (details.kind) {
            InitKind.Empty -> ProjectInit.Empty
            InitKind.GitInit -> ProjectInit.GitInit
            InitKind.GitClone -> ProjectInit.GitClone(details.url.trim())
        }
        val message = UiText.of(if (details.kind == InitKind.GitClone) R.string.newproject_starting_clone else R.string.newproject_creating)
        work(message, UiText.of(R.string.newproject_working_note_project), { projects.create(parent, details.name.trim(), init) }) { result ->
            val project = result.project
            val operation = result.operation
            when {
                project != null -> events.send(NewProjectEvent.Created(project.id, startThread = true))
                operation != null -> follow(operation, details, parent)
                else -> _state.update { it.copy(step = details, error = UiText.of(R.string.newproject_no_result)) }
            }
        }
    }

    /** Follows the clone through the workspace's `operation/updated` events. */
    private fun follow(initial: Operation, details: NewProjectStep.Details, parent: String) {
        set(NewProjectStep.Cloning(initial))
        cloneJob?.cancel()
        cloneJob = viewModelScope.launch {
            workspace.collect { ws ->
                val op = ws.operations.firstOrNull { it.id == initial.id } ?: return@collect
                when (op.status) {
                    OperationStatus.Running -> if (_state.value.step is NewProjectStep.Cloning) set(NewProjectStep.Cloning(op))
                    OperationStatus.Succeeded -> {
                        val projectId = op.projectId
                        events.send(if (projectId != null) NewProjectEvent.Created(projectId, startThread = true) else NewProjectEvent.Done)
                        cloneJob?.cancel()
                    }
                    OperationStatus.Failed, OperationStatus.Cancelled, OperationStatus.Unknown -> {
                        set(NewProjectStep.CloneEnded(op, details, parent))
                        cloneJob?.cancel()
                    }
                }
            }
        }
    }

    /** 取り消す (`operation/cancel`): the daemon stops git and removes the partial folder. */
    fun cancelClone() {
        val cloning = _state.value.step as? NewProjectStep.Cloning ?: return
        viewModelScope.launch {
            try {
                projects.cancelOperation(cloning.operation.id)
            } catch (e: CancellationException) {
                throw e
            } catch (e: Exception) {
                _state.update { it.copy(error = UiText.of(R.string.error_local_store, e.message ?: e.javaClass.simpleName)) }
            }
        }
    }

    /**
     * 送信を取り消す while a request waits ([NewProjectStep.Working]): it leaves the outbox and is
     * never sent; the flow returns to where it was ([work] sees the discard). A request on the
     * wire right now stays (the daemon may be running it), and the flow says so.
     */
    fun discardWork() {
        val working = _state.value.step as? NewProjectStep.Working ?: return
        val clientRequestId = working.clientRequestId ?: return
        viewModelScope.launch {
            try {
                val outcome = projects.discard(clientRequestId)
                if (outcome != OutboxDiscard.Discarded) _state.update { it.copy(error = ResultMessages.discarded(outcome)) }
            } catch (e: CancellationException) {
                throw e
            } catch (e: Exception) {
                _state.update { it.copy(error = UiText.of(R.string.error_local_store, e.message ?: e.javaClass.simpleName)) }
            }
        }
    }

    /** After a failed clone: the same clone again. */
    fun retryClone() {
        val ended = _state.value.step as? NewProjectStep.CloneEnded ?: return
        create(ended.parentPath, ended.details)
    }

    fun dismissError() = _state.update { it.copy(error = null) }

    /**
     * Back within the flow; `false` when the screen closes instead: at the flow's start, and
     * while a request or a clone goes on without it.
     */
    fun back(): Boolean {
        val step = _state.value.step
        val previous: NewProjectStep? = when (step) {
            NewProjectStep.Choose -> null
            is NewProjectStep.Details -> NewProjectStep.Choose
            is NewProjectStep.Browse -> when {
                step.path != null -> {
                    up()
                    return true
                }
                step.details != null -> step.details
                else -> NewProjectStep.Choose
            }
            // The request stays in the outbox and is sent once connected: the project shows up in
            // the list (a new folder is simply there). Waiting here could take hours (offline,
            // the PC asleep, the daemon stopped), so leaving must be possible.
            is NewProjectStep.Working -> null
            // The clone goes on without the screen (the project list shows it).
            is NewProjectStep.Cloning -> null
            is NewProjectStep.CloneEnded -> step.details
        }
        if (previous == null) return false
        set(previous)
        return true
    }

    private fun browse(purpose: BrowsePurpose, path: String?, details: NewProjectStep.Details?) {
        _state.update { it.copy(step = NewProjectStep.Browse(purpose, path, Listing.Loading, details), error = null) }
        listJob?.cancel()
        listJob = viewModelScope.launch {
            val listing = try {
                if (path == null) {
                    val roots = projects.roots()
                    _state.update { it.copy(roots = roots) }
                    Listing.Loaded(roots.map { FsEntry(name = it.name, path = it.path, isDir = true) })
                } else {
                    if (_state.value.roots.isEmpty()) _state.update { it.copy(roots = projects.roots()) }
                    val result = projects.list(path)
                    // The daemon normalises the path (a typed path may differ in case or separators).
                    _state.update { ui ->
                        val step = ui.step as? NewProjectStep.Browse
                        if (step != null && step.path == path) ui.copy(step = step.copy(path = result.path)) else ui
                    }
                    Listing.Loaded(result.entries.filter { it.isDir })
                }
            } catch (e: CancellationException) {
                throw e
            } catch (e: NotConnectedException) {
                Listing.Offline
            } catch (e: RpcException) {
                Listing.Failed(describe(e))
            } catch (e: Exception) {
                Listing.Failed(requestFailed(e))
            }
            _state.update { ui ->
                val step = ui.step as? NewProjectStep.Browse ?: return@update ui
                ui.copy(step = step.copy(listing = listing))
            }
        }
    }

    /**
     * Runs a change that waits for the daemon: [submit] commits it to the outbox, [then] gets
     * the answer. [message] and [note] show meanwhile, with 送信を取り消す once the request is
     * in the outbox ([discardWork]).
     */
    private fun <T> work(message: UiText, note: UiText, submit: suspend () -> PendingMutation<T>, then: suspend (T) -> Unit) {
        val previous = _state.value.step
        _state.update { it.copy(step = NewProjectStep.Working(message, note, previous), error = null) }
        viewModelScope.launch {
            try {
                val pending = submit()
                _state.update { ui ->
                    val working = ui.step as? NewProjectStep.Working ?: return@update ui
                    ui.copy(step = working.copy(clientRequestId = pending.clientRequestId))
                }
                then(pending.await())
            } catch (e: CancellationException) {
                throw e
            } catch (e: RpcException) {
                _state.update { it.copy(step = previous, error = describe(e)) }
            } catch (e: OutboxClearedException) {
                // Taken back (送信を取り消す), or the device was unpaired meanwhile (the shell
                // shows pairing): the flow is where it was.
                _state.update { it.copy(step = previous) }
            } catch (e: Exception) {
                _state.update { it.copy(step = previous, error = UiText.of(R.string.error_local_store, e.message ?: e.javaClass.simpleName)) }
            }
        }
    }

    private fun set(step: NewProjectStep) = _state.update { it.copy(step = step, error = null) }

    private fun updateDetails(change: (NewProjectStep.Details) -> NewProjectStep.Details) {
        val details = _state.value.step as? NewProjectStep.Details ?: return
        set(change(details))
    }

    companion object {
        fun describe(e: RpcException): UiText = when (e.kind) {
            ErrorKind.PathNotAllowed -> UiText.of(R.string.newproject_path_not_allowed)
            ErrorKind.NotFound -> UiText.of(R.string.newproject_not_found)
            ErrorKind.AlreadyExists -> UiText.of(R.string.newproject_already_exists)
            ErrorKind.InvalidState -> UiText.of(R.string.newproject_invalid_state, e.error.message)
            else -> ErrorTexts.server(e.error)
        }

        fun nameProblemText(problem: NameProblem): Int = when (problem) {
            NameProblem.Empty -> R.string.newproject_name_empty
            NameProblem.InvalidCharacters -> R.string.newproject_name_invalid_chars
            NameProblem.NotAllowed -> R.string.newproject_name_not_allowed
        }
    }
}
