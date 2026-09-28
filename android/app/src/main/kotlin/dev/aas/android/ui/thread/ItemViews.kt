package dev.aas.android.ui.thread

import androidx.compose.animation.AnimatedVisibility
import androidx.compose.animation.animateContentSize
import androidx.compose.foundation.clickable
import androidx.compose.foundation.horizontalScroll
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.ExperimentalLayoutApi
import androidx.compose.foundation.layout.FlowRow
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.layout.widthIn
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.foundation.text.selection.SelectionContainer
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.outlined.Build
import androidx.compose.material.icons.outlined.CheckCircle
import androidx.compose.material.icons.outlined.Edit
import androidx.compose.material.icons.outlined.Info
import androidx.compose.material.icons.outlined.Search
import androidx.compose.material3.CircularProgressIndicator
import androidx.compose.material3.Icon
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Surface
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.runtime.Composable
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.saveable.rememberSaveable
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.graphics.vector.ImageVector
import androidx.compose.ui.platform.LocalResources
import androidx.compose.ui.res.stringResource
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.text.style.TextDecoration
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.unit.dp
import dev.aas.android.R
import dev.aas.android.domain.InteractionTexts
import dev.aas.android.domain.diff.UnifiedDiff
import dev.aas.android.protocol.AasJson
import dev.aas.android.protocol.Attachment
import dev.aas.android.protocol.BackgroundTask
import dev.aas.android.protocol.BackgroundTaskId
import dev.aas.android.protocol.BlobId
import dev.aas.android.protocol.FileChange
import dev.aas.android.protocol.FileChangeKind
import dev.aas.android.protocol.Item
import dev.aas.android.protocol.ItemStatus
import dev.aas.android.protocol.NoticeLevel
import dev.aas.android.protocol.PlanEntryStatus
import dev.aas.android.protocol.ToolCategory
import dev.aas.android.protocol.TurnId
import dev.aas.android.protocol.UserMessageDelivery
import dev.aas.android.ui.common.LocalAppPolicy
import dev.aas.android.ui.components.BlobImage
import dev.aas.android.ui.components.CopyIconButton
import dev.aas.android.ui.components.MarkdownText
import dev.aas.android.ui.diff.DiffHunkLines
import dev.aas.android.ui.icons.Article
import dev.aas.android.ui.icons.AutoAwesome
import dev.aas.android.ui.icons.Cloud
import dev.aas.android.ui.icons.Description
import dev.aas.android.ui.icons.ErrorOutline
import dev.aas.android.ui.icons.ExpandLess
import dev.aas.android.ui.icons.ExpandMore
import dev.aas.android.ui.icons.Groups
import dev.aas.android.ui.icons.Lightbulb
import dev.aas.android.ui.icons.PlayCircle
import dev.aas.android.ui.icons.RadioButtonUnchecked
import dev.aas.android.ui.icons.Terminal
import dev.aas.android.ui.icons.WarningAmber
import dev.aas.android.ui.theme.codeStyle
import dev.aas.android.ui.theme.statusColors
import kotlinx.serialization.json.Json
import kotlinx.serialization.json.JsonElement

/** What the item views can open. */
data class ItemActions(
    /** The full output of a command or tool call (also when it is a blob). */
    val onOpenOutput: (Item) -> Unit,
    /** The diff of a turn. */
    val onOpenTurnDiff: (TurnId) -> Unit,
    /** An image attachment in full. */
    val onOpenImage: (BlobId) -> Unit,
    /** The background task an item launched, in the thread's バックグラウンド section. */
    val onOpenBackgroundTask: (BackgroundTaskId) -> Unit = {},
) {
    companion object {
        val None = ItemActions({}, {}, {})
    }
}

/**
 * One item of the conversation, by kind (docs/ux/codex-desktop.md §3.2): the user's message
 * with its images, the agent's Markdown answer, folded reasoning, shell-like command cards with
 * streamed output, file changes with their diffs, tool calls, the plan checklist and notices.
 * An item whose work goes on in the background ([Item.backgroundTaskId]) carries a chip bound to
 * that task's live status ([backgroundTask], when loaded).
 */
