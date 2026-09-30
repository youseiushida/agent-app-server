package dev.aas.android.ui.thread

import android.content.pm.PackageManager
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.PaddingValues
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.imePadding
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.foundation.lazy.items
import androidx.compose.foundation.lazy.rememberLazyListState
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.automirrored.outlined.ArrowBack
import androidx.compose.material.icons.outlined.KeyboardArrowDown
import androidx.compose.material.icons.outlined.MoreVert
import androidx.compose.material3.DropdownMenu
import androidx.compose.material3.DropdownMenuItem
import androidx.compose.material3.ExperimentalMaterial3Api
import androidx.compose.material3.Icon
import androidx.compose.material3.IconButton
import androidx.compose.material3.LinearProgressIndicator
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Scaffold
import androidx.compose.material3.SmallFloatingActionButton
import androidx.compose.material3.Text
import androidx.compose.material3.TopAppBar
import androidx.compose.runtime.Composable
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.derivedStateOf
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.rememberCoroutineScope
import androidx.compose.runtime.saveable.rememberSaveable
import androidx.compose.runtime.setValue
import androidx.compose.runtime.snapshotFlow
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.platform.LocalResources
import androidx.compose.ui.platform.testTag
import androidx.compose.ui.res.stringResource
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.unit.dp
import androidx.lifecycle.Lifecycle
import androidx.lifecycle.compose.LifecycleResumeEffect
import androidx.lifecycle.compose.LocalLifecycleOwner
import androidx.lifecycle.compose.collectAsStateWithLifecycle
import androidx.lifecycle.compose.currentStateAsState
import androidx.navigation.NavGraphBuilder
import androidx.navigation.compose.composable
import androidx.navigation.toRoute
import dev.aas.android.R
import dev.aas.android.domain.PlanChoices
import dev.aas.android.domain.composer.ComposerText
import dev.aas.android.domain.composer.ComposerTrigger
import dev.aas.android.domain.composer.HarnessSettings
import dev.aas.android.domain.composer.SendAction
import dev.aas.android.domain.composer.SendConfirmation
import dev.aas.android.domain.composer.SendLogic
import dev.aas.android.domain.timeline.Timeline
import dev.aas.android.domain.timeline.TimelineRow
import dev.aas.android.protocol.InteractionRequest
import dev.aas.android.protocol.InteractionStatus
import dev.aas.android.protocol.Item
import dev.aas.android.protocol.PickerKind
import dev.aas.android.protocol.QueuedInput
import dev.aas.android.protocol.ThreadSettings
import dev.aas.android.protocol.ThreadStatus
import dev.aas.android.sync.ThreadSync
import dev.aas.android.ui.common.LocalAppContainer
import dev.aas.android.ui.common.aasViewModel
import dev.aas.android.ui.common.asString
import dev.aas.android.ui.components.Banner
import dev.aas.android.ui.components.ConfirmDialog
import dev.aas.android.ui.components.EmptyState
import dev.aas.android.ui.components.HarnessWaitNotice
import dev.aas.android.ui.components.TextInputDialog
import dev.aas.android.ui.components.ThreadActivityChip
import dev.aas.android.ui.composer.ComposerBar
import dev.aas.android.ui.composer.ComposerChip
import dev.aas.android.ui.composer.ModelSheet
import dev.aas.android.ui.composer.PermissionSheet
import dev.aas.android.ui.composer.PromptTemplates
import dev.aas.android.ui.composer.rememberImageSources
import dev.aas.android.ui.icons.ChatBubbleOutline
import dev.aas.android.ui.icons.DeleteOutline
import dev.aas.android.ui.icons.Difference
import dev.aas.android.ui.icons.PushPin
import dev.aas.android.ui.interaction.InteractionCard
import dev.aas.android.ui.interaction.QuestionSheet
import dev.aas.android.ui.navigation.AppNavigator
import dev.aas.android.ui.navigation.ImageRoute
import dev.aas.android.ui.navigation.ItemOutputRoute
import dev.aas.android.ui.navigation.TaskOutputRoute
import dev.aas.android.ui.navigation.ThreadRoute
import dev.aas.android.ui.theme.statusColors
import kotlinx.coroutines.launch

