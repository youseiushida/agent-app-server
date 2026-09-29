package dev.aas.android.ui.projects

import androidx.compose.foundation.ExperimentalFoundationApi
import androidx.compose.foundation.background
import androidx.compose.foundation.combinedClickable
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.PaddingValues
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.foundation.lazy.items
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.automirrored.outlined.ArrowBack
import androidx.compose.material.icons.outlined.Add
import androidx.compose.material.icons.outlined.MoreVert
import androidx.compose.material3.DropdownMenu
import androidx.compose.material3.DropdownMenuItem
import androidx.compose.material3.ExperimentalMaterial3Api
import androidx.compose.material3.HorizontalDivider
import androidx.compose.material3.Icon
import androidx.compose.material3.IconButton
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Scaffold
import androidx.compose.material3.Surface
import androidx.compose.material3.SwipeToDismissBox
import androidx.compose.material3.SwipeToDismissBoxValue
import androidx.compose.material3.Text
import androidx.compose.material3.TopAppBar
import androidx.compose.material3.rememberSwipeToDismissBoxState
import androidx.compose.runtime.Composable
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.rememberCoroutineScope
import androidx.compose.runtime.saveable.rememberSaveable
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.platform.testTag
import androidx.compose.ui.res.stringResource
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.unit.dp
import androidx.lifecycle.ViewModel
import androidx.lifecycle.compose.collectAsStateWithLifecycle
import androidx.lifecycle.viewModelScope
import dev.aas.android.R
import dev.aas.android.data.ProjectRepository
import dev.aas.android.data.ThreadRepository
import dev.aas.android.data.WorkspaceRepository
import dev.aas.android.domain.NativeSessionHarnesses
import dev.aas.android.domain.ProjectLists
import dev.aas.android.domain.ResultMessages
import dev.aas.android.domain.ThreadActivity
import dev.aas.android.domain.ThreadRow
import dev.aas.android.protocol.Harness
import dev.aas.android.protocol.Project
import dev.aas.android.protocol.ThreadStatus
import dev.aas.android.ui.common.UiText
import dev.aas.android.ui.common.UserMessage
import dev.aas.android.ui.common.UserMessages
import dev.aas.android.ui.components.ConfirmDialog
import dev.aas.android.ui.components.EmptyState
import dev.aas.android.ui.components.LabeledExtendedFab
import dev.aas.android.ui.components.TextInputDialog
import dev.aas.android.ui.components.ThreadActivityChip
import dev.aas.android.ui.components.UnreadDot
import dev.aas.android.ui.components.relativeTime
import dev.aas.android.ui.icons.Archive
import dev.aas.android.ui.icons.ChatBubbleOutline
import dev.aas.android.ui.icons.MarkEmailRead
import dev.aas.android.ui.icons.MarkEmailUnread
import dev.aas.android.ui.icons.PushPin
import dev.aas.android.ui.navigation.AppNavigator
import dev.aas.android.ui.theme.statusColors
import dev.aas.android.ui.thread.RunningBackgroundNote
import kotlinx.coroutines.CancellationException
import kotlinx.coroutines.flow.SharingStarted
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.flow.map
import kotlinx.coroutines.flow.stateIn
import kotlinx.coroutines.launch

data class ProjectThreadsUiState(val project: Project?, val rows: List<ThreadRow>, val harnesses: List<Harness>, val synced: Boolean) {
    /** Some harness can list its own sessions (native/list). */
    val canImport: Boolean get() = NativeSessionHarnesses.canImport(harnesses)
}

/**
 * A project's threads (UX §5.2, §8.2): pinned first, then by activity; swipe right to toggle
 * read, left to archive (running threads ask first: the daemon stops them), long press for pin,
 * rename, read state and archive.
 */
