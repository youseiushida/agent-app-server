package dev.aas.android.ui.thread

import androidx.compose.foundation.clickable
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.layout.widthIn
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.outlined.CheckCircle
import androidx.compose.material3.CircularProgressIndicator
import androidx.compose.material3.HorizontalDivider
import androidx.compose.material3.Icon
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.OutlinedButton
import androidx.compose.material3.Surface
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.runtime.Composable
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.platform.LocalResources
import androidx.compose.ui.res.stringResource
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.unit.dp
import dev.aas.android.R
import dev.aas.android.domain.InteractionTexts
import dev.aas.android.domain.timeline.PendingInput
import dev.aas.android.protocol.Delivery
import dev.aas.android.protocol.ExpireReason
import dev.aas.android.protocol.Interaction
import dev.aas.android.protocol.InteractionRequest
import dev.aas.android.protocol.InteractionResolution
import dev.aas.android.protocol.InteractionStatus
import dev.aas.android.protocol.Item
import dev.aas.android.protocol.ItemStatus
import dev.aas.android.protocol.Turn
import dev.aas.android.protocol.TurnStatus
import dev.aas.android.protocol.Usage
import dev.aas.android.ui.common.LocalAppPolicy
import dev.aas.android.ui.components.rememberNow
import dev.aas.android.ui.icons.ExpandLess
import dev.aas.android.ui.icons.ExpandMore
import dev.aas.android.ui.icons.HelpOutline
import dev.aas.android.ui.icons.HourglassEmpty
import dev.aas.android.ui.icons.Schedule
import dev.aas.android.ui.icons.Timer
import dev.aas.android.ui.icons.Unarchive
import dev.aas.android.ui.interaction.ApprovalChoices
import dev.aas.android.ui.theme.statusColors
import java.util.Locale

/** The start of a turn: a divider with its number and the model the CLI reported. */
@Composable
fun TurnStartRow(turn: Turn) {
    Row(Modifier.fillMaxWidth().padding(horizontal = 16.dp, vertical = 8.dp), verticalAlignment = Alignment.CenterVertically) {
        HorizontalDivider(Modifier.weight(1f))
        Text(
            listOfNotNull(stringResource(R.string.turn_number, turn.index + 1), turn.model).joinToString(" · "),
            style = MaterialTheme.typography.labelSmall,
            color = MaterialTheme.colorScheme.onSurfaceVariant,
            modifier = Modifier.padding(horizontal = 8.dp),
            maxLines = 1,
            overflow = TextOverflow.Ellipsis,
        )
        HorizontalDivider(Modifier.weight(1f))
    }
}

/**
 * The end of a turn (docs/ux/codex-desktop.md §3.1 ターン末尾のまとめ): how it ended and after how
 * long ("{time}間作業しました" / "{time}後に停止しました"), its token usage, its error, and the
 * files it changed with the way into its diff.
 */
@Composable
fun TurnEndRow(turn: Turn, onOpenDiff: () -> Unit) {
    val res = LocalResources.current
    val duration = turn.completedAt?.let { InteractionTexts.duration(res, it - turn.startedAt) }
    val (text, color) = when (turn.status) {
        TurnStatus.Completed -> (duration?.let { stringResource(R.string.turn_worked, it) } ?: stringResource(R.string.turn_completed)) to MaterialTheme.colorScheme.onSurfaceVariant
        TurnStatus.Interrupted -> (duration?.let { stringResource(R.string.turn_stopped_after, it) } ?: stringResource(R.string.turn_interrupted)) to MaterialTheme.statusColors.needsApproval
        TurnStatus.Failed -> (duration?.let { stringResource(R.string.turn_failed_after, it) } ?: stringResource(R.string.turn_failed)) to MaterialTheme.statusColors.error
        TurnStatus.Running, TurnStatus.Unknown -> stringResource(R.string.turn_ended) to MaterialTheme.colorScheme.onSurfaceVariant
    }
    Column(Modifier.fillMaxWidth().padding(horizontal = 16.dp, vertical = 6.dp)) {
        Row(verticalAlignment = Alignment.CenterVertically) {
            Icon(if (turn.status == TurnStatus.Completed) Icons.Outlined.CheckCircle else Icons.Outlined.Timer, null, Modifier.size(16.dp), tint = color)
            Spacer(Modifier.width(6.dp))
            Text(text, style = MaterialTheme.typography.labelMedium, color = color)
        }
        turn.error?.let { Text(it.message, style = MaterialTheme.typography.bodySmall, color = MaterialTheme.statusColors.error) }
        turn.usage?.let { usage -> Text(usageText(usage), style = MaterialTheme.typography.labelSmall, color = MaterialTheme.colorScheme.onSurfaceVariant) }
        val diff = turn.diff
        if (diff != null && diff.files > 0) {
            Row(verticalAlignment = Alignment.CenterVertically) {
                Text(stringResource(R.string.turn_diff, diff.files, diff.insertions, diff.deletions), style = MaterialTheme.typography.labelMedium, modifier = Modifier.weight(1f))
                TextButton(onClick = onOpenDiff) { Text(stringResource(R.string.turn_view_diff)) }
            }
        }
    }
}

