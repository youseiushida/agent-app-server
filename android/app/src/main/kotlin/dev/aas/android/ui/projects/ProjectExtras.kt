package dev.aas.android.ui.projects

import androidx.compose.foundation.clickable
import androidx.compose.foundation.horizontalScroll
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.foundation.lazy.items
import androidx.compose.foundation.rememberScrollState
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.automirrored.outlined.ArrowBack
import androidx.compose.material3.CircularProgressIndicator
import androidx.compose.material3.ExperimentalMaterial3Api
import androidx.compose.material3.FilterChip
import androidx.compose.material3.HorizontalDivider
import androidx.compose.material3.Icon
import androidx.compose.material3.IconButton
import androidx.compose.material3.ListItem
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.OutlinedButton
import androidx.compose.material3.Scaffold
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.material3.TopAppBar
import androidx.compose.runtime.Composable
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.getValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.platform.testTag
import androidx.compose.ui.res.stringResource
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.unit.dp
import androidx.lifecycle.ViewModel
import androidx.lifecycle.compose.collectAsStateWithLifecycle
import androidx.lifecycle.viewModelScope
import dev.aas.android.AppPolicy
import dev.aas.android.R
import dev.aas.android.data.HarnessRepository
import dev.aas.android.data.ProjectRepository
import dev.aas.android.data.ThreadRepository
import dev.aas.android.data.WorkspaceRepository
import dev.aas.android.domain.HarnessWait
import dev.aas.android.domain.NativeSessionHarnesses
import dev.aas.android.domain.ResultMessages
import dev.aas.android.protocol.ErrorKind
import dev.aas.android.protocol.Harness
import dev.aas.android.protocol.NativeSession
import dev.aas.android.protocol.RpcException
import dev.aas.android.protocol.Thread
import dev.aas.android.protocol.ThreadCursor
import dev.aas.android.sync.NotConnectedException
import dev.aas.android.sync.OutboxClearedException
import dev.aas.android.sync.OutboxEntry
import dev.aas.android.ui.common.HarnessRefresher
import dev.aas.android.ui.common.UiText
import dev.aas.android.ui.common.UserMessages
import dev.aas.android.ui.common.asString
import dev.aas.android.ui.common.requestFailed
import dev.aas.android.ui.components.EmptyState
import dev.aas.android.ui.components.HarnessWaitNotice
import dev.aas.android.ui.components.relativeTime
import dev.aas.android.ui.icons.Archive
import dev.aas.android.ui.icons.History
import dev.aas.android.ui.navigation.AppNavigator
import kotlinx.coroutines.CancellationException
import kotlinx.coroutines.Job
import kotlinx.coroutines.channels.Channel
import kotlinx.coroutines.flow.Flow
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.SharingStarted
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.flow.asStateFlow
import kotlinx.coroutines.flow.combine
import kotlinx.coroutines.flow.merge
import kotlinx.coroutines.flow.receiveAsFlow
import kotlinx.coroutines.flow.stateIn
import kotlinx.coroutines.flow.update
import kotlinx.coroutines.launch

/** A list fetched from the daemon (it needs the connection). */
sealed interface Fetched<out T> {
    data object Loading : Fetched<Nothing>

    data object Offline : Fetched<Nothing>

    data class Failed(val message: UiText) : Fetched<Nothing>

    data class Loaded<T>(val value: T) : Fetched<T>
}

/** Runs a read-only call and describes its failure. */
internal suspend fun <T> fetched(block: suspend () -> T): Fetched<T> = try {
    Fetched.Loaded(block())
} catch (e: CancellationException) {
    throw e
} catch (e: NotConnectedException) {
    Fetched.Offline
} catch (e: RpcException) {
    Fetched.Failed(UiText.of(R.string.error_server, e.error.message))
} catch (e: Exception) {
    Fetched.Failed(requestFailed(e))
}

// ----- archived threads ------------------------------------------------------------------------

