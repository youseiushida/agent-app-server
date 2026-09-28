package dev.aas.android.ui.projects

import androidx.compose.foundation.ExperimentalFoundationApi
import androidx.compose.foundation.combinedClickable
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.PaddingValues
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.foundation.lazy.items
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.outlined.Add
import androidx.compose.material.icons.outlined.Close
import androidx.compose.material.icons.outlined.Search
import androidx.compose.material3.Card
import androidx.compose.material3.DropdownMenu
import androidx.compose.material3.DropdownMenuItem
import androidx.compose.material3.ExperimentalMaterial3Api
import androidx.compose.material3.HorizontalDivider
import androidx.compose.material3.Icon
import androidx.compose.material3.IconButton
import androidx.compose.material3.LinearProgressIndicator
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.OutlinedTextField
import androidx.compose.material3.Scaffold
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.material3.TopAppBar
import androidx.compose.runtime.Composable
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.saveable.rememberSaveable
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.res.stringResource
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.unit.dp
import androidx.lifecycle.ViewModel
import androidx.lifecycle.compose.collectAsStateWithLifecycle
import androidx.lifecycle.viewModelScope
import androidx.navigation.NavGraphBuilder
import androidx.navigation.compose.composable
import androidx.navigation.toRoute
import dev.aas.android.R
import dev.aas.android.data.ProjectRepository
import dev.aas.android.data.WorkspaceRepository
import dev.aas.android.domain.ProjectLists
import dev.aas.android.domain.ProjectRow
import dev.aas.android.domain.ProjectSort
import dev.aas.android.domain.ThreadActivity
import dev.aas.android.protocol.Operation
import dev.aas.android.protocol.OperationStatus
import dev.aas.android.protocol.ProjectId
import dev.aas.android.settings.AppSettings
import dev.aas.android.settings.SettingsRepository
import dev.aas.android.sync.SyncStatus
import dev.aas.android.ui.common.UiText
import dev.aas.android.ui.common.UserMessages
import dev.aas.android.ui.common.aasViewModel
import dev.aas.android.ui.components.ConfirmDialog
import dev.aas.android.ui.components.EmptyState
import dev.aas.android.ui.components.LabeledExtendedFab
import dev.aas.android.ui.components.TextInputDialog
import dev.aas.android.ui.components.ThreadActivityChip
import dev.aas.android.ui.components.relativeTime
import dev.aas.android.ui.icons.CloudDownload
import dev.aas.android.ui.icons.Folder
import dev.aas.android.ui.icons.Sort
import dev.aas.android.ui.navigation.AppNavigator
import dev.aas.android.ui.navigation.ArchivedThreadsRoute
import dev.aas.android.ui.navigation.ImportSessionRoute
import dev.aas.android.ui.navigation.ProjectThreadsRoute
import dev.aas.android.ui.navigation.ProjectsRoute
import kotlinx.coroutines.CancellationException
import kotlinx.coroutines.flow.Flow
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.SharingStarted
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.flow.combine
import kotlinx.coroutines.flow.stateIn
import kotlinx.coroutines.launch

/*
 * プロジェクト tab: the project list, a project's threads, its archived threads and importing
 * native sessions.
 */

fun NavGraphBuilder.projectsDestinations(navigator: AppNavigator) {
    composable<ProjectsRoute> {
        val vm = aasViewModel { c, _ -> ProjectsViewModel(c.workspaceRepository, c.projectRepository, c.settings, c.settings.settings, c.engine.status, c.userMessages, c.policy.uiStopTimeoutMs) }
        ProjectsScreen(vm, navigator)
    }
    composable<ProjectThreadsRoute> { entry ->
        val route = entry.toRoute<ProjectThreadsRoute>()
        val vm = aasViewModel(key = route.projectId) { c, _ ->
            ProjectThreadsViewModel(route.projectId, c.workspaceRepository, c.threadRepository, c.projectRepository, c.userMessages, c.policy.uiStopTimeoutMs)
        }
        ProjectThreadsScreen(vm, navigator)
    }
    composable<ArchivedThreadsRoute> { entry ->
        val route = entry.toRoute<ArchivedThreadsRoute>()
        val vm = aasViewModel(key = route.projectId) { c, _ -> ArchivedThreadsViewModel(route.projectId, c.projectRepository, c.threadRepository, c.userMessages) }
        ArchivedThreadsScreen(vm, navigator)
    }
    composable<ImportSessionRoute> { entry ->
        val route = entry.toRoute<ImportSessionRoute>()
        val vm = aasViewModel(key = route.projectId) { c, _ -> ImportSessionViewModel(route.projectId, c.projectRepository, c.workspaceRepository, c.userMessages, c.engine.outbox, c.harnessRepository, c.policy) }
        ImportSessionScreen(vm, navigator)
    }
}