class ProjectThreadsViewModel(
    val projectId: String,
    workspace: WorkspaceRepository,
    private val threads: ThreadRepository,
    private val projects: ProjectRepository,
    private val messages: UserMessages,
    stopTimeoutMs: Long,
) : ViewModel() {
    private val workspaceRepository = workspace

    val state: StateFlow<ProjectThreadsUiState> = workspace.workspace.map { ws ->
        ProjectThreadsUiState(ws.projects.firstOrNull { it.id == projectId }, ProjectLists.threads(ws, projectId), ws.harnesses, ws.synced)
    }.stateIn(viewModelScope, SharingStarted.WhileSubscribed(stopTimeoutMs), ProjectThreadsUiState(null, emptyList(), emptyList(), false))

    fun togglePin(row: ThreadRow) = launch {
        threads.setPinned(row.id, !row.pinned)
        messages.show(UiText.of(if (row.pinned) R.string.thread_unpinned else R.string.thread_pinned))
    }

    fun rename(row: ThreadRow, title: String) {
        val trimmed = title.trim()
        if (trimmed.isEmpty()) return
        launch {
            val pending = threads.rename(row.id, trimmed)
            ResultMessages.awaitNativeRename(pending)?.let { messages.show(it) }
        }
    }

    /** Archives; the snackbar offers to undo (`thread/archive` with `archived: false`). */
    fun archive(row: ThreadRow) = launch {
        threads.archive(row.id, archived = true)
        messages.show(UserMessage(UiText.of(R.string.thread_archived, row.title), UiText.of(R.string.undo)) { unarchive(row.id) })
    }

    /**
     * The snackbar's 元に戻す. It runs where the shell runs snackbar actions (the app's scope):
     * the snackbar may be tapped after this screen was left, when this view model's scope is
     * gone.
     */
    private suspend fun unarchive(threadId: String) = reportingFailure { threads.archive(threadId, archived = false) }

    /** Unread ↔ read (per device). */
    fun toggleRead(row: ThreadRow) = launch {
        if (row.unread) workspaceRepository.markViewed(row.id) else workspaceRepository.markUnread(row.id)
    }

    fun renameProject(name: String) {
        val trimmed = name.trim()
        if (trimmed.isEmpty()) return
        launch { projects.rename(projectId, trimmed) }
    }

    private fun launch(block: suspend () -> Unit) {
        viewModelScope.launch { reportingFailure(block) }
    }

    /** Runs a local step (an outbox commit, the read state); a failure of the device's store is shown. */
    private suspend fun reportingFailure(block: suspend () -> Unit) {
        try {
            block()
        } catch (e: CancellationException) {
            throw e
        } catch (e: Exception) {
            messages.show(UiText.of(R.string.error_local_store, e.message ?: e.javaClass.simpleName))
        }
    }
}

/** Test tag of a thread row (`thread-row-<id>`). */
fun threadRowTag(threadId: String) = "thread-row-$threadId"

@OptIn(ExperimentalMaterial3Api::class)
@Composable
fun ProjectThreadsScreen(vm: ProjectThreadsViewModel, navigator: AppNavigator) {
    val ui by vm.state.collectAsStateWithLifecycle()
    var menu by remember { mutableStateOf(false) }
    var renamingProject by rememberSaveable { mutableStateOf(false) }
    var renaming by rememberSaveable { mutableStateOf<String?>(null) }
    var confirmArchive by rememberSaveable { mutableStateOf<String?>(null) }
    val project = ui.project
    Scaffold(
        topBar = {
            TopAppBar(
                title = {
                    Column {
                        Text(project?.name ?: stringResource(R.string.project_unknown), maxLines = 1, overflow = TextOverflow.Ellipsis)
                        project?.let {
                            Text(
                                listOfNotNull(it.path, it.git.branch).joinToString(" · "),
                                style = MaterialTheme.typography.labelSmall,
                                color = MaterialTheme.colorScheme.onSurfaceVariant,
                                maxLines = 1,
                                overflow = TextOverflow.Ellipsis,
                            )
                        }
                    }
                },
                navigationIcon = { IconButton(onClick = navigator::back) { Icon(Icons.AutoMirrored.Outlined.ArrowBack, contentDescription = stringResource(R.string.back)) } },
                actions = {
                    Box {
                        IconButton(onClick = { menu = true }) { Icon(Icons.Outlined.MoreVert, contentDescription = stringResource(R.string.more_actions)) }
                        DropdownMenu(expanded = menu, onDismissRequest = { menu = false }) {
                            DropdownMenuItem(text = { Text(stringResource(R.string.thread_new)) }, onClick = { menu = false; navigator.newThread(vm.projectId) })
                            DropdownMenuItem(text = { Text(stringResource(R.string.threads_archived)) }, onClick = { menu = false; navigator.openArchived(vm.projectId) })
                            if (ui.canImport) DropdownMenuItem(text = { Text(stringResource(R.string.import_session)) }, onClick = { menu = false; navigator.importSession(vm.projectId) })
                            if (project != null) DropdownMenuItem(text = { Text(stringResource(R.string.project_rename)) }, onClick = { menu = false; renamingProject = true })
                        }
                    }
                },
            )
        },
        floatingActionButton = {
            LabeledExtendedFab(stringResource(R.string.thread_new), Icons.Outlined.Add, onClick = { navigator.newThread(vm.projectId) })
        },
    ) { padding ->
        if (ui.rows.isEmpty()) {
            EmptyState(
                Icons.Outlined.ChatBubbleOutline,
                stringResource(R.string.threads_empty),
                Modifier.padding(padding),
                body = stringResource(R.string.threads_empty_body),
                action = stringResource(R.string.thread_new),
                onAction = { navigator.newThread(vm.projectId) },
            )
        } else {
            LazyColumn(Modifier.fillMaxSize().padding(padding), contentPadding = PaddingValues(bottom = FAB_CLEARANCE)) {
                items(ui.rows, key = { it.id }) { row ->
                    SwipeableThreadRow(
                        row = row,
                        onClick = { navigator.openThread(row.id) },
                        onToggleRead = { vm.toggleRead(row) },
                        onArchive = { if (row.thread.status != ThreadStatus.Idle) confirmArchive = row.id else vm.archive(row) },
                        onTogglePin = { vm.togglePin(row) },
                        onRename = { renaming = row.id },
                    )
                    HorizontalDivider()
                }
            }
        }
    }
    val renameRow = renaming?.let { id -> ui.rows.firstOrNull { it.id == id } }
    if (renameRow != null) {
        TextInputDialog(
            title = stringResource(R.string.thread_rename_title),
            initial = renameRow.title,
            confirm = stringResource(R.string.save),
            label = stringResource(R.string.thread_rename_label),
            onConfirm = {
                renaming = null
                vm.rename(renameRow, it)
            },
            onDismiss = { renaming = null },
        )
    }
    val archiveRow = confirmArchive?.let { id -> ui.rows.firstOrNull { it.id == id } }
    if (archiveRow != null) {
        ConfirmDialog(
            title = stringResource(R.string.archive_running_title),
            text = stringResource(R.string.archive_running_body),
            confirm = stringResource(R.string.archive_running_confirm),
            onConfirm = {
                confirmArchive = null
                vm.archive(archiveRow)
            },
            onDismiss = { confirmArchive = null },
            // Its background work stops with the process (the list knows the count, not the titles).
            extra = archiveRow.thread.background.running.takeIf { it > 0 }?.let { count -> { RunningBackgroundNote(count, emptyList()) } },
        )
    }
    if (renamingProject && project != null) {
        TextInputDialog(
            title = stringResource(R.string.project_rename_title),
            initial = project.name,
            confirm = stringResource(R.string.save),
            onConfirm = {
                renamingProject = false
                vm.renameProject(it)
            },
            onDismiss = { renamingProject = false },
        )
    }
}

