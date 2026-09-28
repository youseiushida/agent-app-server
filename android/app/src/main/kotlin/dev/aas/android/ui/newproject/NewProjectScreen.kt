package dev.aas.android.ui.newproject

import androidx.activity.compose.BackHandler
import androidx.compose.foundation.clickable
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.imePadding
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.foundation.lazy.items
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.selection.selectable
import androidx.compose.foundation.verticalScroll
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.automirrored.outlined.ArrowBack
import androidx.compose.material3.Button
import androidx.compose.material3.Card
import androidx.compose.material3.CircularProgressIndicator
import androidx.compose.material3.ExperimentalMaterial3Api
import androidx.compose.material3.HorizontalDivider
import androidx.compose.material3.Icon
import androidx.compose.material3.IconButton
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.OutlinedButton
import androidx.compose.material3.OutlinedTextField
import androidx.compose.material3.RadioButton
import androidx.compose.material3.Scaffold
import androidx.compose.material3.Surface
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.material3.TopAppBar
import androidx.compose.runtime.Composable
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.saveable.rememberSaveable
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.res.stringResource
import androidx.compose.ui.semantics.Role
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.unit.dp
import androidx.lifecycle.compose.collectAsStateWithLifecycle
import androidx.navigation.NavGraphBuilder
import androidx.navigation.compose.composable
import dev.aas.android.R
import dev.aas.android.domain.ServerPaths
import dev.aas.android.protocol.FsEntry
import dev.aas.android.protocol.OperationStatus
import dev.aas.android.ui.common.aasViewModel
import dev.aas.android.ui.common.asString
import dev.aas.android.ui.components.EmptyState
import dev.aas.android.ui.components.TextInputDialog
import dev.aas.android.ui.icons.ArrowUpward
import dev.aas.android.ui.icons.CloudOff
import dev.aas.android.ui.icons.CreateNewFolder
import dev.aas.android.ui.icons.Folder
import dev.aas.android.ui.icons.FolderOpen
import dev.aas.android.ui.icons.NoteAdd
import dev.aas.android.ui.icons.Storage
import dev.aas.android.ui.navigation.AppNavigator
import dev.aas.android.ui.navigation.NewProjectRoute
import dev.aas.android.ui.projects.OperationCard
import dev.aas.android.ui.theme.codeStyle
import dev.aas.android.ui.theme.statusColors

fun NavGraphBuilder.newProjectDestinations(navigator: AppNavigator) {
    composable<NewProjectRoute> {
        val vm = aasViewModel { c, _ -> NewProjectViewModel(c.projectRepository, c.engine.workspace) }
        NewProjectScreen(vm, navigator)
    }
}