@Composable
fun ItemView(item: Item, actions: ItemActions, modifier: Modifier = Modifier, backgroundTask: BackgroundTask? = null) {
    Column(modifier.fillMaxWidth().padding(horizontal = 12.dp, vertical = 4.dp)) {
        when (item) {
            is Item.UserMessage -> UserMessageView(item, actions)
            is Item.AgentMessage -> AgentMessageView(item)
            is Item.Reasoning -> ReasoningView(item)
            is Item.CommandExecution -> CommandView(item, actions)
            is Item.FileChangeItem -> FileChangeView(item, actions)
            is Item.ToolCall -> ToolCallView(item, actions)
            is Item.Plan -> PlanView(item)
            is Item.Notice -> NoticeView(item)
            is Item.Unknown -> Text(stringResource(R.string.item_unknown, item.kind), style = MaterialTheme.typography.bodyMedium, color = MaterialTheme.colorScheme.onSurfaceVariant)
        }
        ItemStatusLabel(item)
        item.backgroundTaskId?.let { taskId -> BackgroundChip(taskId, backgroundTask?.takeIf { it.id == taskId }, actions.onOpenBackgroundTask) }
    }
}

@Composable
private fun ItemStatusLabel(item: Item) {
    // Commands and file changes say it in their own words.
    if (item is Item.CommandExecution || item is Item.FileChangeItem) return
    val label = when (item.status) {
        ItemStatus.Failed -> R.string.item_failed
        ItemStatus.Declined -> R.string.item_declined
        ItemStatus.Interrupted -> R.string.item_interrupted
        // Backgrounded: the chip of its task says how the work goes on.
        else -> return
    }
    Text(stringResource(label), style = MaterialTheme.typography.labelSmall, color = MaterialTheme.statusColors.error)
}

@OptIn(ExperimentalLayoutApi::class)
@Composable
private fun UserMessageView(item: Item.UserMessage, actions: ItemActions) {
    Box(Modifier.fillMaxWidth(), contentAlignment = Alignment.CenterEnd) {
        Surface(
            color = MaterialTheme.colorScheme.primaryContainer,
            shape = RoundedCornerShape(topStart = 16.dp, topEnd = 4.dp, bottomStart = 16.dp, bottomEnd = 16.dp),
            modifier = Modifier.widthIn(max = BUBBLE_MAX_WIDTH),
        ) {
            Column(Modifier.padding(horizontal = 14.dp, vertical = 10.dp)) {
                if (item.delivery == UserMessageDelivery.Steer) {
                    Text(stringResource(R.string.item_steered), style = MaterialTheme.typography.labelSmall, color = MaterialTheme.colorScheme.onPrimaryContainer.copy(alpha = SECONDARY_ALPHA))
                }
                if (item.text.isNotEmpty()) {
                    SelectionContainer { Text(item.text, style = MaterialTheme.typography.bodyLarge, color = MaterialTheme.colorScheme.onPrimaryContainer) }
                }
                val images = item.attachments.filterIsInstance<Attachment.Image>()
                if (images.isNotEmpty()) {
                    Spacer(Modifier.height(6.dp))
                    FlowRow(horizontalArrangement = Arrangement.spacedBy(6.dp), verticalArrangement = Arrangement.spacedBy(6.dp)) {
                        images.forEach { image -> BlobImage(image.blobId, THUMBNAIL_EDGE_PX, THUMBNAIL_SIZE, onClick = { actions.onOpenImage(image.blobId) }) }
                    }
                }
                val unknown = item.attachments.count { it !is Attachment.Image }
                if (unknown > 0) Text(stringResource(R.string.item_unknown_attachments, unknown), style = MaterialTheme.typography.labelSmall)
                if (item.mentions.isNotEmpty()) {
                    Spacer(Modifier.height(4.dp))
                    FlowRow(horizontalArrangement = Arrangement.spacedBy(4.dp)) {
                        item.mentions.forEach { m ->
                            Surface(shape = RoundedCornerShape(50), color = MaterialTheme.colorScheme.surface.copy(alpha = SECONDARY_ALPHA)) {
                                Text("@${m.path}", style = MaterialTheme.typography.labelSmall, maxLines = 1, overflow = TextOverflow.Ellipsis, modifier = Modifier.padding(horizontal = 8.dp, vertical = 2.dp))
                            }
                        }
                    }
                }
            }
        }
    }
}

