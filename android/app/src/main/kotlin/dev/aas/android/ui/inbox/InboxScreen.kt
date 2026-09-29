package dev.aas.android.ui.inbox

import androidx.compose.foundation.clickable
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.foundation.lazy.LazyListScope
import androidx.compose.foundation.lazy.items
import androidx.compose.material.icons.Icons
import androidx.compose.material3.ExperimentalMaterial3Api
import androidx.compose.material3.Icon
import androidx.compose.material3.IconButton
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Scaffold
import androidx.compose.material3.Text
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
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.unit.dp
import androidx.lifecycle.ViewModel
import androidx.lifecycle.compose.collectAsStateWithLifecycle
import androidx.lifecycle.viewModelScope
import androidx.navigation.NavGraphBuilder
import androidx.navigation.compose.composable
import dev.aas.android.AppContainer
import dev.aas.android.R
import dev.aas.android.data.InteractionRepository
import dev.aas.android.data.WorkspaceRepository
import dev.aas.android.domain.ErrorTexts
import dev.aas.android.domain.InboxInteraction
import dev.aas.android.domain.InboxModel
import dev.aas.android.domain.InboxThread
import dev.aas.android.domain.ThreadActivity
import dev.aas.android.protocol.Interaction
import dev.aas.android.protocol.InteractionResolution
import dev.aas.android.protocol.ThreadId
import dev.aas.android.ui.common.UiText
import dev.aas.android.ui.common.UserMessages
import dev.aas.android.ui.common.aasViewModel
import dev.aas.android.ui.common.asString
import dev.aas.android.ui.components.EmptyState
import dev.aas.android.ui.components.SectionHeader
import dev.aas.android.ui.components.ThreadActivityChip
import dev.aas.android.ui.components.UnreadDot
import dev.aas.android.ui.components.relativeTime
import dev.aas.android.ui.icons.DoneAll
import dev.aas.android.ui.icons.MarkEmailRead
import dev.aas.android.ui.interaction.InteractionCard
import dev.aas.android.ui.interaction.QuestionSheet
import dev.aas.android.ui.navigation.AppNavigator
import dev.aas.android.ui.navigation.InboxRoute
import kotlinx.coroutines.CancellationException
import kotlinx.coroutines.ExperimentalCoroutinesApi
import kotlinx.coroutines.flow.SharingStarted
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.flow.combine
import kotlinx.coroutines.flow.mapLatest
import kotlinx.coroutines.flow.onStart
import kotlinx.coroutines.flow.stateIn
import kotlinx.coroutines.launch

fun NavGraphBuilder.inboxDestinations(navigator: AppNavigator) {
    composable<InboxRoute> {
        val vm = aasViewModel { c, _ -> InboxViewModel(c.workspaceRepository, c.interactionRepository, c.userMessages, c) }
        InboxScreen(vm, navigator)
    }
}

@OptIn(ExperimentalCoroutinesApi::class)
class InboxViewModel(
    private val workspace: WorkspaceRepository,
    private val interactions: InteractionRepository,
    private val messages: UserMessages,
    container: AppContainer,
) : ViewModel() {
    val inbox: StateFlow<InboxModel> = combine(workspace.inbox, workspace.interactionTasksKnown.onStart { emit(Unit) }) { model, _ -> model }
        .mapLatest { withTaskTitles(it) }
        .stateIn(viewModelScope, SharingStarted.WhileSubscribed(container.policy.uiStopTimeoutMs), InboxModel.Empty)

    /**
     * Names the background task that asked each approval or question, when this device stores
     * it (its thread was followed here). A failure of the local store is shown; the cards then
     * say only that background work asked.
     */
    private suspend fun withTaskTitles(model: InboxModel): InboxModel {
        val ids = model.interactions.mapNotNull { it.interaction.backgroundTaskId }
        if (ids.isEmpty()) return model
        val tasks = try {
            workspace.backgroundTasks(ids)
        } catch (e: CancellationException) {
            throw e
        } catch (e: Exception) {
            messages.show(UiText.of(R.string.error_local_store, e.message ?: e.javaClass.simpleName))
            emptyMap()
        }
        return model.copy(interactions = model.interactions.map { row -> row.copy(backgroundTaskTitle = row.interaction.backgroundTaskId?.let { tasks[it]?.title }) })
    }

    /** Queues the answer in the outbox (sent now, or as soon as the connection is back). */
    fun respond(interaction: Interaction, resolution: InteractionResolution) {
        viewModelScope.launch {
            try {
                interactions.respond(interaction, resolution)
            } catch (e: CancellationException) {
                throw e
            } catch (e: Exception) {
                messages.show(UiText.of(R.string.error_local_store, e.message ?: e.javaClass.simpleName))
            }
        }
    }

    fun markRead(threadId: ThreadId) {
        viewModelScope.launch {
            try {
                workspace.markViewed(threadId)
            } catch (e: CancellationException) {
                throw e
            } catch (e: Exception) {
                messages.show(UiText.of(R.string.error_local_store, e.message ?: e.javaClass.simpleName))
            }
        }
    }

    fun markAllRead(threads: List<InboxThread>) {
        for (row in threads) markRead(row.thread.id)
    }
}