@OptIn(ExperimentalMaterial3Api::class)
@Composable
fun NewProjectScreen(vm: NewProjectViewModel, navigator: AppNavigator) {
    val ui by vm.state.collectAsStateWithLifecycle()
    LaunchedEffect(vm) {
        vm.eventFlow.collect { event ->
            when (event) {
                is NewProjectEvent.Created -> navigator.projectCreated(event.projectId, event.startThread)
                NewProjectEvent.Done -> navigator.back()
            }
        }
    }
    BackHandler { if (!vm.back()) navigator.back() }
    val step = ui.step
    Scaffold(
        topBar = {
            TopAppBar(
                title = {
                    Text(
                        stringResource(
                            when (step) {
                                NewProjectStep.Choose -> R.string.newproject_title
                                is NewProjectStep.Details -> R.string.newproject_new_title
                                is NewProjectStep.Browse -> if (step.purpose == BrowsePurpose.Existing) R.string.newproject_existing_title else R.string.newproject_location_title
                                is NewProjectStep.Working -> R.string.newproject_title
                                is NewProjectStep.Cloning, is NewProjectStep.CloneEnded -> R.string.operation_clone
                            },
                        ),
                    )
                },
                navigationIcon = {
                    IconButton(onClick = { if (!vm.back()) navigator.back() }) { Icon(Icons.AutoMirrored.Outlined.ArrowBack, contentDescription = stringResource(R.string.back)) }
                },
            )
        },
    ) { padding ->
        Column(Modifier.fillMaxSize().padding(padding).imePadding()) {
            ui.error?.let { error ->
                Surface(color = MaterialTheme.statusColors.error.copy(alpha = ERROR_ALPHA), modifier = Modifier.fillMaxWidth()) {
                    Row(Modifier.padding(horizontal = 16.dp, vertical = 8.dp), verticalAlignment = Alignment.CenterVertically) {
                        Text(error.asString(), style = MaterialTheme.typography.bodyMedium, modifier = Modifier.weight(1f))
                        TextButton(onClick = vm::dismissError) { Text(stringResource(R.string.dismiss)) }
                    }
                }
            }
            when (step) {
                NewProjectStep.Choose -> ChooseStep(onExisting = vm::chooseExisting, onNew = vm::chooseNew)
                is NewProjectStep.Details -> DetailsStep(step, vm)
                is NewProjectStep.Browse -> BrowseStep(step, vm)
                is NewProjectStep.Working -> Box(Modifier.fillMaxSize(), contentAlignment = Alignment.Center) {
                    Column(horizontalAlignment = Alignment.CenterHorizontally) {
                        CircularProgressIndicator()
                        Spacer(Modifier.height(16.dp))
                        Text(step.message.asString(), style = MaterialTheme.typography.bodyLarge)
                        Text(step.note.asString(), style = MaterialTheme.typography.bodySmall, color = MaterialTheme.colorScheme.onSurfaceVariant, modifier = Modifier.padding(24.dp))
                        // Once the request is in the outbox it can be taken back (while it waits there).
                        if (step.clientRequestId != null) TextButton(onClick = vm::discardWork) { Text(stringResource(R.string.pending_discard)) }
                    }
                }
                is NewProjectStep.Cloning -> Column(Modifier.fillMaxWidth().padding(vertical = 16.dp)) {
                    OperationCard(step.operation, onCancel = vm::cancelClone)
                    Text(stringResource(R.string.newproject_clone_note), style = MaterialTheme.typography.bodySmall, color = MaterialTheme.colorScheme.onSurfaceVariant, modifier = Modifier.padding(horizontal = 24.dp))
                }
                is NewProjectStep.CloneEnded -> CloneEndedStep(step, onRetry = vm::retryClone, onBack = { vm.back() })
            }
        }
    }
}

@Composable
private fun ChooseStep(onExisting: () -> Unit, onNew: () -> Unit) {
    Column(Modifier.fillMaxWidth().padding(16.dp), verticalArrangement = Arrangement.spacedBy(12.dp)) {
        ChoiceCard(Icons.Outlined.FolderOpen, stringResource(R.string.newproject_existing), stringResource(R.string.newproject_existing_body), onExisting)
        ChoiceCard(Icons.AutoMirrored.Outlined.NoteAdd, stringResource(R.string.newproject_new), stringResource(R.string.newproject_new_body), onNew)
    }
}

@Composable
private fun ChoiceCard(icon: androidx.compose.ui.graphics.vector.ImageVector, title: String, body: String, onClick: () -> Unit) {
    Card(Modifier.fillMaxWidth().clickable(onClick = onClick)) {
        Row(Modifier.padding(16.dp), verticalAlignment = Alignment.CenterVertically) {
            Icon(icon, null, Modifier.size(32.dp), tint = MaterialTheme.colorScheme.primary)
            Spacer(Modifier.width(16.dp))
            Column {
                Text(title, style = MaterialTheme.typography.titleMedium)
                Text(body, style = MaterialTheme.typography.bodyMedium, color = MaterialTheme.colorScheme.onSurfaceVariant)
            }
        }
    }
}