@Composable
private fun AgentMessageView(item: Item.AgentMessage) {
    SelectionContainer { MarkdownText(item.text) }
    if (item.status != ItemStatus.InProgress && item.text.isNotEmpty()) {
        Row(Modifier.fillMaxWidth(), horizontalArrangement = Arrangement.End) { CopyIconButton(item.text, R.string.copied_message) }
    }
}

@Composable
private fun ReasoningView(item: Item.Reasoning) {
    val res = LocalResources.current
    var open by rememberSaveable(item.id) { mutableStateOf(false) }
    val completedAt = item.completedAt
    val title = when {
        item.status == ItemStatus.InProgress -> stringResource(R.string.item_reasoning_running)
        completedAt != null -> stringResource(R.string.item_reasoning_time, InteractionTexts.duration(res, completedAt - item.startedAt))
        else -> stringResource(R.string.item_reasoning_done)
    }
    Column(Modifier.fillMaxWidth().animateContentSize()) {
        FoldHeader(Icons.Outlined.Lightbulb, title, open, running = item.status == ItemStatus.InProgress) { open = !open }
        AnimatedVisibility(open) {
            if (item.text.isBlank()) {
                Text(stringResource(R.string.item_reasoning_empty), style = MaterialTheme.typography.bodySmall, color = MaterialTheme.colorScheme.onSurfaceVariant, modifier = Modifier.padding(start = 28.dp))
            } else {
                SelectionContainer {
                    MarkdownText(item.text, Modifier.padding(start = 28.dp), style = MaterialTheme.typography.bodyMedium.copy(color = MaterialTheme.colorScheme.onSurfaceVariant))
                }
            }
        }
    }
}

/** A one-line header that folds its content. */
@Composable
fun FoldHeader(icon: ImageVector, title: String, open: Boolean, running: Boolean = false, trailing: String? = null, onToggle: () -> Unit) {
    Row(Modifier.fillMaxWidth().clickable(onClick = onToggle).padding(vertical = 6.dp), verticalAlignment = Alignment.CenterVertically) {
        if (running) {
            CircularProgressIndicator(Modifier.size(18.dp), strokeWidth = 2.dp)
        } else {
            Icon(icon, contentDescription = null, tint = MaterialTheme.colorScheme.onSurfaceVariant, modifier = Modifier.size(18.dp))
        }
        Spacer(Modifier.width(10.dp))
        Text(title, style = MaterialTheme.typography.labelLarge, color = MaterialTheme.colorScheme.onSurfaceVariant, modifier = Modifier.weight(1f), maxLines = 2, overflow = TextOverflow.Ellipsis)
        trailing?.let { Text(it, style = MaterialTheme.typography.labelSmall, color = MaterialTheme.colorScheme.onSurfaceVariant) }
        Icon(
            if (open) Icons.Outlined.ExpandLess else Icons.Outlined.ExpandMore,
            contentDescription = stringResource(if (open) R.string.collapse else R.string.expand),
            tint = MaterialTheme.colorScheme.onSurfaceVariant,
        )
    }
}