/** "入力 12.3k · 出力 1.2k · キャッシュ 8k · 推論 300 トークン · $0.12". */
@Composable
fun usageText(usage: Usage): String {
    val parts = mutableListOf(stringResource(R.string.usage_input, compactNumber(usage.inputTokens)), stringResource(R.string.usage_output, compactNumber(usage.outputTokens)))
    if (usage.cachedInputTokens > 0) parts += stringResource(R.string.usage_cached, compactNumber(usage.cachedInputTokens))
    if (usage.reasoningTokens > 0) parts += stringResource(R.string.usage_reasoning, compactNumber(usage.reasoningTokens))
    val tokens = stringResource(R.string.usage_tokens, parts.joinToString(" · "))
    return usage.costUsd?.let { tokens + " · " + String.format(Locale.ROOT, "$%.2f", it) } ?: tokens
}

/** 950 → "950", 12_345 → "12.3k", 2_500_000 → "2.5M". */
fun compactNumber(n: Long): String = when {
    n < THOUSAND -> n.toString()
    n < MILLION -> String.format(Locale.ROOT, "%.1fk", n / THOUSAND.toDouble())
    else -> String.format(Locale.ROOT, "%.1fM", n / MILLION.toDouble())
}

/** The running turn: 作業中 with the elapsed time and what it is doing (UX §3.1 作業中の表示). */
@Composable
fun WorkingRow(turn: Turn, current: Item?, waitingForAnswer: Boolean) {
    val res = LocalResources.current
    val now = rememberNow(LocalAppPolicy.current.display.workingTickMs)
    val elapsed = InteractionTexts.duration(res, (now - turn.startedAt).coerceAtLeast(0))
    val doing = when {
        waitingForAnswer -> stringResource(R.string.working_waiting_answer)
        current is Item.CommandExecution -> stringResource(R.string.working_command, current.command.lineSequence().firstOrNull().orEmpty())
        current is Item.FileChangeItem -> stringResource(R.string.working_editing)
        current is Item.ToolCall -> current.title.ifEmpty { current.name }
        current is Item.Reasoning -> stringResource(R.string.working_thinking)
        current is Item.AgentMessage -> stringResource(R.string.working_writing)
        else -> null
    }
    Row(Modifier.fillMaxWidth().padding(horizontal = 16.dp, vertical = 8.dp), verticalAlignment = Alignment.CenterVertically) {
        CircularProgressIndicator(Modifier.size(16.dp), strokeWidth = 2.dp)
        Spacer(Modifier.width(10.dp))
        Column {
            Text(stringResource(R.string.working, elapsed), style = MaterialTheme.typography.labelLarge, color = MaterialTheme.statusColors.running)
            doing?.let { Text(it, style = MaterialTheme.typography.bodySmall, color = MaterialTheme.colorScheme.onSurfaceVariant, maxLines = 1, overflow = TextOverflow.Ellipsis) }
        }
    }
}