@Composable
private fun DetailsStep(step: NewProjectStep.Details, vm: NewProjectViewModel) {
    Column(Modifier.fillMaxSize().verticalScroll(rememberScrollState()).padding(16.dp), verticalArrangement = Arrangement.spacedBy(8.dp)) {
        Text(stringResource(R.string.newproject_kind), style = MaterialTheme.typography.titleSmall)
        KindOption(InitKind.Empty, step.kind, R.string.newproject_kind_empty, R.string.newproject_kind_empty_body, vm::setKind)
        KindOption(InitKind.GitInit, step.kind, R.string.newproject_kind_git_init, R.string.newproject_kind_git_init_body, vm::setKind)
        KindOption(InitKind.GitClone, step.kind, R.string.newproject_kind_clone, R.string.newproject_kind_clone_body, vm::setKind)
        if (step.kind == InitKind.GitClone) {
            OutlinedTextField(
                value = step.url,
                onValueChange = vm::setUrl,
                label = { Text(stringResource(R.string.newproject_clone_url)) },
                placeholder = { Text(stringResource(R.string.newproject_clone_url_hint)) },
                singleLine = true,
                isError = step.urlLooksWrong,
                supportingText = {
                    Text(stringResource(if (step.urlLooksWrong) R.string.newproject_clone_url_check else R.string.newproject_clone_credentials))
                },
                modifier = Modifier.fillMaxWidth(),
            )
        }
        OutlinedTextField(
            value = step.name,
            onValueChange = vm::setName,
            label = { Text(stringResource(R.string.newproject_name)) },
            singleLine = true,
            isError = step.nameProblem != null && step.nameEdited,
            supportingText = step.nameProblem?.takeIf { step.nameEdited }?.let { { Text(stringResource(NewProjectViewModel.nameProblemText(it))) } },
            modifier = Modifier.fillMaxWidth(),
        )
        Spacer(Modifier.height(8.dp))
        Button(onClick = vm::toLocation, enabled = step.canContinue, modifier = Modifier.align(Alignment.End)) { Text(stringResource(R.string.newproject_choose_location)) }
    }
}

@Composable
private fun KindOption(kind: InitKind, selected: InitKind, label: Int, body: Int, onSelect: (InitKind) -> Unit) {
    Row(
        Modifier.fillMaxWidth().selectable(selected = kind == selected, role = Role.RadioButton) { onSelect(kind) }.padding(vertical = 6.dp),
        verticalAlignment = Alignment.CenterVertically,
    ) {
        RadioButton(selected = kind == selected, onClick = null)
        Column(Modifier.padding(start = 12.dp)) {
            Text(stringResource(label), style = MaterialTheme.typography.bodyLarge)
            Text(stringResource(body), style = MaterialTheme.typography.bodySmall, color = MaterialTheme.colorScheme.onSurfaceVariant)
        }
    }
}

@Composable
private fun BrowseStep(step: NewProjectStep.Browse, vm: NewProjectViewModel) {
    var typing by rememberSaveable { mutableStateOf(false) }
    var newFolder by rememberSaveable { mutableStateOf(false) }
    Column(Modifier.fillMaxSize()) {
        // Where we are, with a way up.
        Row(Modifier.fillMaxWidth().padding(start = 4.dp, end = 8.dp), verticalAlignment = Alignment.CenterVertically) {
            IconButton(onClick = vm::up, enabled = step.path != null) { Icon(Icons.Outlined.ArrowUpward, contentDescription = stringResource(R.string.newproject_up)) }
            Text(
                step.path ?: stringResource(R.string.newproject_roots),
                style = if (step.path != null) MaterialTheme.codeStyle else MaterialTheme.typography.titleSmall,
                maxLines = 2,
                overflow = TextOverflow.Ellipsis,
                modifier = Modifier.weight(1f),
            )
            TextButton(onClick = { typing = true }) { Text(stringResource(R.string.newproject_type_path)) }
        }
        step.details?.let { details ->
            Text(
                stringResource(R.string.newproject_location_for, details.name),
                style = MaterialTheme.typography.bodySmall,
                color = MaterialTheme.colorScheme.onSurfaceVariant,
                modifier = Modifier.padding(horizontal = 16.dp),
            )
        }
        HorizontalDivider()
        Box(Modifier.weight(1f).fillMaxWidth()) {
            when (val listing = step.listing) {
                Listing.Loading -> CircularProgressIndicator(Modifier.align(Alignment.Center))
                Listing.Offline -> EmptyState(Icons.Outlined.CloudOff, stringResource(R.string.newproject_offline), action = stringResource(R.string.action_retry), onAction = vm::retry)
                is Listing.Failed -> EmptyState(Icons.Outlined.Folder, listing.message.asString(), action = stringResource(R.string.action_retry), onAction = vm::retry)
                is Listing.Loaded -> if (listing.entries.isEmpty()) {
                    EmptyState(Icons.Outlined.Folder, stringResource(if (step.path == null) R.string.newproject_no_roots else R.string.newproject_no_folders))
                } else {
                    LazyColumn(Modifier.fillMaxSize()) {
                        items(listing.entries, key = { it.path }) { entry -> FolderRow(entry, isRoot = step.path == null) { vm.open(entry.path) } }
                    }
                }
            }
        }
        if (step.path != null) {
            HorizontalDivider()
            Row(Modifier.fillMaxWidth().padding(horizontal = 16.dp, vertical = 8.dp), verticalAlignment = Alignment.CenterVertically) {
                OutlinedButton(onClick = { newFolder = true }) {
                    Icon(Icons.Outlined.CreateNewFolder, null, Modifier.size(18.dp))
                    Spacer(Modifier.width(6.dp))
                    Text(stringResource(R.string.newproject_new_folder))
                }
                Spacer(Modifier.weight(1f))
                if (step.purpose == BrowsePurpose.Existing) {
                    Button(onClick = vm::openCurrentFolder) { Text(stringResource(R.string.newproject_open_this)) }
                } else {
                    Button(onClick = vm::createHere) { Text(stringResource(R.string.newproject_create_here)) }
                }
            }
        }
    }
    if (typing) {
        TextInputDialog(
            title = stringResource(R.string.newproject_type_path),
            initial = step.path ?: "",
            confirm = stringResource(R.string.newproject_go),
            label = stringResource(R.string.newproject_path),
            onConfirm = {
                typing = false
                vm.openTyped(it)
            },
            onDismiss = { typing = false },
        )
    }
    if (newFolder) {
        TextInputDialog(
            title = stringResource(R.string.newproject_new_folder),
            initial = "",
            confirm = stringResource(R.string.newproject_create_folder),
            label = stringResource(R.string.newproject_folder_name),
            onConfirm = {
                newFolder = false
                vm.mkdir(it)
            },
            onDismiss = { newFolder = false },
        )
    }
}