data class ProjectsUiState(
    /** `null` until the first sync delivered the workspace. */
    val rows: List<ProjectRow>?,
    /** Clones in progress (with their progress line and 取り消す). */
    val operations: List<Operation>,
    val query: String,
    val sort: ProjectSort,
    val online: Boolean,
)

class ProjectsViewModel(
    workspace: WorkspaceRepository,
    private val projects: ProjectRepository,
    private val settingsRepository: SettingsRepository,
    settings: Flow<AppSettings>,
    status: StateFlow<SyncStatus>,
    private val messages: UserMessages,
    stopTimeoutMs: Long,
) : ViewModel() {
    private val query = MutableStateFlow("")

    val state: StateFlow<ProjectsUiState> = combine(workspace.workspace, query, settings, status) { ws, q, s, st ->
        ProjectsUiState(
            rows = if (ws.synced) ProjectLists.projects(ws, s.projectSort, q) else null,
            operations = ws.operations.filter { it.status == OperationStatus.Running },
            query = q,
            sort = s.projectSort,
            online = st.isOnline,
        )
    }.stateIn(viewModelScope, SharingStarted.WhileSubscribed(stopTimeoutMs), ProjectsUiState(null, emptyList(), "", ProjectSort.Recent, status.value.isOnline))

    fun setQuery(value: String) {
        query.value = value
    }

    fun setSort(sort: ProjectSort) = launch { settingsRepository.setProjectSort(sort) }

    fun rename(projectId: ProjectId, name: String) {
        val trimmed = name.trim()
        if (trimmed.isEmpty()) return
        launch { projects.rename(projectId, trimmed) }
    }

    fun archive(projectId: ProjectId) = launch {
        projects.archive(projectId, archived = true)
        messages.show(UiText.of(R.string.project_archived))
    }

    fun remove(projectId: ProjectId) = launch { projects.remove(projectId) }

    fun cancelOperation(operation: Operation) = launch { projects.cancelOperation(operation.id) }

    private fun launch(block: suspend () -> Unit) {
        viewModelScope.launch {
            try {
                block()
            } catch (e: CancellationException) {
                throw e
            } catch (e: Exception) {
                messages.show(UiText.of(R.string.error_local_store, e.message ?: e.javaClass.simpleName))
            }
        }
    }
}