/** The folded summary of consecutive activity ("コマンド 3 · ファイルの編集 1 · ツール 2"). */
@Composable
fun ActivityGroupRow(items: List<Item>, expanded: Boolean, onToggle: () -> Unit) {
    val commands = items.count { it is Item.CommandExecution }
    val edits = items.count { it is Item.FileChangeItem }
    val tools = items.count { it is Item.ToolCall }
    val thoughts = items.count { it is Item.Reasoning }
    val parts = buildList {
        if (commands > 0) add(stringResource(R.string.group_commands, commands))
        if (edits > 0) add(stringResource(R.string.group_edits, edits))
        if (tools > 0) add(stringResource(R.string.group_tools, tools))
        if (thoughts > 0) add(stringResource(R.string.group_reasoning, thoughts))
    }
    val running = items.any { it.status == ItemStatus.InProgress }
    val failed = items.count { it.status == ItemStatus.Failed || it.status == ItemStatus.Declined }
    Row(
        Modifier.fillMaxWidth().clickable(onClick = onToggle).padding(horizontal = 16.dp, vertical = 8.dp),
        verticalAlignment = Alignment.CenterVertically,
    ) {
        if (running) CircularProgressIndicator(Modifier.size(16.dp), strokeWidth = 2.dp) else Icon(Icons.Outlined.HourglassEmpty, null, Modifier.size(16.dp), tint = MaterialTheme.colorScheme.onSurfaceVariant)
        Spacer(Modifier.width(10.dp))
        Column(Modifier.weight(1f)) {
            Text(stringResource(R.string.group_title, items.size), style = MaterialTheme.typography.labelLarge, color = MaterialTheme.colorScheme.onSurfaceVariant)
            Text(parts.joinToString(" · "), style = MaterialTheme.typography.labelSmall, color = MaterialTheme.colorScheme.onSurfaceVariant)
            if (failed > 0) Text(stringResource(R.string.group_failed, failed), style = MaterialTheme.typography.labelSmall, color = MaterialTheme.statusColors.error)
        }
        Icon(if (expanded) Icons.Outlined.ExpandLess else Icons.Outlined.ExpandMore, contentDescription = stringResource(if (expanded) R.string.collapse else R.string.expand), tint = MaterialTheme.colorScheme.onSurfaceVariant)
    }
}

/**
 * A closed approval or question as one line (UX §4.4: "質問済み · n 件の質問", "回答が提供されて
 * いません"): what was asked and how it ended.
 */
@Composable
fun InteractionRecordRow(interaction: Interaction) {
    val request = interaction.request
    val title = request.title.ifEmpty { stringResource(R.string.thread_unknown) }
    val text = when (interaction.status) {
        InteractionStatus.Expired -> stringResource(R.string.record_expired, title, expireReasonLabel(interaction.expireReason))
        else -> when (val resolution = interaction.resolution) {
            InteractionResolution.Dismissed -> stringResource(R.string.record_dismissed, title)
            is InteractionResolution.Approval -> {
                val option = (request as? InteractionRequest.Approval)?.options?.firstOrNull { it.id == resolution.optionId }
                val choice = option?.let { ApprovalChoices.label(it) } ?: resolution.optionId
                stringResource(R.string.record_approval, title, choice) + (resolution.feedback?.let { "\n" + stringResource(R.string.record_feedback, it) } ?: "")
            }
            is InteractionResolution.Question -> stringResource(R.string.record_question, (request as? InteractionRequest.Question)?.questions?.size ?: resolution.answers.size)
            is InteractionResolution.Unknown, null -> stringResource(R.string.record_closed, title)
        }
    }
    Row(Modifier.fillMaxWidth().padding(horizontal = 16.dp, vertical = 6.dp), verticalAlignment = Alignment.Top) {
        Icon(
            if (request is InteractionRequest.Question) Icons.AutoMirrored.Outlined.HelpOutline else Icons.Outlined.CheckCircle,
            null,
            Modifier.size(16.dp),
            tint = MaterialTheme.colorScheme.onSurfaceVariant,
        )
        Spacer(Modifier.width(8.dp))
        Text(text, style = MaterialTheme.typography.bodySmall, color = MaterialTheme.colorScheme.onSurfaceVariant)
    }
}

@Composable
fun expireReasonLabel(reason: ExpireReason?): String = stringResource(
    when (reason) {
        ExpireReason.ProcessExited -> R.string.expire_process_exited
        ExpireReason.TurnEnded -> R.string.expire_turn_ended
        ExpireReason.HarnessCancelled -> R.string.expire_harness_cancelled
        ExpireReason.DaemonRestarted -> R.string.expire_daemon_restarted
        ExpireReason.Unknown, null -> R.string.expire_unknown
    },
)

/**
 * A message in the outbox: shown where it will appear, marked as waiting (offline) or sending,
 * with the retries so far. It is replaced by the real message when the daemon has it. A message
 * waiting for its harness ([harnessName], the server answered `harnessUnavailable`) says so,
 * with the server's reason and 再確認, instead of counting retries.
 */