@Composable
private fun CommandView(item: Item.CommandExecution, actions: ItemActions) {
    val res = LocalResources.current
    val display = LocalAppPolicy.current.display
    val running = item.status == ItemStatus.InProgress
    var open by rememberSaveable(item.id) { mutableStateOf(false) }
    val lines = remember(item.output) { item.output.trimEnd('\n').let { if (it.isEmpty()) emptyList() else it.split('\n') } }
    Surface(color = MaterialTheme.colorScheme.surfaceContainer, shape = RoundedCornerShape(10.dp), modifier = Modifier.fillMaxWidth()) {
        Column(Modifier.animateContentSize()) {
            Row(Modifier.fillMaxWidth().clickable { open = !open }.padding(start = 12.dp, top = 8.dp, bottom = 8.dp, end = 4.dp), verticalAlignment = Alignment.Top) {
                if (running) CircularProgressIndicator(Modifier.padding(top = 2.dp).size(16.dp), strokeWidth = 2.dp) else Icon(Icons.Outlined.Terminal, null, Modifier.size(18.dp), tint = MaterialTheme.colorScheme.onSurfaceVariant)
                Spacer(Modifier.width(8.dp))
                Column(Modifier.weight(1f)) {
                    Text("$ ${item.command}", style = MaterialTheme.codeStyle, maxLines = if (open) Int.MAX_VALUE else display.commandCollapsedLines, overflow = TextOverflow.Ellipsis)
                    val status = commandStatus(item)
                    val details = listOfNotNull(status.first, item.durationMs?.let { InteractionTexts.duration(res, it) }).joinToString(" · ")
                    Text(details, style = MaterialTheme.typography.labelSmall, color = status.second ?: MaterialTheme.colorScheme.onSurfaceVariant)
                }
                Icon(if (open) Icons.Outlined.ExpandLess else Icons.Outlined.ExpandMore, contentDescription = stringResource(if (open) R.string.collapse else R.string.expand), tint = MaterialTheme.colorScheme.onSurfaceVariant)
            }
            val shown = when {
                open -> lines.takeLast(display.outputExpandedLines)
                running -> lines.takeLast(display.outputRunningLines)
                else -> emptyList()
            }
            if (shown.isNotEmpty()) OutputLines(shown)
            if (open) {
                item.cwd?.let { Text(stringResource(R.string.subject_cwd, it), style = MaterialTheme.typography.labelSmall, color = MaterialTheme.colorScheme.onSurfaceVariant, modifier = Modifier.padding(horizontal = 12.dp)) }
                if (lines.isEmpty() && !running) {
                    Text(
                        stringResource(if (item.outputBlobId != null) R.string.item_output_in_blob else R.string.item_no_output),
                        style = MaterialTheme.typography.bodySmall,
                        color = MaterialTheme.colorScheme.onSurfaceVariant,
                        modifier = Modifier.padding(horizontal = 12.dp, vertical = 4.dp),
                    )
                }
                if (item.outputTruncated) Text(stringResource(R.string.item_output_truncated), style = MaterialTheme.typography.labelSmall, color = MaterialTheme.colorScheme.onSurfaceVariant, modifier = Modifier.padding(horizontal = 12.dp))
                Row(Modifier.fillMaxWidth().padding(horizontal = 4.dp), verticalAlignment = Alignment.CenterVertically) {
                    if (lines.size > display.outputExpandedLines || item.outputTruncated || item.outputBlobId != null) {
                        TextButton(onClick = { actions.onOpenOutput(item) }) { Text(stringResource(R.string.item_show_full_output)) }
                    }
                    Spacer(Modifier.weight(1f))
                    CopyIconButton(item.command, R.string.copied_command)
                }
            }
        }
    }
}

/** The status words of a command and their colour (`null`: the default). */
@Composable
private fun commandStatus(item: Item.CommandExecution): Pair<String, Color?> = when (item.status) {
    ItemStatus.InProgress -> stringResource(R.string.item_command_running) to MaterialTheme.statusColors.running
    ItemStatus.Completed -> when (val code = item.exitCode) {
        null, 0 -> stringResource(R.string.item_command_done) to null
        else -> stringResource(R.string.item_command_exit, code) to MaterialTheme.statusColors.error
    }
    ItemStatus.Failed -> (item.exitCode?.let { stringResource(R.string.item_command_exit, it) } ?: stringResource(R.string.item_failed)) to MaterialTheme.statusColors.error
    ItemStatus.Declined -> stringResource(R.string.item_declined) to MaterialTheme.statusColors.error
    ItemStatus.Interrupted -> stringResource(R.string.item_command_stopped) to MaterialTheme.statusColors.needsApproval
    ItemStatus.Backgrounded -> stringResource(R.string.item_backgrounded) to MaterialTheme.statusColors.running
    ItemStatus.Unknown -> stringResource(R.string.item_command_done) to null
}

@Composable
private fun OutputLines(lines: List<String>) {
    Text(
        lines.joinToString("\n"),
        style = MaterialTheme.codeStyle,
        softWrap = false,
        color = MaterialTheme.colorScheme.onSurfaceVariant,
        modifier = Modifier.fillMaxWidth().horizontalScroll(rememberScrollState()).padding(horizontal = 12.dp, vertical = 4.dp),
    )
}