fun NavGraphBuilder.threadDestinations(navigator: AppNavigator) {
    composable<ThreadRoute> { entry ->
        val route = entry.toRoute<ThreadRoute>()
        val res = LocalResources.current
        val vm = aasViewModel(key = "${route.threadId}/${route.interactionId}") { c, handle ->
            ThreadViewModel(
                route = route,
                threads = c.threadRepository,
                interactions = c.interactionRepository,
                workspace = c.engine.workspace,
                outbox = c.engine.outbox,
                status = c.engine.status,
                settings = c.settings.settings,
                visibility = c.visibility,
                messages = c.userMessages,
                drafts = c.composerDrafts,
                sentDrafts = c.sentDrafts,
                uploader = c.blobUploader,
                policy = c.policy,
                templates = PromptTemplates(res.getString(R.string.template_review), res.getString(R.string.template_init)),
                saved = handle,
                harnesses = c.harnessRepository,
            )
        }
        ThreadScreen(vm, navigator)
    }
    composable<ItemOutputRoute> { entry ->
        val route = entry.toRoute<ItemOutputRoute>()
        val vm = aasViewModel(key = "${route.threadId}/${route.itemId}") { c, _ ->
            OutputViewModel(route.threadId, OutputTarget.OfItem(route.itemId), c.threadRepository, c.blobRepository, c.policy)
        }
        OutputScreen(vm, navigator)
    }
    composable<TaskOutputRoute> { entry ->
        val route = entry.toRoute<TaskOutputRoute>()
        val vm = aasViewModel(key = "${route.threadId}/task/${route.taskId}") { c, _ ->
            OutputViewModel(route.threadId, OutputTarget.OfTask(route.taskId), c.threadRepository, c.blobRepository, c.policy)
        }
        OutputScreen(vm, navigator)
    }
    composable<ImageRoute> { entry ->
        ImageScreen(entry.toRoute<ImageRoute>().blobId, navigator)
    }
}

/** Test tag of the conversation list. */
const val THREAD_LIST_TAG = "thread-list"

/**
 * The thread (docs/ux/codex-desktop.md §3, §8): the conversation newest at the bottom (it follows
 * new content while the user is at the bottom; otherwise a button brings them back), approvals
 * and questions in place plus a banner above the composer, the daemon's queue, and the composer.
 * Everything stored locally stays readable offline; what is sent offline waits in the outbox and
 * is marked as such.
 */