data class ArchivedThreadsUiState(val threads: Fetched<List<Thread>>, val hasMore: Boolean, val loadingMore: Boolean, val restoring: Set<String>)

/**
 * The archived threads of a project: only the daemon has them (the workspace holds the active
 * ones), paged with `thread/list { includeArchived, before }`.
 */
class ArchivedThreadsViewModel(
    private val projectId: String,
    private val projects: ProjectRepository,
    private val threads: ThreadRepository,
    private val messages: UserMessages,
) : ViewModel() {
    private val _state = MutableStateFlow(ArchivedThreadsUiState(Fetched.Loading, false, false, emptySet()))
    val state: StateFlow<ArchivedThreadsUiState> = _state.asStateFlow()
    private var cursor: ThreadCursor? = null

    init {
        reload()
    }

    fun reload() {
        cursor = null
        _state.update { it.copy(threads = Fetched.Loading) }
        viewModelScope.launch { page(append = false) }
    }

    fun loadMore() {
        if (_state.value.loadingMore || !_state.value.hasMore) return
        _state.update { it.copy(loadingMore = true) }
        viewModelScope.launch { page(append = true) }
    }

    private suspend fun page(append: Boolean) {
        val result = fetched { projects.threads(projectId, includeArchived = true, before = cursor) }
        _state.update { ui ->
            when (result) {
                is Fetched.Loaded -> {
                    // The page as the server sent it gives the next cursor; the list shows each thread once.
                    val list = result.value.threads
                    cursor = list.lastOrNull()?.let { ThreadCursor(it.lastActivityAt, it.id) }
                    val archived = list.filter { it.archived }
                    val previous = if (append) (ui.threads as? Fetched.Loaded)?.value.orEmpty() else emptyList()
                    ui.copy(threads = Fetched.Loaded(projects.joinThreadPages(previous, archived)), hasMore = result.value.hasMore, loadingMore = false)
                }
                else -> if (append) {
                    messages.show((result as? Fetched.Failed)?.message ?: UiText.of(R.string.error_not_connected))
                    ui.copy(loadingMore = false)
                } else {
                    @Suppress("UNCHECKED_CAST")
                    ui.copy(threads = result as Fetched<List<Thread>>, loadingMore = false)
                }
            }
        }
    }

    /** Takes the thread out of the archive (it goes back to the project's list). */
    fun unarchive(thread: Thread) {
        _state.update { it.copy(restoring = it.restoring + thread.id) }
        viewModelScope.launch {
            try {
                threads.archive(thread.id, archived = false)
                messages.show(UiText.of(R.string.thread_unarchived, thread.title))
                _state.update { ui ->
                    val list = (ui.threads as? Fetched.Loaded)?.value?.filterNot { it.id == thread.id }
                    ui.copy(threads = list?.let { Fetched.Loaded(it) } ?: ui.threads, restoring = ui.restoring - thread.id)
                }
            } catch (e: CancellationException) {
                throw e
            } catch (e: Exception) {
                _state.update { it.copy(restoring = it.restoring - thread.id) }
                messages.show(UiText.of(R.string.error_local_store, e.message ?: e.javaClass.simpleName))
            }
        }
    }
}

