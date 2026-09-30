package dev.aas.android.ui.newthread

import android.content.pm.PackageManager
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.ExperimentalLayoutApi
import androidx.compose.foundation.layout.FlowRow
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.imePadding
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.selection.selectable
import androidx.compose.foundation.verticalScroll
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.automirrored.outlined.ArrowBack
import androidx.compose.material3.ExperimentalMaterial3Api
import androidx.compose.material3.FilterChip
import androidx.compose.material3.Icon
import androidx.compose.material3.IconButton
import androidx.compose.material3.LinearProgressIndicator
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.OutlinedTextField
import androidx.compose.material3.RadioButton
import androidx.compose.material3.Scaffold
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
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.platform.LocalResources
import androidx.compose.ui.res.stringResource
import androidx.compose.ui.semantics.Role
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.unit.dp
import androidx.lifecycle.compose.collectAsStateWithLifecycle
import androidx.navigation.NavGraphBuilder
import androidx.navigation.compose.composable
import androidx.navigation.toRoute
import dev.aas.android.R
import dev.aas.android.domain.HarnessState
import dev.aas.android.domain.composer.ComposerTrigger
import dev.aas.android.domain.composer.HarnessSettings
import dev.aas.android.protocol.PickerKind
import dev.aas.android.ui.common.LocalAppContainer
import dev.aas.android.ui.common.aasViewModel
import dev.aas.android.ui.components.HarnessWaitNotice
import dev.aas.android.ui.components.SectionHeader
import dev.aas.android.ui.components.StatusDot
import dev.aas.android.ui.components.color
import dev.aas.android.ui.components.label
import dev.aas.android.ui.composer.ComposerBar
import dev.aas.android.ui.composer.ComposerChip
import dev.aas.android.ui.composer.ModelSheet
import dev.aas.android.ui.composer.PermissionSheet
import dev.aas.android.ui.composer.PromptTemplates
import dev.aas.android.ui.composer.rememberImageSources
import dev.aas.android.ui.navigation.AppNavigator
import dev.aas.android.ui.navigation.NewThreadRoute
import dev.aas.android.ui.theme.statusColors
import dev.aas.android.ui.thread.ProjectTrustBanner

fun NavGraphBuilder.newThreadDestinations(navigator: AppNavigator) {
    composable<NewThreadRoute> { entry ->
        val route = entry.toRoute<NewThreadRoute>()
        val res = LocalResources.current
        val vm = aasViewModel(key = "${route.projectId}/${route.harnessId}") { c, _ ->
            NewThreadViewModel(
                route = route,
                workspace = c.engine.workspace,
                threads = c.threadRepository,
                projects = c.projectRepository,
                status = c.engine.status,
                messages = c.userMessages,
                drafts = c.composerDrafts,
                uploader = c.blobUploader,
                policy = c.policy,
                templates = PromptTemplates(res.getString(R.string.template_review), res.getString(R.string.template_init)),
                outbox = c.engine.outbox,
                harnesses = c.harnessRepository,
                sentDrafts = c.sentDrafts,
            )
        }
        NewThreadScreen(vm, navigator)
    }
}