@Composable
private fun FolderRow(entry: FsEntry, isRoot: Boolean, onOpen: () -> Unit) {
    Row(Modifier.fillMaxWidth().clickable(onClick = onOpen).padding(horizontal = 16.dp, vertical = 12.dp), verticalAlignment = Alignment.CenterVertically) {
        Icon(if (isRoot) Icons.Outlined.Storage else Icons.Outlined.Folder, null, tint = MaterialTheme.colorScheme.primary)
        Spacer(Modifier.width(16.dp))
        Column(Modifier.weight(1f)) {
            Text(if (isRoot) entry.name else ServerPaths.name(entry.path).ifEmpty { entry.name }, style = MaterialTheme.typography.bodyLarge, maxLines = 1, overflow = TextOverflow.Ellipsis)
            if (isRoot) Text(entry.path, style = MaterialTheme.codeStyle, color = MaterialTheme.colorScheme.onSurfaceVariant, maxLines = 1, overflow = TextOverflow.Ellipsis)
        }
        if (entry.isGitRepo == true) Text(stringResource(R.string.newproject_git_repo), style = MaterialTheme.typography.labelSmall, color = MaterialTheme.statusColors.connected)
    }
}

@Composable
private fun CloneEndedStep(step: NewProjectStep.CloneEnded, onRetry: () -> Unit, onBack: () -> Unit) {
    val cancelled = step.operation.status == OperationStatus.Cancelled
    Column(Modifier.fillMaxWidth().padding(24.dp), verticalArrangement = Arrangement.spacedBy(12.dp)) {
        Text(stringResource(if (cancelled) R.string.newproject_clone_cancelled else R.string.newproject_clone_failed), style = MaterialTheme.typography.titleMedium)
        step.operation.message?.let { Text(it, style = MaterialTheme.typography.bodyMedium, color = if (cancelled) MaterialTheme.colorScheme.onSurfaceVariant else MaterialTheme.statusColors.error) }
        Row(horizontalArrangement = Arrangement.spacedBy(8.dp)) {
            OutlinedButton(onClick = onBack) { Text(stringResource(R.string.newproject_edit)) }
            Button(onClick = onRetry) { Text(stringResource(R.string.action_retry)) }
        }
    }
}

private const val ERROR_ALPHA = 0.14f