@Composable
private fun FileChangeView(item: Item.FileChangeItem, actions: ItemActions) {
    Surface(color = MaterialTheme.colorScheme.surfaceContainer, shape = RoundedCornerShape(10.dp), modifier = Modifier.fillMaxWidth()) {
        Column(Modifier.padding(vertical = 6.dp)) {
            Row(Modifier.padding(horizontal = 12.dp, vertical = 4.dp), verticalAlignment = Alignment.CenterVertically) {
                if (item.status == ItemStatus.InProgress) CircularProgressIndicator(Modifier.size(16.dp), strokeWidth = 2.dp) else Icon(Icons.Outlined.Edit, null, Modifier.size(18.dp), tint = MaterialTheme.colorScheme.onSurfaceVariant)
                Spacer(Modifier.width(8.dp))
                val headline = when (item.status) {
                    ItemStatus.Declined -> stringResource(R.string.item_files_declined, item.changes.size)
                    ItemStatus.Interrupted -> stringResource(R.string.item_files_stopped, item.changes.size)
                    ItemStatus.Failed -> stringResource(R.string.item_files_failed, item.changes.size)
                    ItemStatus.InProgress -> stringResource(R.string.item_files_editing, item.changes.size)
                    else -> stringResource(R.string.item_files_changed, item.changes.size)
                }
                Text(headline, style = MaterialTheme.typography.labelLarge, modifier = Modifier.weight(1f))
            }
            item.changes.forEachIndexed { index, change -> FileChangeRow(item.id, index, change) }
            if (item.status == ItemStatus.Completed) {
                TextButton(onClick = { actions.onOpenTurnDiff(item.turnId) }, modifier = Modifier.padding(horizontal = 4.dp)) { Text(stringResource(R.string.item_view_turn_diff)) }
            }
        }
    }
}

@Composable
private fun FileChangeRow(itemId: String, index: Int, change: FileChange) {
    val display = LocalAppPolicy.current.display
    var open by rememberSaveable("$itemId/$index") { mutableStateOf(false) }
    val hunks = remember(change.diff) { change.diff?.let { UnifiedDiff.parseHunks(it) }.orEmpty() }
    Column(Modifier.fillMaxWidth().animateContentSize()) {
        Row(
            Modifier.fillMaxWidth().clickable(enabled = hunks.isNotEmpty()) { open = !open }.padding(horizontal = 12.dp, vertical = 6.dp),
            verticalAlignment = Alignment.CenterVertically,
        ) {
            Text(fileKindLabel(change.kind), style = MaterialTheme.typography.labelSmall, color = fileKindColor(change.kind), modifier = Modifier.width(FILE_KIND_WIDTH))
            Text(
                change.path + (change.movePath?.let { " → $it" } ?: ""),
                style = MaterialTheme.codeStyle,
                maxLines = 2,
                overflow = TextOverflow.Ellipsis,
                modifier = Modifier.weight(1f),
            )
            change.added?.let { Text("+$it", style = MaterialTheme.typography.labelSmall, color = MaterialTheme.statusColors.connected) }
            change.removed?.let {
                Spacer(Modifier.width(4.dp))
                Text("−$it", style = MaterialTheme.typography.labelSmall, color = MaterialTheme.statusColors.error)
            }
            if (hunks.isNotEmpty()) Icon(if (open) Icons.Outlined.ExpandLess else Icons.Outlined.ExpandMore, contentDescription = stringResource(if (open) R.string.collapse else R.string.expand), tint = MaterialTheme.colorScheme.onSurfaceVariant)
        }
        if (open) DiffHunkLines(hunks, maxLines = display.inlineDiffLines, modifier = Modifier.padding(horizontal = 8.dp))
    }
}

@Composable
fun fileKindLabel(kind: FileChangeKind): String = stringResource(
    when (kind) {
        FileChangeKind.Add -> R.string.file_add
        FileChangeKind.Update -> R.string.file_update
        FileChangeKind.Delete -> R.string.file_delete
        FileChangeKind.Move -> R.string.file_move
        FileChangeKind.Unknown -> R.string.file_changed
    },
)

@Composable
fun fileKindColor(kind: FileChangeKind): Color = when (kind) {
    FileChangeKind.Add -> MaterialTheme.statusColors.connected
    FileChangeKind.Delete -> MaterialTheme.statusColors.error
    FileChangeKind.Move -> MaterialTheme.statusColors.needsInput
    FileChangeKind.Update, FileChangeKind.Unknown -> MaterialTheme.statusColors.running
}