@OptIn(ExperimentalMaterial3Api::class)
@Composable
fun ThreadScreen(vm: ThreadViewModel, navigator: AppNavigator) {
    val ui by vm.state.collectAsStateWithLifecycle()
    val composer by vm.composer.state.collectAsStateWithLifecycle()
    val sideQuestion by vm.sideQuestion.collectAsStateWithLifecycle()
    val harnessStatus by vm.harnessStatus.collectAsStateWithLifecycle()
    val focusHandled by vm.focusHandled.collectAsStateWithLifecycle()
    val policy = LocalAppContainer.current.policy
    val state = ui.thread
    val thread = state.thread
    val listState = rememberLazyListState()
    val scope = rememberCoroutineScope()
    var questionFor by rememberSaveable { mutableStateOf<String?>(null) }
    var picker by rememberSaveable { mutableStateOf<PickerKind?>(null) }
    // The model the model sheet opens with (`/model <id>` whose model needs another effort).
    var pickerModel by rememberSaveable { mutableStateOf<String?>(null) }
    var showStatus by rememberSaveable { mutableStateOf(false) }
    var showRename by rememberSaveable { mutableStateOf(false) }
    var confirmArchive by rememberSaveable { mutableStateOf(false) }
    var confirmStop by rememberSaveable { mutableStateOf(false) }
    var confirmStopTask by rememberSaveable { mutableStateOf<String?>(null) }
    var confirmExitPlan by rememberSaveable { mutableStateOf(false) }
    // A row to show once it exists (a background task's card appears when its section opens).
    var scrollTarget by remember { mutableStateOf<String?>(null) }
    var editQueuedId by rememberSaveable { mutableStateOf<String?>(null) }
    var pausedSend by rememberSaveable { mutableStateOf<SendAction?>(null) }
    var planSend by rememberSaveable { mutableStateOf<SendAction?>(null) }
    var menuOpen by remember { mutableStateOf(false) }
    val lifecycleState by LocalLifecycleOwner.current.lifecycle.currentStateAsState()
    val resumed = lifecycleState.isAtLeast(Lifecycle.State.RESUMED)
    val reversed = remember(ui.rows) { ui.rows.asReversed() }
    val actions = remember(vm, navigator) {
        ItemActions(
            onOpenOutput = { item -> navigator.openOutput(vm.threadId, item.id) },
            onOpenTurnDiff = { turnId -> navigator.openDiff(vm.threadId, turnId) },
            onOpenImage = { blobId -> navigator.openImage(blobId) },
            onOpenBackgroundTask = vm::openBackgroundTask,
            onMoveToBackground = vm::moveToBackground,
            onImplementPlan = vm::implementPlan,
            onImplementPlanInNewThread = vm::implementPlanInNewThread,
        )
    }
    val imageSources = rememberImageSources(policy.maxImagesPerMessage, onPicked = vm::pickImages)
    val hasCamera = LocalContext.current.packageManager.hasSystemFeature(PackageManager.FEATURE_CAMERA_ANY)

    LifecycleResumeEffect(vm) {
        vm.onVisible()
        onPauseOrDispose { vm.onHidden() }
    }
    // New activity while the thread is on screen is read at once.
    LaunchedEffect(thread?.head, resumed) {
        if (resumed && thread != null) vm.markViewed()
    }

    // Following the bottom: decided when the user's scroll ends, so programmatic changes
    // (content growing, new rows) never switch it off.
    var follow by remember { mutableStateOf(true) }
    val atBottom by remember { derivedStateOf { listState.firstVisibleItemIndex == 0 && listState.firstVisibleItemScrollOffset == 0 } }
    LaunchedEffect(listState) {
        snapshotFlow { listState.isScrollInProgress }.collect { scrolling ->
            if (!scrolling) follow = listState.firstVisibleItemIndex == 0 && listState.firstVisibleItemScrollOffset == 0
        }
    }
    LaunchedEffect(reversed.firstOrNull()?.key, reversed.size) {
        if (follow && reversed.isNotEmpty() && !listState.isScrollInProgress) listState.scrollToItem(0)
    }

    // Older turns load when their row comes into view (online; offline the row offers a retry).
    val olderVisible by remember { derivedStateOf { listState.layoutInfo.visibleItemsInfo.any { it.key == TimelineRow.LoadOlder.key } } }
    LaunchedEffect(olderVisible, ui.online) {
        if (olderVisible && ui.online && !ui.loadingOlder && ui.olderError == null) vm.loadOlder()
    }

    // The `/` palette needs the command list (again when the harness changed it).
    val slash = composer.trigger is ComposerTrigger.Slash
    LaunchedEffect(slash, state.commandsVersion, ui.online) {
        if (slash) vm.loadCommands()
    }

    LaunchedEffect(vm) {
        vm.eventFlow.collect { event ->
            when (event) {
                is ThreadEvent.OpenThread -> navigator.openThread(event.threadId)
                is ThreadEvent.OpenDiff -> navigator.openDiff(vm.threadId, event.turnId)
                is ThreadEvent.OpenNewThread -> navigator.newThread(event.projectId, event.harnessId)
                is ThreadEvent.OpenImport -> navigator.importSession(event.projectId, event.harnessId)
                is ThreadEvent.OpenPicker -> {
                    pickerModel = event.model
                    picker = event.kind
                }
                ThreadEvent.ShowStatus -> showStatus = true
                ThreadEvent.ShowRename -> showRename = true
                ThreadEvent.ConfirmArchive -> confirmArchive = true
                ThreadEvent.ConfirmStop -> confirmStop = true
                ThreadEvent.Leave -> navigator.back()
                ThreadEvent.ScrollToBottom -> {
                    follow = true
                    listState.scrollToItem(0)
                }
                is ThreadEvent.ScrollToRow -> scrollTarget = event.key
            }
        }
    }
    LaunchedEffect(scrollTarget, reversed) {
        val target = scrollTarget ?: return@LaunchedEffect
        val index = reversed.indexOfFirst { it.key == target }
        if (index >= 0) {
            follow = false
            listState.animateScrollToItem(index)
            scrollTarget = null
        }
    }

    fun indexOfInteraction(id: String): Int = reversed.indexOfFirst { it.key == "interaction-$id" }

    // A notification asked for an interaction: open its question sheet or scroll to its card.
    LaunchedEffect(focusHandled, ui.pendingInteractions, reversed.size) {
        val focus = vm.route.interactionId
        if (focusHandled || focus == null) return@LaunchedEffect
        val interaction = ui.pendingInteractions.firstOrNull { it.id == focus }
        if (interaction != null) {
            if (interaction.request is InteractionRequest.Question) {
                questionFor = focus
            } else {
                val index = indexOfInteraction(focus)
                if (index >= 0) {
                    follow = false
                    listState.scrollToItem(index)
                }
            }
            vm.focusApplied()
        } else if (state.sync == ThreadSync.Live) {
            // Loaded and it is not pending any more (answered elsewhere): nothing to focus.
            vm.focusApplied()
        }
    }

    Scaffold(
        topBar = {
            TopAppBar(
                title = {
                    Column {
                        Text(thread?.title ?: stringResource(R.string.thread_loading), maxLines = 1, overflow = TextOverflow.Ellipsis)
                        Row(verticalAlignment = Alignment.CenterVertically) {
                            ui.activity?.let { ThreadActivityChip(it, backgroundRunning = thread?.background?.running ?: 0) }
                            if (thread?.pinned == true) Icon(Icons.Outlined.PushPin, stringResource(R.string.pinned), Modifier.padding(start = 6.dp).width(14.dp))
                            ui.project?.let {
                                Text(
                                    it.name,
                                    style = MaterialTheme.typography.labelSmall,
                                    color = MaterialTheme.colorScheme.onSurfaceVariant,
                                    maxLines = 1,
                                    overflow = TextOverflow.Ellipsis,
                                    modifier = Modifier.padding(start = 8.dp),
                                )
                            }
                        }
                    }
                },
                navigationIcon = {
                    IconButton(onClick = navigator::back) { Icon(Icons.AutoMirrored.Outlined.ArrowBack, contentDescription = stringResource(R.string.back)) }
                },
                actions = {
                    if (thread?.diffAvailable == true) {
                        IconButton(onClick = { navigator.openDiff(vm.threadId, null) }) { Icon(Icons.Outlined.Difference, contentDescription = stringResource(R.string.thread_menu_diff)) }
                    }
                    if (thread != null) {
                        Box {
                            IconButton(onClick = { menuOpen = true }) { Icon(Icons.Outlined.MoreVert, contentDescription = stringResource(R.string.more_actions)) }
                            DropdownMenu(expanded = menuOpen, onDismissRequest = { menuOpen = false }) {
                                MenuItem(R.string.thread_menu_rename) { menuOpen = false; showRename = true }
                                MenuItem(if (thread.pinned) R.string.thread_menu_unpin else R.string.thread_menu_pin) { menuOpen = false; vm.togglePin() }
                                if (ui.harness?.capabilities?.fork == true && !thread.archived) MenuItem(R.string.thread_menu_fork) { menuOpen = false; vm.fork() }
                                MenuItem(R.string.thread_menu_diff) { menuOpen = false; navigator.openDiff(vm.threadId, null) }
                                MenuItem(R.string.thread_menu_status) { menuOpen = false; showStatus = true }
                                MenuItem(R.string.thread_menu_new) { menuOpen = false; navigator.newThread(thread.projectId, thread.harnessId) }
                                MenuItem(R.string.thread_menu_mark_unread) { menuOpen = false; vm.markUnread() }
                                if (thread.status != ThreadStatus.Idle) MenuItem(R.string.thread_menu_stop) { menuOpen = false; confirmStop = true }
                                if (thread.archived) MenuItem(R.string.thread_unarchive) { menuOpen = false; vm.unarchive() }
                                else MenuItem(R.string.thread_menu_archive) { menuOpen = false; confirmArchive = true }
                            }
                        }
                    }
                },
            )
        },
        bottomBar = {
            if (state.sync != ThreadSync.Removed) {
                Column(Modifier.fillMaxWidth().imePadding()) {
                    ui.plan?.let { PlanPill(it) }
                    if (thread != null && thread.queuePaused && ui.queued.isNotEmpty() && !SendLogic.turnActive(thread)) {
                        PausedQueueBanner(ui.queued.size, onResume = vm::resumeQueue)
                    }
                    if (ui.queued.isNotEmpty()) {
                        QueuedList(ui.queued, ui.canSendQueuedNow, onEdit = { editQueuedId = it.id }, onSendNow = vm::steerQueued, onRemove = vm::removeQueued)
                    }
                    val harness = ui.harness
                    if (ui.trustUndecided && harness != null && thread?.archived == false) {
                        ProjectTrustBanner(harness.displayName, onTrust = { vm.setTrust(true) }, onDistrust = { vm.setTrust(false) })
                    }
                    ui.harnessWaits.forEach { wait ->
                        HarnessWaitNotice(
                            wait = wait,
                            probing = wait.harnessId in ui.probing,
                            onRefresh = { vm.refreshHarness(wait.harnessId) },
                            onDiscard = { vm.discardPending(wait.clientRequestId) },
                            modifier = Modifier.padding(horizontal = 12.dp, vertical = 4.dp),
                        )
                    }
                    InteractionBanner(ui.pendingInteractions) { interaction ->
                        if (interaction.request is InteractionRequest.Question) {
                            questionFor = interaction.id
                        } else {
                            val index = indexOfInteraction(interaction.id)
                            if (index >= 0) scope.launch {
                                follow = false
                                listState.animateScrollToItem(index)
                            }
                        }
                    }
                    ui.insertOffer?.let { text ->
                        ComposerInsertOffer(
                            text = text,
                            composerEmpty = vm.composer.textValue.text.isBlank(),
                            onReplace = { vm.acceptInsert(replace = true) },
                            onAppend = { vm.acceptInsert(replace = false) },
                            onDismiss = vm::dismissInsert,
                        )
                    }
                    ComposerBar(
                        value = vm.composer.textValue,
                        state = composer,
                        send = ui.send,
                        placeholder = placeholder(ui),
                        imagesAllowed = ui.harness?.capabilities?.images == true,
                        inputEnabled = thread != null && !thread.archived,
                        onValueChange = vm.composer::onValueChange,
                        onChoose = vm::choose,
                        onChooseMention = vm.composer::chooseMention,
                        onPickImages = imageSources.pickFromGallery,
                        onTakePhoto = if (hasCamera) imageSources.takePhoto else null,
                        onRemoveAttachment = vm.composer::removeAttachment,
                        onRetryAttachment = vm.composer::retryAttachment,
                        onSend = { action ->
                            when (vm.sendConfirmation(action)) {
                                is SendConfirmation.PlanAhead -> planSend = action
                                is SendConfirmation.PausedQueue -> pausedSend = action
                                null -> vm.send(action)
                            }
                        },
                        interruptHint = if (ui.keepsBackgroundOnInterrupt) stringResource(R.string.composer_hint_interrupt_background) else null,
                    ) {
                        if (thread?.modes?.plan == true) {
                            // Plan mode (the app's /plan, or the harness entered it): tapping offers to leave it.
                            ComposerChip(
                                stringResource(R.string.plan_chip),
                                onClick = if (ui.features.planMode != null) ({ confirmExitPlan = true }) else null,
                                emphasized = true,
                            )
                        }
                        if (harness != null && thread != null) {
                            ComposerChip(
                                HarnessSettings.label(harness, thread.settings),
                                onClick = if (harness.models.isNotEmpty() || harness.effortLevels.isNotEmpty()) ({ picker = PickerKind.Model }) else null,
                            )
                            HarnessSettings.permission(harness, thread.settings)?.let { mode -> ComposerChip(mode.label, onClick = { picker = PickerKind.PermissionMode }) }
                        }
                        if (thread?.modes?.fast == true) {
                            ComposerChip(
                                thread.fastModeState?.let { stringResource(R.string.fast_chip_state, it) } ?: stringResource(R.string.fast_chip),
                                onClick = { picker = PickerKind.Model },
                            )
                        }
                        ui.context?.let { ComposerChip(stringResource(R.string.composer_context, contextPercent(it)), onClick = { showStatus = true }) }
                        if (ui.otherPending > 0) ComposerChip(stringResource(R.string.composer_pending_changes, ui.otherPending), onClick = null)
                    }
                }
            }
        },
    ) { padding ->
        Box(Modifier.fillMaxSize().padding(padding)) {
            Column(Modifier.fillMaxSize()) {
                if (state.sync == ThreadSync.Loading) LinearProgressIndicator(Modifier.fillMaxWidth())
                if (!ui.online && state.sync != ThreadSync.Removed && thread != null) OfflineNote()
                if (state.sync == ThreadSync.Failed) {
                    Banner(
                        text = stringResource(R.string.thread_load_failed, state.loadError?.message.orEmpty()),
                        color = MaterialTheme.statusColors.error,
                        action = stringResource(R.string.action_retry),
                        onAction = vm::retryLoad,
                    )
                }
                if (thread?.archived == true) ArchivedBanner(onUnarchive = vm::unarchive)
                when {
                    state.sync == ThreadSync.Removed -> EmptyState(Icons.Outlined.DeleteOutline, stringResource(R.string.thread_removed))
                    thread == null && state.items.isEmpty() -> EmptyState(
                        Icons.Outlined.ChatBubbleOutline,
                        stringResource(if (state.sync == ThreadSync.Cached && !ui.online) R.string.thread_not_cached else R.string.thread_loading),
                    )
                    ui.rows.isEmpty() -> EmptyState(Icons.Outlined.ChatBubbleOutline, stringResource(R.string.thread_empty), body = stringResource(R.string.thread_empty_body))
                    else -> LazyColumn(
                        state = listState,
                        reverseLayout = true,
                        contentPadding = PaddingValues(vertical = 8.dp),
                        modifier = Modifier.fillMaxSize().testTag(THREAD_LIST_TAG),
                    ) {
                        items(reversed, key = { it.key }, contentType = { it.javaClass.name }) { row ->
                            TimelineRowView(row, ui, vm, actions, navigator, onOpenQuestion = { questionFor = it }, onStopTask = { confirmStopTask = it })
                        }
                    }
                }
            }
            if (!atBottom && ui.rows.isNotEmpty()) {
                SmallFloatingActionButton(
                    onClick = {
                        follow = true
                        scope.launch { listState.animateScrollToItem(0) }
                    },
                    modifier = Modifier.align(Alignment.BottomEnd).padding(16.dp),
                ) { Icon(Icons.Outlined.KeyboardArrowDown, contentDescription = stringResource(R.string.scroll_to_bottom)) }
            }
        }
    }

    // ----- sheets and dialogs -------------------------------------------------------------------

    val openQuestion = questionFor?.let { id -> ui.pendingInteractions.firstOrNull { it.id == id } }
    if (openQuestion != null) {
        QuestionSheet(openQuestion, onRespond = { vm.respond(openQuestion, it) }, onDismiss = { questionFor = null })
    }
    LaunchedEffect(questionFor, openQuestion == null, state.sync) {
        // The question was answered elsewhere or withdrawn: the sheet goes.
        if (questionFor != null && openQuestion == null && state.sync == ThreadSync.Live) questionFor = null
    }
    val harness = ui.harness
    if (harness != null && thread != null) {
        when (picker) {
            PickerKind.Model, PickerKind.Effort -> ModelSheet(
                harness,
                thread.settings,
                allowDefaultEffort = thread.settings.effort == null,
                onApply = { choice ->
                    vm.applySettings(
                        ThreadViewModel.settingsChange(harness, thread.settings, choice.model, choice.effort, choice.permissionMode),
                        ThreadViewModel.fastChange(thread.modes, choice.fast),
                    )
                },
                onDismiss = {
                    picker = null
                    pickerModel = null
                },
                initialModel = pickerModel,
                fastMode = if (harness.features.fastModeModels.isNotEmpty()) thread.modes.fast else null,
                fastModeState = thread.fastModeState,
            )
            PickerKind.PermissionMode -> PermissionSheet(harness, thread.settings, onApply = { vm.applySettings(ThreadSettings(permissionMode = it.id)) }, onDismiss = { picker = null })
            PickerKind.Unknown, null -> Unit
        }
    }
    LaunchedEffect(showStatus) {
        if (showStatus) vm.loadHarnessStatus()
    }
    if (showStatus && thread != null) {
        StatusSheet(
            thread,
            harness,
            ui.context,
            onDismiss = { showStatus = false },
            harnessStatus = harnessStatus,
            trust = ui.trust,
            onSetTrust = vm::setTrust,
        )
    }
    sideQuestion?.let { SideQuestionSheet(it, onDismiss = vm::dismissSideQuestion) }
    if (confirmExitPlan) {
        ConfirmDialog(
            title = stringResource(R.string.plan_exit_title),
            text = stringResource(R.string.plan_exit_body),
            confirm = stringResource(R.string.plan_exit_confirm),
            onConfirm = {
                confirmExitPlan = false
                vm.exitPlanMode()
            },
            onDismiss = { confirmExitPlan = false },
        )
    }
    if (showRename && thread != null) {
        TextInputDialog(
            title = stringResource(R.string.thread_rename_title),
            initial = thread.title,
            confirm = stringResource(R.string.save),
            label = stringResource(R.string.thread_rename_label),
            onConfirm = {
                showRename = false
                vm.rename(it)
            },
            onDismiss = { showRename = false },
        )
    }
    // The agent's process takes its background work with it: the dialogs name what runs.
    val runningTasks = ui.runningTasks
    val runningCount = maxOf(runningTasks.size, thread?.background?.running ?: 0)
    if (confirmArchive && thread != null) {
        val running = thread.status != ThreadStatus.Idle
        ConfirmDialog(
            title = stringResource(if (running) R.string.archive_running_title else R.string.archive_title),
            text = stringResource(if (running) R.string.archive_running_body else R.string.archive_body),
            confirm = stringResource(if (running) R.string.archive_running_confirm else R.string.thread_menu_archive),
            onConfirm = {
                confirmArchive = false
                vm.archive()
            },
            onDismiss = { confirmArchive = false },
            extra = if (running && runningCount > 0) ({ RunningBackgroundNote(runningCount, runningTasks.map { it.title }) }) else null,
        )
    }
    if (confirmStop) {
        ConfirmDialog(
            title = stringResource(R.string.stop_title),
            text = stringResource(R.string.stop_body),
            confirm = stringResource(R.string.thread_menu_stop),
            onConfirm = {
                confirmStop = false
                vm.stopProcess()
            },
            onDismiss = { confirmStop = false },
            extra = if (runningCount > 0) ({ RunningBackgroundNote(runningCount, runningTasks.map { it.title }) }) else null,
        )
    }
    // Only while 停止 applies: a task that ended (or is being stopped) meanwhile closes the dialog.
    val stoppingTask = confirmStopTask?.let { id -> ui.thread.backgroundTasks.firstOrNull { it.id == id } }
        ?.takeIf { ui.stopOf(it) == BackgroundStop.Available || ui.stopOf(it) == BackgroundStop.Unconfirmed }
    if (stoppingTask != null) {
        ConfirmDialog(
            title = stringResource(R.string.bg_stop_title),
            text = stringResource(R.string.bg_stop_body, stoppingTask.title),
            confirm = stringResource(R.string.bg_stop),
            onConfirm = {
                confirmStopTask = null
                vm.stopBackgroundTask(stoppingTask)
            },
            onDismiss = { confirmStopTask = null },
        )
    } else if (confirmStopTask != null && state.sync == ThreadSync.Live) {
        // The task ended, is being stopped, or is gone from the thread while the dialog was open.
        LaunchedEffect(confirmStopTask) { confirmStopTask = null }
    }
    val editing: QueuedInput? = editQueuedId?.let { id -> ui.queued.firstOrNull { it.id == id } }
    if (editing != null) {
        TextInputDialog(
            title = stringResource(R.string.queue_edit_title),
            initial = ComposerText.textOf(editing.input),
            confirm = stringResource(R.string.save),
            singleLine = false,
            onConfirm = {
                editQueuedId = null
                vm.editQueued(editing, it)
            },
            onDismiss = { editQueuedId = null },
            validate = { null },
        )
    } else if (editQueuedId != null && state.sync == ThreadSync.Live) {
        // It left the queue while the dialog was open.
        LaunchedEffect(editQueuedId) { editQueuedId = null }
    }
    planSend?.let { action ->
        val ahead = vm.sendConfirmation(action) as? SendConfirmation.PlanAhead
        if (ahead == null) {
            // Nothing starts before the request any more (the queue moved on, the draft changed):
            // the user sends again, now without the question.
            LaunchedEffect(action) { planSend = null }
        } else {
            PlanAheadDialog(
                ahead = ahead,
                onSend = {
                    planSend = null
                    vm.send(action)
                },
                onClearAndSend = if (ahead.canClear) {
                    {
                        planSend = null
                        vm.send(action, clearQueueFirst = true)
                    }
                } else {
                    null
                },
                onDismiss = { planSend = null },
            )
        }
    }
    pausedSend?.let { action ->
        PausedQueueDialog(
            count = ui.queued.size,
            onSend = {
                pausedSend = null
                vm.send(action)
            },
            onClearAndSend = {
                pausedSend = null
                vm.send(action, clearQueueFirst = true)
            },
            onDismiss = { pausedSend = null },
        )
    }
}