@OptIn(ExperimentalMaterial3Api::class, ExperimentalLayoutApi::class)
@Composable
fun NewThreadScreen(vm: NewThreadViewModel, navigator: AppNavigator) {
    val ui by vm.state.collectAsStateWithLifecycle()
    val composer by vm.composer.state.collectAsStateWithLifecycle()
    val policy = LocalAppContainer.current.policy
    var picker by rememberSaveable { mutableStateOf<PickerKind?>(null) }
    // The model the model sheet opens with (`/model <id>` whose model needs another permission mode).
    var pickerModel by rememberSaveable { mutableStateOf<String?>(null) }
    val imageSources = rememberImageSources(policy.maxImagesPerMessage, onPicked = vm::pickImages)
    val hasCamera = LocalContext.current.packageManager.hasSystemFeature(PackageManager.FEATURE_CAMERA_ANY)
    LaunchedEffect(vm) {
        vm.eventFlow.collect { event ->
            when (event) {
                is NewThreadEvent.Created -> navigator.threadCreated(event.threadId)
                is NewThreadEvent.OpenPicker -> {
                    pickerModel = event.model
                    picker = event.kind
                }
                is NewThreadEvent.OpenImport -> navigator.importSession(event.projectId, event.harnessId)
            }
        }
    }
    val slash = composer.trigger is ComposerTrigger.Slash
    LaunchedEffect(slash, ui.harness?.id, ui.online) { if (slash) vm.loadCommands() }
    val harness = ui.harness
    Scaffold(
        topBar = {
            TopAppBar(
                title = {
                    Column {
                        Text(stringResource(R.string.newthread_title))
                        ui.project?.let { Text(it.name, style = MaterialTheme.typography.labelSmall, color = MaterialTheme.colorScheme.onSurfaceVariant, maxLines = 1, overflow = TextOverflow.Ellipsis) }
                    }
                },
                navigationIcon = { IconButton(onClick = navigator::back) { Icon(Icons.AutoMirrored.Outlined.ArrowBack, contentDescription = stringResource(R.string.back)) } },
            )
        },
        bottomBar = {
            Column(Modifier.fillMaxWidth().imePadding()) {
                ui.waiting?.let { wait ->
                    HarnessWaitNotice(
                        wait = wait,
                        probing = wait.harnessId in ui.probing,
                        onRefresh = { vm.refreshHarness(wait.harnessId) },
                        onDiscard = vm::discardCreation,
                        modifier = Modifier.padding(horizontal = 12.dp, vertical = 6.dp),
                    )
                }
                val chosen = harness
                if (ui.trustUndecided && chosen != null) {
                    ProjectTrustBanner(chosen.displayName, onTrust = { vm.setTrust(true) }, onDistrust = { vm.setTrust(false) })
                }
                if (ui.creating) LinearProgressIndicator(Modifier.fillMaxWidth())
                ComposerBar(
                    value = vm.composer.textValue,
                    state = composer,
                    send = ui.send,
                    placeholder = stringResource(R.string.composer_placeholder_new),
                    imagesAllowed = harness?.capabilities?.images == true,
                    inputEnabled = !ui.creating,
                    onValueChange = vm.composer::onValueChange,
                    onChoose = vm::choose,
                    onChooseMention = vm.composer::chooseMention,
                    onPickImages = imageSources.pickFromGallery,
                    onTakePhoto = if (hasCamera) imageSources.takePhoto else null,
                    onRemoveAttachment = vm.composer::removeAttachment,
                    onRetryAttachment = vm.composer::retryAttachment,
                    onSend = { vm.create() },
                )
            }
        },
    ) { padding ->
        Column(Modifier.fillMaxSize().padding(padding).verticalScroll(rememberScrollState())) {
            val unavailable = ui.harnesses.filter { !it.available }
            SectionHeader(stringResource(R.string.newthread_harness)) {
                if (unavailable.isNotEmpty()) TextButton(onClick = { vm.refreshHarness(null) }) { Text(stringResource(R.string.harness_refresh)) }
            }
            FlowRow(Modifier.padding(horizontal = 16.dp), horizontalArrangement = Arrangement.spacedBy(8.dp)) {
                ui.harnesses.forEach { h ->
                    FilterChip(
                        selected = h.id == harness?.id,
                        onClick = { vm.selectHarness(h) },
                        enabled = h.available,
                        label = { Text(h.displayName) },
                    )
                }
            }
            if (unavailable.isNotEmpty()) {
                // Unavailable harnesses cannot be chosen; each says why (or that it is being
                // checked again), and 再確認 above probes them after, say, a login on the PC.
                unavailable.forEach { h ->
                    val state = HarnessState.of(h, ui.probing)
                    Row(Modifier.padding(horizontal = 16.dp, vertical = 2.dp), verticalAlignment = Alignment.CenterVertically) {
                        StatusDot(state.color(), size = 6)
                        Text(
                            stringResource(R.string.harness_name_state, h.displayName, state.label()),
                            style = MaterialTheme.typography.bodySmall,
                            color = MaterialTheme.colorScheme.onSurfaceVariant,
                            modifier = Modifier.padding(start = 6.dp),
                        )
                    }
                }
                Text(
                    stringResource(R.string.newthread_harness_refresh_note),
                    style = MaterialTheme.typography.bodySmall,
                    color = MaterialTheme.colorScheme.onSurfaceVariant,
                    modifier = Modifier.padding(horizontal = 16.dp, vertical = 2.dp),
                )
            }
            if (ui.harnesses.none { it.available }) {
                Text(stringResource(R.string.newthread_no_harness), style = MaterialTheme.typography.bodyMedium, color = MaterialTheme.statusColors.error, modifier = Modifier.padding(16.dp))
            }
            if (harness != null) {
                if (harness.models.isNotEmpty() || harness.effortLevels.isNotEmpty()) {
                    SectionHeader(stringResource(R.string.newthread_model))
                    Row(Modifier.padding(horizontal = 16.dp)) {
                        ComposerChip(HarnessSettings.label(harness, ui.choices.settings), onClick = { picker = PickerKind.Model })
                    }
                }
                HarnessSettings.permission(harness, ui.choices.settings)?.let { mode ->
                    SectionHeader(stringResource(R.string.newthread_permission))
                    Row(Modifier.padding(horizontal = 16.dp)) { ComposerChip(mode.label, onClick = { picker = PickerKind.PermissionMode }) }
                    mode.description?.let { Text(it, style = MaterialTheme.typography.bodySmall, color = MaterialTheme.colorScheme.onSurfaceVariant, modifier = Modifier.padding(horizontal = 16.dp, vertical = 4.dp)) }
                }
            }
            SectionHeader(stringResource(R.string.newthread_workspace))
            WorkspaceOption(selected = !ui.choices.worktree, enabled = true, label = R.string.workspace_local, body = R.string.workspace_local_body) { vm.setWorktree(false) }
            WorkspaceOption(
                selected = ui.choices.worktree,
                enabled = ui.worktreeAvailable,
                label = R.string.workspace_worktree,
                body = if (ui.worktreeAvailable) R.string.workspace_worktree_body else R.string.workspace_worktree_needs_git,
            ) { vm.setWorktree(true) }
            if (ui.choices.worktree && ui.worktreeAvailable) {
                OutlinedTextField(
                    value = ui.choices.branch,
                    onValueChange = vm::setBranch,
                    label = { Text(stringResource(R.string.workspace_branch)) },
                    placeholder = { Text(stringResource(R.string.workspace_branch_hint)) },
                    singleLine = true,
                    modifier = Modifier.fillMaxWidth().padding(horizontal = 16.dp, vertical = 4.dp),
                )
                OutlinedTextField(
                    value = ui.choices.baseRef,
                    onValueChange = vm::setBaseRef,
                    label = { Text(stringResource(R.string.workspace_base)) },
                    placeholder = { Text(stringResource(R.string.workspace_base_hint)) },
                    singleLine = true,
                    modifier = Modifier.fillMaxWidth().padding(horizontal = 16.dp, vertical = 4.dp),
                )
            }
            if (!ui.online) {
                Text(stringResource(R.string.newthread_offline_note), style = MaterialTheme.typography.bodySmall, color = MaterialTheme.colorScheme.onSurfaceVariant, modifier = Modifier.padding(16.dp))
            }
        }
    }
    if (harness != null) {
        when (picker) {
            PickerKind.Model, PickerKind.Effort -> ModelSheet(
                harness,
                ui.choices.settings,
                allowDefaultEffort = true,
                onApply = { choice -> vm.setModel(choice.model, choice.effort, choice.permissionMode) },
                onDismiss = {
                    picker = null
                    pickerModel = null
                },
                initialModel = pickerModel,
            )
            PickerKind.PermissionMode -> PermissionSheet(harness, ui.choices.settings, onApply = { vm.setPermission(it.id) }, onDismiss = { picker = null })
            PickerKind.Unknown, null -> Unit
        }
    }
}

@Composable
private fun WorkspaceOption(selected: Boolean, enabled: Boolean, label: Int, body: Int, onSelect: () -> Unit) {
    Row(
        Modifier.fillMaxWidth().selectable(selected = selected, enabled = enabled, role = Role.RadioButton, onClick = onSelect).padding(horizontal = 16.dp, vertical = 6.dp),
        verticalAlignment = Alignment.CenterVertically,
    ) {
        RadioButton(selected = selected, onClick = null, enabled = enabled)
        Column(Modifier.padding(start = 12.dp)) {
            Text(stringResource(label), style = MaterialTheme.typography.bodyLarge, color = if (enabled) MaterialTheme.colorScheme.onSurface else MaterialTheme.colorScheme.onSurfaceVariant)
            Text(stringResource(body), style = MaterialTheme.typography.bodySmall, color = MaterialTheme.colorScheme.onSurfaceVariant)
        }
    }
}