@OptIn(ExperimentalMaterial3Api::class)
@Composable
private fun SwipeableThreadRow(
    row: ThreadRow,
    onClick: () -> Unit,
    onToggleRead: () -> Unit,
    onArchive: () -> Unit,
    onTogglePin: () -> Unit,
    onRename: () -> Unit,
) {
    val state = rememberSwipeToDismissBoxState()
    val scope = rememberCoroutineScope()
    SwipeToDismissBox(
        state = state,
        backgroundContent = { SwipeBackground(state.dismissDirection, row.unread) },
        onDismiss = { direction ->
            when (direction) {
                SwipeToDismissBoxValue.StartToEnd -> onToggleRead()
                SwipeToDismissBoxValue.EndToStart -> onArchive()
                SwipeToDismissBoxValue.Settled -> Unit
            }
            // The row stays (read state) or leaves with the next workspace update (archive).
            scope.launch { state.reset() }
        },
    ) {
        Surface { ThreadRowView(row, onClick, onToggleRead, onArchive, onTogglePin, onRename) }
    }
}

@Composable
private fun SwipeBackground(direction: SwipeToDismissBoxValue, unread: Boolean) {
    val (color, icon, label, alignment) = when (direction) {
        SwipeToDismissBoxValue.StartToEnd -> Quad(MaterialTheme.statusColors.unread, if (unread) Icons.Outlined.MarkEmailRead else Icons.Outlined.MarkEmailUnread, stringResource(if (unread) R.string.mark_read else R.string.mark_unread), Alignment.CenterStart)
        SwipeToDismissBoxValue.EndToStart -> Quad(MaterialTheme.statusColors.needsApproval, Icons.Outlined.Archive, stringResource(R.string.thread_menu_archive), Alignment.CenterEnd)
        SwipeToDismissBoxValue.Settled -> Quad(Color.Transparent, null, "", Alignment.Center)
    }
    Box(Modifier.fillMaxSize().background(color.copy(alpha = SWIPE_ALPHA)).padding(horizontal = 24.dp), contentAlignment = alignment) {
        if (icon != null) {
            Row(verticalAlignment = Alignment.CenterVertically) {
                Icon(icon, contentDescription = null)
                Spacer(Modifier.width(8.dp))
                Text(label, style = MaterialTheme.typography.labelLarge)
            }
        }
    }
}