@Composable
private fun TimelineRowView(
    row: TimelineRow,
    ui: ThreadUiState,
    vm: ThreadViewModel,
    actions: ItemActions,
    navigator: AppNavigator,
    onOpenQuestion: (String) -> Unit,
    onStopTask: (String) -> Unit,
) {
    when (row) {
        TimelineRow.LoadOlder -> LoadOlderRow(ui.loadingOlder, ui.olderError?.asString(), vm::loadOlder)
        is TimelineRow.TurnStart -> TurnStartRow(
            row.turn,
            fork = ui.forkChoices(row.turn),
            onForkHere = { vm.forkAt(row.turn, before = false) },
            onEditPrompt = { vm.forkAt(row.turn, before = true) },
        )
        is TimelineRow.ItemRow -> ItemView(
            row.item,
            actions,
            backgroundTask = row.item.backgroundTaskId?.let { ui.backgroundTasks[it] },
            moveToBackground = ui.moveToBackground(row.item),
            planChoices = (row.item as? Item.ProposedPlan)?.let { ui.planChoices(it) } ?: PlanChoices.None,
        )
        is TimelineRow.ActivityGroup -> ActivityGroupRow(row.items, row.expanded) { vm.toggleGroup(row.groupKey, row.expanded) }
        is TimelineRow.GroupedItem -> ItemView(
            row.item,
            actions,
            Modifier.padding(start = 16.dp),
            backgroundTask = row.item.backgroundTaskId?.let { ui.backgroundTasks[it] },
            moveToBackground = ui.moveToBackground(row.item),
        )
        is TimelineRow.InteractionRow -> {
            val interaction = row.interaction
            if (interaction.status == InteractionStatus.Pending) {
                InteractionCard(
                    interaction = interaction,
                    responsePending = interaction.id in ui.answering,
                    onRespond = { vm.respond(interaction, it) },
                    onOpenQuestion = { onOpenQuestion(interaction.id) },
                    modifier = Modifier.padding(horizontal = 12.dp, vertical = 6.dp),
                    backgroundTaskTitle = interaction.backgroundTaskId?.let { ui.backgroundTasks[it]?.title },
                )
            } else {
                InteractionRecordRow(interaction)
            }
        }
        is TimelineRow.Working -> WorkingRow(row.turn, row.current, waitingForAnswer = ui.pendingInteractions.any { it.turnId == row.turn.id })
        is TimelineRow.TurnEnd -> TurnEndRow(
            row.turn,
            onOpenDiff = { navigator.openDiff(vm.threadId, row.turn.id) },
            resumeFailed = ui.resumeFailedChoices(row.turn),
            onRetry = { vm.retryTurn(row.turn) },
            onForkNew = { vm.forkAfterFailedResume(row.turn) },
        )
        is TimelineRow.Pending -> PendingInputRow(
            input = row.input,
            online = ui.online,
            harnessName = row.input.waitingForHarness?.let { id -> ui.harness?.takeIf { it.id == id }?.displayName ?: id },
            probing = row.input.waitingForHarness in ui.probing,
            onRefreshHarness = { row.input.waitingForHarness?.let(vm::refreshHarness) },
            onDiscard = { vm.discardPending(row.input.clientRequestId) },
        )
        is TimelineRow.BackgroundHeader -> BackgroundHeaderRow(row) { vm.toggleGroup(Timeline.BACKGROUND_SECTION, row.expanded) }
        is TimelineRow.BackgroundEndedHeader -> BackgroundEndedHeaderRow(row) { vm.toggleGroup(Timeline.BACKGROUND_ENDED, row.expanded) }
        is TimelineRow.BackgroundTaskRow -> BackgroundTaskCard(
            task = row.task,
            depth = row.depth,
            parentTitle = row.parentTitle,
            stop = ui.stopOf(row.task),
            onStop = { onStopTask(row.task.id) },
            onOpenOutput = { navigator.openTaskOutput(vm.threadId, row.task.id) },
        )
    }
}