@OptIn(ExperimentalMaterial3Api::class)
@Composable
fun ProjectsScreen(vm: ProjectsViewModel, navigator: AppNavigator) {
    val ui by vm.state.collectAsStateWithLifecycle()
    var searching by rememberSaveable { mutableStateOf(false) }
    var sortMenu by remember { mutableStateOf(false) }
    var renaming by rememberSaveable { mutableStateOf<String?>(null) }
    var removing by rememberSaveable { mutableStateOf<String?>(null) }
    Scaffold(
        topBar = {
            TopAppBar(
                title = { Text(stringResource(R.string.tab_projects)) },
                actions = {
                    IconButton(onClick = {
                        searching = !searching
                        if (!searching) vm.setQuery("")
                    }) { Icon(if (searching) Icons.Outlined.Close else Icons.Outlined.Search, contentDescription = stringResource(R.string.projects_search)) }
                    Box {
                        IconButton(onClick = { sortMenu = true }) { Icon(Icons.AutoMirrored.Outlined.Sort, contentDescription = stringResource(R.string.projects_sort)) }
                        DropdownMenu(expanded = sortMenu, onDismissRequest = { sortMenu = false }) {
                            DropdownMenuItem(text = { Text(stringResource(R.string.projects_sort_recent)) }, onClick = { sortMenu = false; vm.setSort(ProjectSort.Recent) })
                            DropdownMenuItem(text = { Text(stringResource(R.string.projects_sort_name)) }, onClick = { sortMenu = false; vm.setSort(ProjectSort.Name) })
                        }
                    }
                },
            )
        },
        floatingActionButton = {
            LabeledExtendedFab(stringResource(R.string.project_new), Icons.Outlined.Add, onClick = navigator::newProject)
        },
    ) { padding ->
        Column(Modifier.fillMaxSize().padding(padding)) {
            if (searching) {
                OutlinedTextField(
                    value = ui.query,
                    onValueChange = vm::setQuery,
                    placeholder = { Text(stringResource(R.string.projects_search_hint)) },
                    singleLine = true,
                    modifier = Modifier.fillMaxWidth().padding(horizontal = 16.dp, vertical = 4.dp),
                )
            }
            val rows = ui.rows
            when {
                rows == null -> EmptyState(Icons.Outlined.Folder, stringResource(R.string.projects_not_synced), body = stringResource(R.string.projects_not_synced_body))
                rows.isEmpty() && ui.operations.isEmpty() && ui.query.isNotEmpty() -> EmptyState(Icons.Outlined.Search, stringResource(R.string.projects_no_match))
                rows.isEmpty() && ui.operations.isEmpty() -> EmptyState(
                    Icons.Outlined.Folder,
                    stringResource(R.string.projects_empty),
                    body = stringResource(R.string.projects_empty_body),
                    action = stringResource(R.string.project_new),
                    onAction = navigator::newProject,
                )
                else -> LazyColumn(Modifier.fillMaxSize(), contentPadding = PaddingValues(bottom = FAB_CLEARANCE)) {
                    items(ui.operations, key = { "op-" + it.id }) { op -> OperationCard(op, onCancel = { vm.cancelOperation(op) }) }
                    items(rows, key = { it.project.id }) { row ->
                        ProjectRowView(
                            row,
                            onClick = { navigator.openProject(row.project.id) },
                            onRename = { renaming = row.project.id },
                            onArchive = { vm.archive(row.project.id) },
                            onRemove = { removing = row.project.id },
                        )
                        HorizontalDivider()
                    }
                }
            }
        }
    }
    val renameRow = renaming?.let { id -> ui.rows?.firstOrNull { it.project.id == id } }
    if (renameRow != null) {
        TextInputDialog(
            title = stringResource(R.string.project_rename_title),
            initial = renameRow.project.name,
            confirm = stringResource(R.string.save),
            onConfirm = {
                renaming = null
                vm.rename(renameRow.project.id, it)
            },
            onDismiss = { renaming = null },
        )
    }
    val removeRow = removing?.let { id -> ui.rows?.firstOrNull { it.project.id == id } }
    if (removeRow != null) {
        ConfirmDialog(
            title = stringResource(R.string.project_remove_title, removeRow.project.name),
            text = stringResource(R.string.project_remove_body, removeRow.project.path),
            confirm = stringResource(R.string.project_remove),
            onConfirm = {
                removing = null
                vm.remove(removeRow.project.id)
            },
            onDismiss = { removing = null },
        )
    }
}