@OptIn(ExperimentalMaterial3Api::class)
@Composable
fun ArchivedThreadsScreen(vm: ArchivedThreadsViewModel, navigator: AppNavigator) {
    val ui by vm.state.collectAsStateWithLifecycle()
    Scaffold(
        topBar = {
            TopAppBar(
                title = { Text(stringResource(R.string.threads_archived)) },
                navigationIcon = { IconButton(onClick = navigator::back) { Icon(Icons.AutoMirrored.Outlined.ArrowBack, contentDescription = stringResource(R.string.back)) } },
            )
        },
    ) { padding ->
        Box(Modifier.fillMaxSize().padding(padding)) {
            when (val threads = ui.threads) {
                Fetched.Loading -> CircularProgressIndicator(Modifier.align(Alignment.Center))
                Fetched.Offline -> EmptyState(Icons.Outlined.Archive, stringResource(R.string.archived_offline), action = stringResource(R.string.action_retry), onAction = vm::reload)
                is Fetched.Failed -> EmptyState(Icons.Outlined.Archive, threads.message.asString(), action = stringResource(R.string.action_retry), onAction = vm::reload)
                is Fetched.Loaded -> if (threads.value.isEmpty() && !ui.hasMore) {
                    EmptyState(Icons.Outlined.Archive, stringResource(R.string.archived_empty))
                } else {
                    LazyColumn(Modifier.fillMaxSize()) {
                        items(threads.value, key = { it.id }) { thread ->
                            ListItem(
                                headlineContent = { Text(thread.title, maxLines = 1, overflow = TextOverflow.Ellipsis) },
                                supportingContent = { Text(relativeTime(thread.lastActivityAt)) },
                                trailingContent = {
                                    TextButton(onClick = { vm.unarchive(thread) }, enabled = thread.id !in ui.restoring) { Text(stringResource(R.string.thread_unarchive)) }
                                },
                                modifier = Modifier.clickable { navigator.openThread(thread.id) },
                            )
                            HorizontalDivider()
                        }
                        if (ui.hasMore) {
                            item(key = "more") {
                                Box(Modifier.fillMaxWidth().padding(16.dp), contentAlignment = Alignment.Center) {
                                    if (ui.loadingMore) CircularProgressIndicator() else OutlinedButton(onClick = vm::loadMore) { Text(stringResource(R.string.load_more)) }
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}

// ----- native sessions -------------------------------------------------------------------------

data class ImportSessionUiState(
    /** The harnesses to choose from (NativeSessionHarnesses.choices; the selected one is always among them). */
    val harnesses: List<Harness>,
    /** The harness whose sessions are listed; `null` until the workspace is known, or while none can import. */
    val selected: String?,
    val sessions: Fetched<List<NativeSession>>,
    val importing: String?,
    /** `native/list` was refused because the harness cannot be used: 再確認 probes it again. */
    val harnessUnavailable: Boolean = false,
    /**
     * `native/list` was refused with `capabilityUnsupported`: the selected harness cannot list its
     * sessions, so listing it again is not offered (the answer would be the same).
     */
    val listingUnsupported: Boolean = false,
    /** The harnesses the server answered `capabilityUnsupported` for on this screen (NativeSessionHarnesses). */
    val unable: Set<String> = emptySet(),
    /** The import waits for its harness (the server answered `harnessUnavailable`). */
    val waiting: HarnessWait? = null,
    val probing: Set<String> = emptySet(),
) {
    /** Some offered harness is not the one listed: the chips are shown (also when only one is offered and nothing is selected). */
    val canSwitch: Boolean get() = harnesses.any { it.id != selected }
}

/**
 * Continue a conversation started in the PC's terminal (design.md §1 ネイティブセッションの取り込み):
 * the harness's sessions in this project's folder (`native/list`), imported as a thread
 * (`native/import`; an already imported one opens its thread).
 *
 * The harness listed first is [requestedHarnessId] (`/resume` passes its thread's), else the
 * project's default harness, else the first that can list its sessions
 * (NativeSessionHarnesses.preselect). The user can switch at any time: a listing that fails
 * (the harness cannot be used, the server's error) shows why in place of the list, and the
 * other harnesses stay selectable.
 *
 * The screen picks a harness again, on the server's explicit answers only, when the one it lists
 * cannot be: when the server answers `capabilityUnsupported` (the harness's capabilities were
 * unknown while it was unavailable; the server probed it and it cannot list its sessions), it
 * lists the next harness [NativeSessionHarnesses.preselect] gives and says why; when none could
 * list, it lists the first one that `harness/updated` makes able to.
 */
class ImportSessionViewModel(
    private val projectId: String,
    private val requestedHarnessId: String?,
    private val projects: ProjectRepository,
    private val workspace: WorkspaceRepository,
    private val messages: UserMessages,
    outbox: StateFlow<List<OutboxEntry>>,
    harnessRepository: HarnessRepository,
    policy: AppPolicy,
) : ViewModel() {
    private val _state = MutableStateFlow(ImportSessionUiState(emptyList(), null, Fetched.Loading, null))
    private val refresher = HarnessRefresher(harnessRepository, messages)

    /** The listing in progress (replaced when the user switches harness or lists again). */
    private var listing: Job? = null

    /** The `clientRequestId` of the `native/import` being awaited. */
    private val importingId = MutableStateFlow<String?>(null)

    val state: StateFlow<ImportSessionUiState> = combine(_state, importingId, outbox, harnessRepository.refreshing, workspace.workspace) { s, id, out, probing, ws ->
        val waiting = id?.let { crid -> out.firstOrNull { it.clientRequestId == crid } }?.let { HarnessWait.of(it, ws.harnesses) }
        val choices = NativeSessionHarnesses.choices(ws.harnesses, s.selected, requestedHarnessId, s.unable)
        s.copy(harnesses = choices, waiting = waiting, probing = probing)
    }.stateIn(viewModelScope, SharingStarted.WhileSubscribed(policy.uiStopTimeoutMs), _state.value)
    private val opened = Channel<String>(Channel.BUFFERED)

    /** Threads to open (imported or already imported). */
    val openThread: Flow<String> = opened.receiveAsFlow()

    init {
        // The workspace lists the harnesses once the store holds the server's data (at once,
        // unless the app has never synced). Both flows only say that something changed:
        // pickHarness reads their current values, so it never acts twice on one answer.
        viewModelScope.launch { merge(_state, workspace.workspace).collect { pickHarness() } }
    }

    /**
     * Selects the harness to list while none that can list is selected: nothing yet (the first
     * one, or the first that `harness/updated` makes able to list), or the selected one was
     * refused with `capabilityUnsupported` (the next one, and the user is told why; a refused
     * harness is in `unable`, which preselect never picks). When no other harness can list, the
     * refusal stays shown in place.
     */
    private fun pickHarness() {
        val ws = workspace.workspace.value
        val s = _state.value
        val current = s.selected
        if (!ws.synced || (current != null && !s.listingUnsupported)) return
        val defaultHarness = ws.projects.firstOrNull { it.id == projectId }?.defaults?.harnessId
        val next = NativeSessionHarnesses.preselect(ws.harnesses, requestedHarnessId, defaultHarness, s.unable)
        when {
            next != null -> {
                if (current != null) {
                    messages.show(UiText.of(R.string.import_harness_unsupported_switched, harnessName(ws.harnesses, current), harnessName(ws.harnesses, next)))
                }
                select(next)
            }
            current == null && s.sessions == Fetched.Loading -> _state.update { it.copy(sessions = Fetched.Loaded(emptyList())) }
            else -> Unit
        }
    }

    private fun harnessName(harnesses: List<Harness>, harnessId: String): String = harnesses.firstOrNull { it.id == harnessId }?.displayName ?: harnessId

    /** Lists [harnessId]'s sessions (a listing of the previous harness still running is dropped). */
    fun select(harnessId: String) {
        _state.update { it.copy(selected = harnessId) }
        reload()
    }

    fun reload() {
        val harnessId = _state.value.selected ?: return
        list(harnessId, probeFirst = false)
    }

    /** 再確認 after `native/list` said the harness cannot be used: probe it, then list again. */
    fun refreshHarness() {
        val harnessId = _state.value.selected ?: return
        list(harnessId, probeFirst = true)
    }

    private fun list(harnessId: String, probeFirst: Boolean) {
        listing?.cancel()
        _state.update { it.copy(sessions = Fetched.Loading, harnessUnavailable = false, listingUnsupported = false) }
        listing = viewModelScope.launch {
            if (probeFirst) refresher.refreshNow(harnessId)
            val result = fetchSessions(harnessId)
            _state.update { ui ->
                if (ui.selected != harnessId) return@update ui
                // The server's latest answer about the capability: refused, or it listed.
                val unable = when {
                    result.unsupported -> ui.unable + harnessId
                    result.sessions is Fetched.Loaded -> ui.unable - harnessId
                    else -> ui.unable
                }
                ui.copy(sessions = result.sessions, harnessUnavailable = result.harnessUnavailable, listingUnsupported = result.unsupported, unable = unable)
            }
        }
    }

    /** A `native/list` answer as the screen shows it. */
    private data class Listing(
        val sessions: Fetched<List<NativeSession>>,
        /** Refused with `harnessUnavailable`: 再確認 is offered. */
        val harnessUnavailable: Boolean = false,
        /** Refused with `capabilityUnsupported`: the harness cannot list its sessions. */
        val unsupported: Boolean = false,
    )

    /** `native/list` of [harnessId], newest first, or why it failed. */
    private suspend fun fetchSessions(harnessId: String): Listing = try {
        Listing(Fetched.Loaded(projects.nativeSessions(projectId, harnessId).sortedByDescending { it.updatedAt ?: 0L }))
    } catch (e: CancellationException) {
        throw e
    } catch (e: RpcException) {
        val name = harnessName(workspace.workspace.value.harnesses, harnessId)
        when (e.kind) {
            // Not definitive: the server probes it again; 再確認 asks it to now.
            ErrorKind.HarnessUnavailable -> Listing(Fetched.Failed(UiText.of(R.string.import_harness_unavailable, name, e.error.reason ?: e.error.message)), harnessUnavailable = true)
            // Definitive: listing it again gets the same answer (pickHarness moves on).
            ErrorKind.CapabilityUnsupported -> Listing(Fetched.Failed(UiText.of(R.string.import_harness_unsupported, name)), unsupported = true)
            else -> Listing(Fetched.Failed(UiText.of(R.string.error_server, e.error.message)))
        }
    } catch (e: NotConnectedException) {
        Listing(Fetched.Offline)
    } catch (e: Exception) {
        Listing(Fetched.Failed(requestFailed(e)))
    }

    /** 再確認 of the harness an import waits for. */
    fun refreshWaiting(harnessId: String) = refresher.refresh(viewModelScope, harnessId)

    /** Withdraws an import that waits for its harness (it is never sent again). */
    fun discardImport() {
        val crid = importingId.value ?: return
        viewModelScope.launch {
            try {
                messages.show(ResultMessages.discarded(workspace.discard(crid)))
            } catch (e: CancellationException) {
                throw e
            } catch (e: Exception) {
                messages.show(UiText.of(R.string.error_local_store, e.message ?: e.javaClass.simpleName))
            }
        }
    }

    fun import(session: NativeSession) {
        session.importedThreadId?.let {
            viewModelScope.launch { opened.send(it) }
            return
        }
        val harnessId = _state.value.selected ?: return
        if (_state.value.importing != null) return
        _state.update { it.copy(importing = session.nativeSessionId) }
        viewModelScope.launch {
            try {
                val pending = projects.importSession(projectId, harnessId, session.nativeSessionId)
                importingId.value = pending.clientRequestId
                opened.send(pending.await().thread.id)
            } catch (e: CancellationException) {
                throw e
            } catch (e: RpcException) {
                // The shell reports the definitive failure of the request.
            } catch (e: OutboxClearedException) {
                // Withdrawn by the user (discardImport says so), or unpaired meanwhile.
            } catch (e: Exception) {
                messages.show(UiText.of(R.string.error_local_store, e.message ?: e.javaClass.simpleName))
            } finally {
                importingId.value = null
                _state.update { it.copy(importing = null) }
            }
        }
    }
}

/** Test tag of a session row of the import screen (`native-session-<id>`). */
fun nativeSessionRowTag(nativeSessionId: String) = "native-session-$nativeSessionId"

@OptIn(ExperimentalMaterial3Api::class)
@Composable
fun ImportSessionScreen(vm: ImportSessionViewModel, navigator: AppNavigator) {
    val ui by vm.state.collectAsStateWithLifecycle()
    LaunchedEffect(vm) { vm.openThread.collect { navigator.sessionImported(it) } }
    Scaffold(
        topBar = {
            TopAppBar(
                title = { Text(stringResource(R.string.import_session)) },
                navigationIcon = { IconButton(onClick = navigator::back) { Icon(Icons.AutoMirrored.Outlined.ArrowBack, contentDescription = stringResource(R.string.back)) } },
            )
        },
    ) { padding ->
        Column(Modifier.fillMaxSize().padding(padding)) {
            Text(stringResource(R.string.import_session_body), style = MaterialTheme.typography.bodyMedium, modifier = Modifier.padding(16.dp))
            ui.waiting?.let { wait ->
                HarnessWaitNotice(
                    wait = wait,
                    probing = wait.harnessId in ui.probing,
                    onRefresh = { vm.refreshWaiting(wait.harnessId) },
                    onDiscard = vm::discardImport,
                    modifier = Modifier.padding(horizontal = 16.dp, vertical = 4.dp),
                )
            }
            if (ui.canSwitch) {
                Row(Modifier.horizontalScroll(rememberScrollState()).padding(horizontal = 16.dp), horizontalArrangement = Arrangement.spacedBy(8.dp)) {
                    ui.harnesses.forEach { h -> FilterChip(selected = ui.selected == h.id, onClick = { vm.select(h.id) }, label = { Text(h.displayName) }) }
                }
            }
            Box(Modifier.fillMaxSize()) {
                when (val sessions = ui.sessions) {
                    Fetched.Loading -> CircularProgressIndicator(Modifier.align(Alignment.Center))
                    Fetched.Offline -> EmptyState(Icons.Outlined.History, stringResource(R.string.import_offline), action = stringResource(R.string.action_retry), onAction = vm::reload)
                    is Fetched.Failed -> when {
                        ui.harnessUnavailable -> EmptyState(Icons.Outlined.History, sessions.message.asString(), action = stringResource(R.string.harness_refresh), onAction = vm::refreshHarness)
                        // The same answer every time: no retry; the chips (when another harness can list) switch.
                        ui.listingUnsupported -> EmptyState(Icons.Outlined.History, sessions.message.asString())
                        else -> EmptyState(Icons.Outlined.History, sessions.message.asString(), action = stringResource(R.string.action_retry), onAction = vm::reload)
                    }
                    is Fetched.Loaded -> if (ui.selected == null) {
                        EmptyState(Icons.Outlined.History, stringResource(R.string.import_no_harness))
                    } else if (sessions.value.isEmpty()) {
                        EmptyState(Icons.Outlined.History, stringResource(R.string.import_empty))
                    } else {
                        LazyColumn(Modifier.fillMaxSize()) {
                            items(sessions.value, key = { it.nativeSessionId }) { session ->
                                ListItem(
                                    headlineContent = { Text(session.title ?: session.nativeSessionId, maxLines = 2, overflow = TextOverflow.Ellipsis) },
                                    supportingContent = {
                                        Text(listOfNotNull(session.updatedAt?.let { relativeTime(it) }, session.cwd).joinToString(" · "), maxLines = 1, overflow = TextOverflow.Ellipsis)
                                    },
                                    trailingContent = {
                                        when {
                                            ui.importing == session.nativeSessionId -> CircularProgressIndicator()
                                            session.importedThreadId != null -> Text(stringResource(R.string.import_done), style = MaterialTheme.typography.labelSmall)
                                            else -> Unit
                                        }
                                    },
                                    modifier = Modifier.clickable(enabled = ui.importing == null) { vm.import(session) }.testTag(nativeSessionRowTag(session.nativeSessionId)),
                                )
                                HorizontalDivider()
                            }
                        }
                    }
                }
            }
        }
    }
}