@Composable
private fun MenuItem(label: Int, onClick: () -> Unit) {
    DropdownMenuItem(text = { Text(stringResource(label)) }, onClick = onClick)
}

@Composable
private fun placeholder(ui: ThreadUiState): String {
    val thread = ui.thread.thread
    return stringResource(
        when {
            thread == null -> R.string.thread_loading
            thread.archived -> R.string.composer_placeholder_archived
            ui.send.primary == SendAction.Interrupt || ui.send.primary == SendAction.Queue -> R.string.composer_placeholder_running_queue
            ui.send.primary == SendAction.Steer -> R.string.composer_placeholder_running_steer
            else -> R.string.composer_placeholder_follow_up
        },
    )
}

@Composable
private fun OfflineNote() {
    androidx.compose.material3.Surface(color = MaterialTheme.colorScheme.surfaceContainerHigh, modifier = Modifier.fillMaxWidth()) {
        Text(
            stringResource(R.string.thread_offline_note),
            style = MaterialTheme.typography.bodySmall,
            color = MaterialTheme.colorScheme.onSurfaceVariant,
            modifier = Modifier.padding(horizontal = 16.dp, vertical = 6.dp),
        )
    }
}

/**
 * `/plan <request>` while messages would start before the request (docs/android.md 31.3): plan
 * mode applies from the next turn, so they would run in plan mode too. The user chooses: send
 * anyway, clear the queue first (when all of them wait in the daemon's queue), or cancel (the
 * draft stays).
 */