@Composable
fun PendingInputRow(
    input: PendingInput,
    online: Boolean,
    onDiscard: () -> Unit,
    harnessName: String? = null,
    probing: Boolean = false,
    onRefreshHarness: () -> Unit = {},
) {
    Box(Modifier.fillMaxWidth().padding(horizontal = 12.dp, vertical = 4.dp), contentAlignment = Alignment.CenterEnd) {
        Surface(
            color = MaterialTheme.colorScheme.primaryContainer.copy(alpha = PENDING_ALPHA),
            shape = RoundedCornerShape(topStart = 16.dp, topEnd = 4.dp, bottomStart = 16.dp, bottomEnd = 16.dp),
            modifier = Modifier.widthIn(max = 320.dp),
        ) {
            Column(Modifier.padding(horizontal = 14.dp, vertical = 10.dp), verticalArrangement = Arrangement.spacedBy(4.dp)) {
                Text(input.text ?: stringResource(R.string.pending_unreadable), style = MaterialTheme.typography.bodyLarge, color = MaterialTheme.colorScheme.onPrimaryContainer)
                if (input.images > 0) Text(stringResource(R.string.item_attachments, input.images), style = MaterialTheme.typography.labelSmall)
                Row(verticalAlignment = Alignment.CenterVertically) {
                    Icon(Icons.Outlined.Schedule, null, Modifier.size(14.dp), tint = MaterialTheme.colorScheme.onSurfaceVariant)
                    Spacer(Modifier.width(4.dp))
                    val state = stringResource(if (online) R.string.pending_sending else R.string.pending_offline)
                    val delivery = when (input.delivery) {
                        Delivery.Queue -> stringResource(R.string.pending_queue)
                        Delivery.Steer -> stringResource(R.string.pending_steer)
                        else -> null
                    }
                    Text(listOfNotNull(state, delivery).joinToString(" · "), style = MaterialTheme.typography.labelSmall, color = MaterialTheme.colorScheme.onSurfaceVariant)
                }
                if (harnessName != null) {
                    Text(stringResource(R.string.harness_wait_title, harnessName), style = MaterialTheme.typography.labelMedium, color = MaterialTheme.statusColors.needsApproval)
                    input.lastError?.let { Text(stringResource(R.string.harness_wait_reason, it), style = MaterialTheme.typography.labelSmall) }
                    Text(stringResource(R.string.harness_wait_note), style = MaterialTheme.typography.labelSmall, color = MaterialTheme.colorScheme.onSurfaceVariant)
                } else if (input.failures > 0) {
                    Text(stringResource(R.string.pending_retries, input.failures, input.lastError.orEmpty()), style = MaterialTheme.typography.labelSmall, color = MaterialTheme.statusColors.needsApproval)
                }
                Row(Modifier.align(Alignment.End), verticalAlignment = Alignment.CenterVertically) {
                    if (harnessName != null) {
                        if (probing) {
                            Text(stringResource(R.string.harness_state_probing), style = MaterialTheme.typography.labelMedium)
                        } else {
                            TextButton(onClick = onRefreshHarness) { Text(stringResource(R.string.harness_refresh), style = MaterialTheme.typography.labelMedium) }
                        }
                    }
                    // Also the way out of a message that keeps failing (its thread's later requests wait behind it).
                    TextButton(onClick = onDiscard) {
                        Text(stringResource(R.string.pending_discard), style = MaterialTheme.typography.labelMedium)
                    }
                }
            }
        }
    }
}

/** Older turns: loaded when this row comes into view (online), or by the button. */
@Composable
fun LoadOlderRow(loading: Boolean, failed: String?, onLoad: () -> Unit) {
    Column(Modifier.fillMaxWidth().padding(8.dp), horizontalAlignment = Alignment.CenterHorizontally) {
        if (loading) {
            CircularProgressIndicator(Modifier.size(20.dp), strokeWidth = 2.dp)
        } else {
            failed?.let { Text(it, style = MaterialTheme.typography.bodySmall, color = MaterialTheme.statusColors.error) }
            OutlinedButton(onClick = onLoad) { Text(stringResource(R.string.thread_load_older)) }
        }
    }
}

/** A banner row of the thread (archived, offline). */
@Composable
fun ArchivedBanner(onUnarchive: () -> Unit) {
    Surface(color = MaterialTheme.colorScheme.surfaceContainerHigh, modifier = Modifier.fillMaxWidth()) {
        Row(Modifier.padding(horizontal = 16.dp, vertical = 6.dp), verticalAlignment = Alignment.CenterVertically) {
            Text(stringResource(R.string.thread_archived_banner), style = MaterialTheme.typography.bodyMedium, modifier = Modifier.weight(1f))
            TextButton(onClick = onUnarchive) {
                Icon(Icons.Outlined.Unarchive, null, Modifier.size(18.dp))
                Spacer(Modifier.width(4.dp))
                Text(stringResource(R.string.thread_unarchive))
            }
        }
    }
}

private const val THOUSAND = 1_000L
private const val MILLION = 1_000_000L

private const val PENDING_ALPHA = 0.55f