@Composable
private fun ToolCallView(item: Item.ToolCall, actions: ItemActions) {
    val display = LocalAppPolicy.current.display
    var open by rememberSaveable(item.id) { mutableStateOf(false) }
    val running = item.status == ItemStatus.InProgress
    val title = item.title.ifEmpty { item.name }
    val subtitle = listOfNotNull(toolCategoryLabel(item.category), item.server?.let { stringResource(R.string.item_tool_server, it) }).joinToString(" · ")
    Surface(color = MaterialTheme.colorScheme.surfaceContainer, shape = RoundedCornerShape(10.dp), modifier = Modifier.fillMaxWidth()) {
        Column(Modifier.animateContentSize().padding(horizontal = 12.dp, vertical = 4.dp)) {
            FoldHeader(toolIcon(item.category), title, open, running = running, trailing = subtitle) { open = !open }
            if (open) {
                item.input?.let {
                    Text(stringResource(R.string.item_tool_input), style = MaterialTheme.typography.labelSmall, color = MaterialTheme.colorScheme.onSurfaceVariant)
                    val pretty = remember(it) { prettyJson(it) }
                    Text(pretty.lines().take(display.toolInputLines).joinToString("\n"), style = MaterialTheme.codeStyle, softWrap = false, modifier = Modifier.horizontalScroll(rememberScrollState()).padding(vertical = 4.dp))
                }
                val output = item.output.orEmpty()
                if (output.isNotEmpty()) {
                    Text(stringResource(R.string.item_tool_output), style = MaterialTheme.typography.labelSmall, color = MaterialTheme.colorScheme.onSurfaceVariant)
                    val outLines = remember(output) { output.trimEnd('\n').split('\n') }
                    Text(outLines.takeLast(display.outputExpandedLines).joinToString("\n"), style = MaterialTheme.codeStyle, softWrap = false, modifier = Modifier.horizontalScroll(rememberScrollState()).padding(vertical = 4.dp))
                    if (outLines.size > display.outputExpandedLines || item.outputTruncated || item.outputBlobId != null) {
                        TextButton(onClick = { actions.onOpenOutput(item) }) { Text(stringResource(R.string.item_show_full_output)) }
                    }
                } else if (item.outputBlobId != null) {
                    TextButton(onClick = { actions.onOpenOutput(item) }) { Text(stringResource(R.string.item_show_full_output)) }
                }
            } else if (running && !item.output.isNullOrEmpty()) {
                Text(item.output.orEmpty().trimEnd('\n').split('\n').takeLast(display.outputRunningLines).joinToString("\n"), style = MaterialTheme.codeStyle, softWrap = false, color = MaterialTheme.colorScheme.onSurfaceVariant, modifier = Modifier.horizontalScroll(rememberScrollState()))
            }
        }
    }
}

@Composable
fun toolCategoryLabel(category: ToolCategory): String = stringResource(
    when (category) {
        ToolCategory.Read -> R.string.tool_read
        ToolCategory.Search -> R.string.tool_search
        ToolCategory.Fetch -> R.string.tool_fetch
        ToolCategory.Mcp -> R.string.tool_mcp
        ToolCategory.Subagent -> R.string.tool_subagent
        ToolCategory.Edit -> R.string.tool_edit
        ToolCategory.Execute -> R.string.tool_execute
        ToolCategory.Think -> R.string.tool_think
        ToolCategory.Other, ToolCategory.Unknown -> R.string.tool_other
    },
)

private fun toolIcon(category: ToolCategory): ImageVector = when (category) {
    ToolCategory.Read -> Icons.AutoMirrored.Outlined.Article
    ToolCategory.Search -> Icons.Outlined.Search
    ToolCategory.Fetch -> Icons.Outlined.Cloud
    ToolCategory.Mcp -> Icons.Outlined.Build
    ToolCategory.Subagent -> Icons.Outlined.Groups
    ToolCategory.Edit -> Icons.Outlined.Edit
    ToolCategory.Execute -> Icons.Outlined.Terminal
    ToolCategory.Think -> Icons.Outlined.Lightbulb
    ToolCategory.Other, ToolCategory.Unknown -> Icons.Outlined.AutoAwesome
}