/** 要対応: pending approvals and questions (answerable here), errors, running and unread threads. */
@OptIn(ExperimentalMaterial3Api::class)
@Composable
fun InboxScreen(vm: InboxViewModel, navigator: AppNavigator) {
    val inbox by vm.inbox.collectAsStateWithLifecycle()
    var questionFor by rememberSaveable { mutableStateOf<String?>(null) }
    Scaffold(
        topBar = {
            TopAppBar(
                title = { Text(stringResource(R.string.tab_inbox)) },
                actions = {
                    val readable = inbox.errors + inbox.unread
                    if (readable.isNotEmpty()) {
                        IconButton(onClick = { vm.markAllRead(readable) }) {
                            Icon(Icons.Outlined.DoneAll, contentDescription = stringResource(R.string.inbox_mark_all_read))
                        }
                    }
                },
            )
        },
    ) { padding ->
        if (inbox.isEmpty) {
            EmptyState(Icons.Outlined.DoneAll, stringResource(R.string.inbox_empty), Modifier.padding(padding))
            return@Scaffold
        }
        LazyColumn(Modifier.fillMaxSize(), contentPadding = padding) {
            if (inbox.interactions.isNotEmpty()) {
                item(key = "h-interactions") { SectionHeader(stringResource(R.string.inbox_section_waiting, inbox.interactions.size)) }
                items(inbox.interactions, key = { "i-" + it.interaction.id }) { row ->
                    InteractionCard(
                        interaction = row.interaction,
                        responsePending = row.responsePending,
                        onRespond = { vm.respond(row.interaction, it) },
                        onOpenQuestion = { questionFor = row.interaction.id },
                        modifier = Modifier.padding(horizontal = 16.dp, vertical = 6.dp),
                        header = { InteractionHeader(row) { navigator.openThread(row.interaction.threadId, row.interaction.id) } },
                        backgroundTaskTitle = row.backgroundTaskTitle,
                    )
                }
            }
            threadSection("errors", R.string.inbox_section_errors, inbox.errors, navigator, vm)
            threadSection("running", R.string.inbox_section_running, inbox.running, navigator, vm)
            threadSection("unread", R.string.inbox_section_unread, inbox.unread, navigator, vm)
            item(key = "bottom") { Spacer(Modifier.height(16.dp)) }
        }
    }
    val open = questionFor?.let { id -> inbox.interactions.firstOrNull { it.interaction.id == id } }
    if (open != null) {
        QuestionSheet(open.interaction, onRespond = { vm.respond(open.interaction, it) }, onDismiss = { questionFor = null })
    }
    // The question left the inbox (answered elsewhere or withdrawn): forget the open sheet.
    LaunchedEffect(questionFor, open == null) {
        if (questionFor != null && open == null) questionFor = null
    }
}

private fun LazyListScope.threadSection(
    key: String,
    title: Int,
    rows: List<InboxThread>,
    navigator: AppNavigator,
    vm: InboxViewModel,
) {
    if (rows.isEmpty()) return
    item(key = "h-$key") { SectionHeader(stringResource(title, rows.size)) }
    items(rows, key = { "$key-" + it.thread.id }) { row -> InboxThreadRow(row, onOpen = { navigator.openThread(row.thread.id) }, onMarkRead = { vm.markRead(row.thread.id) }) }
}

@Composable
private fun InteractionHeader(row: InboxInteraction, onOpenThread: () -> Unit) {
    Row(
        Modifier.fillMaxWidth().clickable(onClickLabel = stringResource(R.string.open_thread), onClick = onOpenThread).padding(bottom = 8.dp),
        verticalAlignment = Alignment.CenterVertically,
    ) {
        Column(Modifier.weight(1f)) {
            Text(row.thread?.title ?: stringResource(R.string.thread_unknown), style = MaterialTheme.typography.labelLarge, maxLines = 1, overflow = TextOverflow.Ellipsis)
            row.project?.let { Text(it.name, style = MaterialTheme.typography.labelSmall, color = MaterialTheme.colorScheme.onSurfaceVariant, maxLines = 1) }
        }
        Text(relativeTime(row.interaction.createdAt), style = MaterialTheme.typography.labelSmall, color = MaterialTheme.colorScheme.onSurfaceVariant)
    }
}

@Composable
private fun InboxThreadRow(row: InboxThread, onOpen: () -> Unit, onMarkRead: () -> Unit) {
    Row(
        Modifier.fillMaxWidth().clickable(onClick = onOpen).padding(start = 16.dp, end = 4.dp, top = 8.dp, bottom = 8.dp),
        verticalAlignment = Alignment.CenterVertically,
    ) {
        UnreadDot(row.unread)
        Spacer(Modifier.width(12.dp))
        Column(Modifier.weight(1f)) {
            Text(row.thread.title, style = MaterialTheme.typography.bodyLarge, maxLines = 1, overflow = TextOverflow.Ellipsis)
            Row(verticalAlignment = Alignment.CenterVertically) {
                ThreadActivityChip(row.activity, backgroundRunning = row.thread.background.running)
                Spacer(Modifier.width(8.dp))
                Text(
                    listOfNotNull(row.project?.name, relativeTime(row.thread.lastActivityAt)).joinToString(" · "),
                    style = MaterialTheme.typography.labelSmall,
                    color = MaterialTheme.colorScheme.onSurfaceVariant,
                    maxLines = 1,
                    overflow = TextOverflow.Ellipsis,
                )
            }
            row.thread.lastError?.takeIf { row.activity == ThreadActivity.Error }?.let {
                Text(ErrorTexts.turnErrorLine(it.kind, it.message).asString(), style = MaterialTheme.typography.bodySmall, color = MaterialTheme.colorScheme.error, maxLines = 2, overflow = TextOverflow.Ellipsis)
            }
        }
        if (row.unread) {
            IconButton(onClick = onMarkRead) { Icon(Icons.Outlined.MarkEmailRead, contentDescription = stringResource(R.string.mark_read)) }
        }
    }
}