private data class Quad<A, B, C, D>(val a: A, val b: B, val c: C, val d: D)

/**
 * A thread in a list: unread dot, title, status chip, harness, pending approvals and questions,
 * queued messages, pin, last activity. Long press opens its menu.
 */
@OptIn(ExperimentalFoundationApi::class)
@Composable
fun ThreadRowView(
    row: ThreadRow,
    onClick: () -> Unit,
    onToggleRead: (() -> Unit)? = null,
    onArchive: (() -> Unit)? = null,
    onTogglePin: (() -> Unit)? = null,
    onRename: (() -> Unit)? = null,
) {
    var menu by remember { mutableStateOf(false) }
    val hasMenu = onToggleRead != null || onArchive != null || onTogglePin != null || onRename != null
    Box {
        Row(
            Modifier.fillMaxWidth()
                .combinedClickable(onClick = onClick, onLongClick = if (hasMenu) ({ menu = true }) else null)
                .padding(start = 12.dp, end = 16.dp, top = 12.dp, bottom = 12.dp)
                .testTag(threadRowTag(row.id)),
            verticalAlignment = Alignment.CenterVertically,
        ) {
            UnreadDot(row.unread)
            Spacer(Modifier.width(12.dp))
            Column(Modifier.weight(1f), verticalArrangement = Arrangement.spacedBy(2.dp)) {
                Row(verticalAlignment = Alignment.CenterVertically) {
                    if (row.pinned) {
                        Icon(Icons.Outlined.PushPin, contentDescription = stringResource(R.string.pinned), Modifier.size(14.dp), tint = MaterialTheme.colorScheme.onSurfaceVariant)
                        Spacer(Modifier.width(4.dp))
                    }
                    Text(
                        row.title,
                        style = MaterialTheme.typography.bodyLarge.copy(fontWeight = if (row.unread) FontWeight.SemiBold else FontWeight.Normal),
                        maxLines = 1,
                        overflow = TextOverflow.Ellipsis,
                    )
                }
                Row(verticalAlignment = Alignment.CenterVertically, horizontalArrangement = Arrangement.spacedBy(6.dp)) {
                    ThreadActivityChip(row.activity, backgroundRunning = row.thread.background.running)
                    if (row.approvals > 1 || (row.approvals > 0 && row.activity != ThreadActivity.NeedsApproval)) Badge(stringResource(R.string.thread_row_approvals, row.approvals), MaterialTheme.statusColors.needsApproval)
                    if (row.questions > 0 && (row.questions > 1 || row.activity != ThreadActivity.NeedsInput)) Badge(stringResource(R.string.thread_row_questions, row.questions), MaterialTheme.statusColors.needsInput)
                    if (row.queued > 0) Badge(stringResource(R.string.thread_row_queued, row.queued), MaterialTheme.colorScheme.onSurfaceVariant)
                    row.harnessName?.let { Text(it, style = MaterialTheme.typography.labelSmall, color = MaterialTheme.colorScheme.onSurfaceVariant, maxLines = 1) }
                }
            }
            Text(relativeTime(row.lastActivityAt), style = MaterialTheme.typography.labelSmall, color = MaterialTheme.colorScheme.onSurfaceVariant)
        }
        DropdownMenu(expanded = menu, onDismissRequest = { menu = false }) {
            onTogglePin?.let { DropdownMenuItem(text = { Text(stringResource(if (row.pinned) R.string.thread_menu_unpin else R.string.thread_menu_pin)) }, onClick = { menu = false; it() }) }
            onRename?.let { DropdownMenuItem(text = { Text(stringResource(R.string.thread_menu_rename)) }, onClick = { menu = false; it() }) }
            onToggleRead?.let { DropdownMenuItem(text = { Text(stringResource(if (row.unread) R.string.mark_read else R.string.mark_unread)) }, onClick = { menu = false; it() }) }
            onArchive?.let { DropdownMenuItem(text = { Text(stringResource(R.string.thread_menu_archive)) }, onClick = { menu = false; it() }) }
        }
    }
}

@Composable
private fun Badge(text: String, color: Color) {
    Surface(shape = RoundedCornerShape(50), color = color.copy(alpha = BADGE_ALPHA)) {
        Text(text, style = MaterialTheme.typography.labelSmall, color = color, modifier = Modifier.padding(horizontal = 6.dp, vertical = 1.dp), maxLines = 1)
    }
}

private val FAB_CLEARANCE = 88.dp
private const val SWIPE_ALPHA = 0.25f
private const val BADGE_ALPHA = 0.14f