@Composable
fun PlanView(item: Item.Plan, modifier: Modifier = Modifier) {
    val done = item.entries.count { it.status == PlanEntryStatus.Completed }
    Surface(color = MaterialTheme.colorScheme.surfaceContainer, shape = RoundedCornerShape(10.dp), modifier = modifier.fillMaxWidth()) {
        Column(Modifier.padding(12.dp)) {
            Row(verticalAlignment = Alignment.CenterVertically) {
                Icon(Icons.Outlined.Description, null, Modifier.size(18.dp), tint = MaterialTheme.colorScheme.onSurfaceVariant)
                Spacer(Modifier.width(8.dp))
                Text(stringResource(R.string.item_plan), style = MaterialTheme.typography.labelLarge, modifier = Modifier.weight(1f))
                Text(stringResource(R.string.item_plan_progress, done, item.entries.size), style = MaterialTheme.typography.labelSmall, color = MaterialTheme.colorScheme.onSurfaceVariant)
            }
            Spacer(Modifier.height(6.dp))
            item.entries.forEach { entry ->
                Row(Modifier.padding(vertical = 3.dp), verticalAlignment = Alignment.Top) {
                    val (icon, tint) = when (entry.status) {
                        PlanEntryStatus.Completed -> Icons.Outlined.CheckCircle to MaterialTheme.statusColors.connected
                        PlanEntryStatus.InProgress -> Icons.Outlined.PlayCircle to MaterialTheme.statusColors.running
                        PlanEntryStatus.Pending, PlanEntryStatus.Unknown -> Icons.Outlined.RadioButtonUnchecked to MaterialTheme.colorScheme.onSurfaceVariant
                    }
                    Icon(icon, contentDescription = planStatusLabel(entry.status), tint = tint, modifier = Modifier.size(18.dp))
                    Spacer(Modifier.width(8.dp))
                    Text(
                        entry.text,
                        style = MaterialTheme.typography.bodyMedium.copy(
                            fontWeight = if (entry.status == PlanEntryStatus.InProgress) FontWeight.SemiBold else FontWeight.Normal,
                            textDecoration = if (entry.status == PlanEntryStatus.Completed) TextDecoration.LineThrough else TextDecoration.None,
                        ),
                    )
                }
            }
        }
    }
}

@Composable
private fun planStatusLabel(status: PlanEntryStatus): String = stringResource(
    when (status) {
        PlanEntryStatus.Completed -> R.string.plan_completed
        PlanEntryStatus.InProgress -> R.string.plan_in_progress
        PlanEntryStatus.Pending, PlanEntryStatus.Unknown -> R.string.plan_pending
    },
)

@Composable
private fun NoticeView(item: Item.Notice) {
    val (icon, color, label) = when (item.level) {
        NoticeLevel.Error -> Triple(Icons.Outlined.ErrorOutline, MaterialTheme.statusColors.error, R.string.notice_error)
        NoticeLevel.Warning -> Triple(Icons.Outlined.WarningAmber, MaterialTheme.statusColors.needsApproval, R.string.notice_warning)
        NoticeLevel.Info, NoticeLevel.Unknown -> Triple(Icons.Outlined.Info, MaterialTheme.colorScheme.onSurfaceVariant, R.string.notice_info)
    }
    Row(Modifier.fillMaxWidth().padding(vertical = 4.dp), verticalAlignment = Alignment.Top) {
        Icon(icon, contentDescription = stringResource(label), tint = color, modifier = Modifier.size(18.dp))
        Spacer(Modifier.width(8.dp))
        Column(Modifier.weight(1f)) {
            SelectionContainer { Text(item.message, style = MaterialTheme.typography.bodyMedium, color = if (item.level == NoticeLevel.Info) MaterialTheme.colorScheme.onSurfaceVariant else color) }
            item.code?.let { Text(it, style = MaterialTheme.typography.labelSmall, color = MaterialTheme.colorScheme.onSurfaceVariant) }
        }
    }
}

private fun prettyJson(element: JsonElement): String = PrettyJson.encodeToString(JsonElement.serializer(), element)

private val PrettyJson = Json(from = AasJson) { prettyPrint = true }

/** Widest a user bubble grows (the rest of the row stays free, like a chat). */
private val BUBBLE_MAX_WIDTH = 320.dp

private val THUMBNAIL_SIZE = 96.dp

/** Decoded size of attachment thumbnails: 96 dp at up to 3x density. */
private const val THUMBNAIL_EDGE_PX = 288

private const val SECONDARY_ALPHA = 0.72f

private val FILE_KIND_WIDTH = 40.dp