/** A running clone (UX §8.2): git's latest progress line as it is, and 取り消す (`operation/cancel`). */
@Composable
fun OperationCard(operation: Operation, onCancel: () -> Unit, modifier: Modifier = Modifier) {
    Card(modifier.fillMaxWidth().padding(horizontal = 16.dp, vertical = 8.dp)) {
        Column(Modifier.padding(16.dp)) {
            Row(verticalAlignment = Alignment.CenterVertically) {
                Icon(Icons.Outlined.CloudDownload, null)
                Spacer(Modifier.width(12.dp))
                Column(Modifier.weight(1f)) {
                    Text(stringResource(R.string.operation_clone), style = MaterialTheme.typography.titleSmall)
                    operation.message?.let { Text(it, style = MaterialTheme.typography.bodySmall, maxLines = 2, overflow = TextOverflow.Ellipsis) }
                }
                if (operation.status == OperationStatus.Running) TextButton(onClick = onCancel) { Text(stringResource(R.string.operation_cancel)) }
            }
            Spacer(Modifier.height(8.dp))
            LinearProgressIndicator(Modifier.fillMaxWidth())
            Spacer(Modifier.height(4.dp))
            Text(
                operation.progress ?: stringResource(R.string.operation_waiting_progress),
                style = MaterialTheme.typography.labelSmall,
                color = MaterialTheme.colorScheme.onSurfaceVariant,
                maxLines = 2,
                overflow = TextOverflow.Ellipsis,
            )
        }
    }
}

@OptIn(ExperimentalFoundationApi::class)
@Composable
private fun ProjectRowView(row: ProjectRow, onClick: () -> Unit, onRename: () -> Unit, onArchive: () -> Unit, onRemove: () -> Unit) {
    var menu by remember { mutableStateOf(false) }
    Box {
        Row(
            Modifier.fillMaxWidth().combinedClickable(onClick = onClick, onLongClick = { menu = true }).padding(horizontal = 16.dp, vertical = 12.dp),
            verticalAlignment = Alignment.CenterVertically,
        ) {
            Icon(Icons.Outlined.Folder, contentDescription = null, tint = MaterialTheme.colorScheme.primary)
            Spacer(Modifier.width(16.dp))
            Column(Modifier.weight(1f)) {
                Row(verticalAlignment = Alignment.CenterVertically) {
                    Text(row.project.name, style = MaterialTheme.typography.titleMedium, maxLines = 1, overflow = TextOverflow.Ellipsis, modifier = Modifier.weight(1f, fill = false))
                    if (row.activity != ThreadActivity.Idle) {
                        Spacer(Modifier.width(8.dp))
                        ThreadActivityChip(row.activity)
                    }
                }
                Text(row.project.path, style = MaterialTheme.typography.bodySmall, color = MaterialTheme.colorScheme.onSurfaceVariant, maxLines = 1, overflow = TextOverflow.Ellipsis)
                val parts = buildList {
                    add(stringResource(R.string.project_threads, row.threads))
                    if (row.approvals > 0) add(stringResource(R.string.project_approvals, row.approvals))
                    if (row.questions > 0) add(stringResource(R.string.project_questions, row.questions))
                    if (row.running > 0) add(stringResource(R.string.project_running, row.running))
                    if (row.errors > 0) add(stringResource(R.string.project_errors, row.errors))
                    if (row.unread > 0) add(stringResource(R.string.project_unread, row.unread))
                    if (row.queued > 0) add(stringResource(R.string.project_queued, row.queued))
                    row.project.git.branch?.let { add(it) }
                }
                Text(parts.joinToString(" · "), style = MaterialTheme.typography.labelSmall, color = MaterialTheme.colorScheme.onSurfaceVariant, maxLines = 2, overflow = TextOverflow.Ellipsis)
            }
            if (row.lastActivityAt > 0) Text(relativeTime(row.lastActivityAt), style = MaterialTheme.typography.labelSmall, color = MaterialTheme.colorScheme.onSurfaceVariant)
        }
        DropdownMenu(expanded = menu, onDismissRequest = { menu = false }) {
            DropdownMenuItem(text = { Text(stringResource(R.string.project_rename)) }, onClick = { menu = false; onRename() })
            DropdownMenuItem(text = { Text(stringResource(R.string.project_archive)) }, onClick = { menu = false; onArchive() })
            DropdownMenuItem(text = { Text(stringResource(R.string.project_remove)) }, onClick = { menu = false; onRemove() })
        }
    }
}

/** Room below the list for the floating button. */
private val FAB_CLEARANCE = 88.dp