@Composable
internal fun PlanAheadDialog(ahead: SendConfirmation.PlanAhead, onSend: () -> Unit, onClearAndSend: (() -> Unit)?, onDismiss: () -> Unit) {
    androidx.compose.material3.AlertDialog(
        onDismissRequest = onDismiss,
        title = { Text(stringResource(R.string.plan_ahead_title)) },
        text = { Text(stringResource(R.string.plan_ahead_body, ahead.ahead)) },
        confirmButton = {
            Column(horizontalAlignment = Alignment.End) {
                androidx.compose.material3.TextButton(onClick = onSend) { Text(stringResource(R.string.plan_ahead_send)) }
                if (onClearAndSend != null) {
                    androidx.compose.material3.TextButton(onClick = onClearAndSend) { Text(stringResource(R.string.plan_ahead_clear)) }
                }
                androidx.compose.material3.TextButton(onClick = onDismiss) { Text(stringResource(R.string.cancel)) }
            }
        },
    )
}

/**
 * Sending while the queue is paused starts a turn and resumes the queue (protocol.md §4). The
 * user chooses: send (the queue continues afterwards), clear the queue and send, or cancel.
 */
@Composable
private fun PausedQueueDialog(count: Int, onSend: () -> Unit, onClearAndSend: () -> Unit, onDismiss: () -> Unit) {
    androidx.compose.material3.AlertDialog(
        onDismissRequest = onDismiss,
        title = { Text(stringResource(R.string.paused_send_title)) },
        text = { Text(stringResource(R.string.paused_send_body, count)) },
        confirmButton = {
            Column(horizontalAlignment = Alignment.End) {
                androidx.compose.material3.TextButton(onClick = onSend) { Text(stringResource(R.string.paused_send_keep)) }
                androidx.compose.material3.TextButton(onClick = onClearAndSend) { Text(stringResource(R.string.paused_send_clear)) }
                androidx.compose.material3.TextButton(onClick = onDismiss) { Text(stringResource(R.string.cancel)) }
            }
        },
    )
}
