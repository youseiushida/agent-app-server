package dev.aas.android.ui.thread

import android.text.format.Formatter
import androidx.compose.foundation.clickable
import androidx.compose.foundation.horizontalScroll
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.outlined.CheckCircle
import androidx.compose.material3.CircularProgressIndicator
import androidx.compose.material3.Icon
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.OutlinedButton
import androidx.compose.material3.Surface
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.runtime.Composable
import androidx.compose.runtime.remember
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.graphics.vector.ImageVector
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.platform.LocalResources
import androidx.compose.ui.platform.testTag
import androidx.compose.ui.res.stringResource
import androidx.compose.ui.semantics.contentDescription
import androidx.compose.ui.semantics.semantics
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.unit.dp
import dev.aas.android.R
import dev.aas.android.domain.InteractionTexts
import dev.aas.android.domain.TaskOutput
import dev.aas.android.domain.timeline.TimelineRow
import dev.aas.android.protocol.BackgroundEndReason
import dev.aas.android.protocol.BackgroundTask
import dev.aas.android.protocol.BackgroundTaskKind
import dev.aas.android.protocol.BackgroundTaskStatus
import dev.aas.android.protocol.WorkflowAgent
import dev.aas.android.protocol.WorkflowAgentState
import dev.aas.android.ui.common.LocalAppPolicy
import dev.aas.android.ui.components.rememberNow
import dev.aas.android.ui.icons.AccountTree
import dev.aas.android.ui.icons.AutoAwesome
import dev.aas.android.ui.icons.Cloud
import dev.aas.android.ui.icons.ErrorOutline
import dev.aas.android.ui.icons.ExpandLess
import dev.aas.android.ui.icons.ExpandMore
import dev.aas.android.ui.icons.Groups
import dev.aas.android.ui.icons.PlayCircle
import dev.aas.android.ui.icons.RadioButtonUnchecked
import dev.aas.android.ui.icons.Schedule
import dev.aas.android.ui.icons.Terminal
import dev.aas.android.ui.icons.Visibility
import dev.aas.android.ui.theme.codeStyle
import dev.aas.android.ui.theme.statusColors
import java.text.DateFormat
import java.util.Date

/** Test tags of the バックグラウンド section (Compose UI and device tests). */
object BackgroundTags {
    const val HEADER = "bg-header"
    const val ENDED = "bg-ended"

    /** The card of one task. */
    fun task(taskId: String) = "bg-task-$taskId"

    /** The 停止 button of one task. */
    fun stop(taskId: String) = "bg-stop-$taskId"

    /** The chip of an item whose work goes on as [taskId]. */
    fun chip(taskId: String) = "bg-chip-$taskId"

    /** The output lines in the card of one task. */
    fun output(taskId: String) = "bg-output-$taskId"

    /** The 出力の全文を表示 button of one task. */
    fun showOutput(taskId: String) = "bg-show-output-$taskId"
}

/**
 * Whether and how a task can be stopped from this phone (`backgroundTask/stop`), from explicit
 * state only: the harness's capability, the task's `stoppable` and `stopRequestedAt` /
 * `stopUnconfirmedAt`, and this thread's outbox.
 */
enum class BackgroundStop {
    /** Ended, not stoppable on its own, or the harness cannot stop single tasks: no button. */
    None,

    /** 停止 can be pressed. */
    Available,

    /** A `backgroundTask/stop` for it waits in the outbox (sent when connected). */
    Queued,

    /** The daemon asked the harness to stop it and waits for the end (停止中…). */
    Requested,

    /** The harness did not confirm the last stop in time: 停止 can be pressed again (with a note). */
    Unconfirmed,
    ;

    companion object {
        fun of(task: BackgroundTask, harnessCanStop: Boolean, queued: Boolean): BackgroundStop = when {
            task.status != BackgroundTaskStatus.Running || !task.stoppable || !harnessCanStop -> None
            queued -> Queued
            task.stopRequestedAt != null -> Requested
            task.stopUnconfirmedAt != null -> Unconfirmed
            else -> Available
        }
    }
}

/** The head of the バックグラウンド section: its counts; tapping folds or opens it. */
@Composable
fun BackgroundHeaderRow(row: TimelineRow.BackgroundHeader, onToggle: () -> Unit) {
    val parts = buildList {
        if (row.running > 0) add(stringResource(R.string.bg_section_running, row.running))
        if (row.ambient > 0) add(stringResource(R.string.bg_section_ambient, row.ambient))
        if (row.ended > 0) add(stringResource(R.string.bg_section_ended, row.ended))
        if (isEmpty()) add(stringResource(R.string.bg_section_idle))
    }
    val working = row.running > 0 || row.ambient > 0
    Row(
        Modifier.fillMaxWidth().clickable(onClick = onToggle).padding(horizontal = 16.dp, vertical = 10.dp).testTag(BackgroundTags.HEADER),
        verticalAlignment = Alignment.CenterVertically,
    ) {
        if (row.running > 0) {
            CircularProgressIndicator(Modifier.size(16.dp), strokeWidth = 2.dp, color = MaterialTheme.statusColors.running)
        } else {
            Icon(Icons.Outlined.AutoAwesome, null, Modifier.size(16.dp), tint = MaterialTheme.colorScheme.onSurfaceVariant)
        }
        Spacer(Modifier.width(10.dp))
        Text(
            stringResource(R.string.bg_section_title),
            style = MaterialTheme.typography.labelLarge,
            color = if (working) MaterialTheme.statusColors.running else MaterialTheme.colorScheme.onSurfaceVariant,
        )
        Spacer(Modifier.width(8.dp))
        Text(parts.joinToString(" · "), style = MaterialTheme.typography.labelMedium, color = MaterialTheme.colorScheme.onSurfaceVariant, modifier = Modifier.weight(1f))
        Icon(
            if (row.expanded) Icons.Outlined.ExpandLess else Icons.Outlined.ExpandMore,
            contentDescription = stringResource(if (row.expanded) R.string.collapse else R.string.expand),
            tint = MaterialTheme.colorScheme.onSurfaceVariant,
        )
    }
}

/** The folded list of ended tasks: 終了した作業 (n). */
@Composable
fun BackgroundEndedHeaderRow(row: TimelineRow.BackgroundEndedHeader, onToggle: () -> Unit) {
    Row(
        Modifier.fillMaxWidth().clickable(onClick = onToggle).padding(start = 42.dp, end = 16.dp, top = 6.dp, bottom = 6.dp).testTag(BackgroundTags.ENDED),
        verticalAlignment = Alignment.CenterVertically,
    ) {
        Text(stringResource(R.string.bg_ended_title, row.count), style = MaterialTheme.typography.labelMedium, color = MaterialTheme.colorScheme.onSurfaceVariant, modifier = Modifier.weight(1f))
        Icon(
            if (row.expanded) Icons.Outlined.ExpandLess else Icons.Outlined.ExpandMore,
            contentDescription = stringResource(if (row.expanded) R.string.collapse else R.string.expand),
            tint = MaterialTheme.colorScheme.onSurfaceVariant,
        )
    }
}

/**
 * One background task (docs/android.md 30): its kind, the harness's title verbatim, how long it
 * has run (or how it ended and why), the progress the harness reports (last tool, tool uses,
 * tokens, a workflow's agents with their states), its output (streamed while it runs, the whole
 * one at the end; [TaskOutputView]), the result it reported (summary, exit code) and
 * 停止 / 停止中…. A lost task is in the error colour.
 */
@Composable
fun BackgroundTaskCard(
    task: BackgroundTask,
    depth: Int,
    parentTitle: String?,
    stop: BackgroundStop,
    onStop: () -> Unit,
    onOpenOutput: () -> Unit,
    modifier: Modifier = Modifier,
) {
    val display = LocalAppPolicy.current.display
    val running = task.status == BackgroundTaskStatus.Running
    val now = rememberNow(display.workingTickMs)
    val accent = backgroundStatusColor(task.status)
    Surface(
        color = MaterialTheme.colorScheme.surfaceContainer,
        shape = RoundedCornerShape(10.dp),
        modifier = modifier
            .fillMaxWidth()
            .padding(start = 12.dp + DEPTH_INDENT * depth.coerceAtMost(MAX_DEPTH), end = 12.dp, top = 3.dp, bottom = 3.dp)
            .testTag(BackgroundTags.task(task.id)),
    ) {
        Column(Modifier.padding(horizontal = 12.dp, vertical = 8.dp), verticalArrangement = Arrangement.spacedBy(2.dp)) {
            Row(verticalAlignment = Alignment.Top) {
                if (running) {
                    CircularProgressIndicator(Modifier.padding(top = 2.dp).size(16.dp), strokeWidth = 2.dp, color = MaterialTheme.statusColors.running)
                } else {
                    Icon(backgroundKindIcon(task.kind), contentDescription = null, tint = accent ?: MaterialTheme.colorScheme.onSurfaceVariant, modifier = Modifier.size(18.dp))
                }
                Spacer(Modifier.width(8.dp))
                Column(Modifier.weight(1f)) {
                    Text(
                        task.title.ifEmpty { backgroundKindLabel(task.kind) },
                        style = MaterialTheme.typography.bodyMedium,
                        fontWeight = FontWeight.Medium,
                        color = if (task.status == BackgroundTaskStatus.Lost) MaterialTheme.statusColors.error else MaterialTheme.colorScheme.onSurface,
                        maxLines = 3,
                        overflow = TextOverflow.Ellipsis,
                    )
                    val meta = buildList {
                        add(backgroundKindLabel(task.kind))
                        add(backgroundStatusText(task, now))
                        if (task.runs > 1) add(stringResource(R.string.bg_run_number, task.runs))
                        if (task.ambient) add(stringResource(R.string.bg_ambient))
                    }
                    Text(meta.joinToString(" · "), style = MaterialTheme.typography.labelSmall, color = accent ?: MaterialTheme.colorScheme.onSurfaceVariant)
                }
                StopControl(task, stop, onStop)
            }
            parentTitle?.let { Detail(stringResource(R.string.bg_parent, it)) }
            if (!running) endReasonText(task.endReason)?.let { Detail(it, color = if (task.status == BackgroundTaskStatus.Lost) MaterialTheme.statusColors.error else null) }
            if (stop == BackgroundStop.Unconfirmed) Detail(stringResource(R.string.bg_stop_unconfirmed), color = MaterialTheme.statusColors.needsApproval)
            task.nextRunAt?.takeIf { running }?.let { Detail(stringResource(R.string.bg_next_run, DateFormat.getTimeInstance(DateFormat.SHORT).format(Date(it)))) }
            progressText(task)?.let { Detail(it) }
            task.progress?.summary?.takeIf { it.isNotBlank() }?.let { Detail(it, maxLines = SUMMARY_LINES) }
            task.progress?.workflow?.takeIf { it.isNotEmpty() }?.let { agents -> WorkflowAgents(agents) }
            task.result?.let { result ->
                result.summary?.takeIf { it.isNotBlank() }?.let { Detail(it, maxLines = SUMMARY_LINES, color = MaterialTheme.colorScheme.onSurface) }
                result.exitCode?.let { code ->
                    Detail(stringResource(R.string.bg_exit_code, code), color = if (code != 0) MaterialTheme.statusColors.error else null)
                }
            }
            TaskOutput.of(task)?.let { output -> TaskOutputView(task.id, output, display.taskOutputLines, onOpenOutput) }
        }
    }
}

/**
 * A task's output in its card (monospace): while the harness streams it, its newest lines, which
 * follow the output as it grows; the whole output once reported (its last lines, or its first
 * ones when only the beginning is here); what cut it; and 出力の全文を表示, which follows the
 * stream too, or reads the blob with all of it (docs/android.md 30).
 */
@Composable
private fun TaskOutputView(taskId: String, output: TaskOutput, shownLines: Int, onOpenOutput: () -> Unit) {
    // The beginning of an output cut short reads on from its start; any other shows its end.
    val fromStart = output.blobId != null || output.cutWithoutBlob
    val excerpt = remember(output.text, fromStart, shownLines) {
        if (fromStart) TaskOutput.firstLines(output.text, shownLines) else TaskOutput.lastLines(output.text, shownLines)
    }
    if (output.live) {
        Text(
            stringResource(R.string.bg_output_live),
            style = MaterialTheme.typography.labelSmall,
            color = MaterialTheme.statusColors.running,
            modifier = Modifier.padding(start = 26.dp, top = 2.dp),
        )
    }
    if (excerpt.lines.isNotEmpty()) {
        Text(
            excerpt.lines.joinToString("\n"),
            style = MaterialTheme.codeStyle,
            softWrap = false,
            color = MaterialTheme.colorScheme.onSurfaceVariant,
            modifier = Modifier.fillMaxWidth().horizontalScroll(rememberScrollState()).padding(vertical = 2.dp).testTag(BackgroundTags.output(taskId)),
        )
    }
    val context = LocalContext.current
    output.omittedBytes?.let { Detail(stringResource(R.string.bg_output_omitted, Formatter.formatShortFileSize(context, it))) }
    when {
        output.streamLimitReached -> Detail(stringResource(if (output.live) R.string.bg_output_stream_limit else R.string.bg_output_stream_limit_ended))
        output.blobId != null -> Detail(stringResource(R.string.bg_output_partial))
        output.cutWithoutBlob -> Detail(stringResource(R.string.item_output_truncated))
    }
    if (output.live || output.blobId != null || excerpt.more) {
        TextButton(onClick = onOpenOutput, modifier = Modifier.testTag(BackgroundTags.showOutput(taskId))) { Text(stringResource(R.string.bg_show_output)) }
    }
}

@Composable
private fun StopControl(task: BackgroundTask, stop: BackgroundStop, onStop: () -> Unit) {
    when (stop) {
        BackgroundStop.None -> Unit
        BackgroundStop.Available, BackgroundStop.Unconfirmed -> {
            val description = stringResource(R.string.bg_stop_description, task.title)
            OutlinedButton(
                onClick = onStop,
                modifier = Modifier.padding(start = 8.dp).semantics { contentDescription = description }.testTag(BackgroundTags.stop(task.id)),
            ) { Text(stringResource(R.string.bg_stop)) }
        }
        BackgroundStop.Queued, BackgroundStop.Requested -> {
            OutlinedButton(onClick = {}, enabled = false, modifier = Modifier.padding(start = 8.dp).testTag(BackgroundTags.stop(task.id))) {
                Text(stringResource(if (stop == BackgroundStop.Queued) R.string.bg_stop_pending else R.string.bg_stopping))
            }
        }
    }
}

@Composable
private fun Detail(text: String, maxLines: Int = 2, color: Color? = null) {
    Text(
        text,
        style = MaterialTheme.typography.bodySmall,
        color = color ?: MaterialTheme.colorScheme.onSurfaceVariant,
        maxLines = maxLines,
        overflow = TextOverflow.Ellipsis,
        modifier = Modifier.padding(start = 26.dp),
    )
}

/** A workflow's agents in the harness's order: state, label, phase, agent type, model, tokens. */
@Composable
private fun WorkflowAgents(agents: List<WorkflowAgent>) {
    Column(Modifier.padding(start = 26.dp, top = 2.dp), verticalArrangement = Arrangement.spacedBy(2.dp)) {
        agents.forEach { agent ->
            Row(verticalAlignment = Alignment.CenterVertically) {
                val (icon, tint) = when (agent.state) {
                    WorkflowAgentState.Done -> Icons.Outlined.CheckCircle to MaterialTheme.statusColors.connected
                    WorkflowAgentState.Progress, WorkflowAgentState.Start -> Icons.Outlined.PlayCircle to MaterialTheme.statusColors.running
                    WorkflowAgentState.Error -> Icons.Outlined.ErrorOutline to MaterialTheme.statusColors.error
                    WorkflowAgentState.Unknown -> Icons.Outlined.RadioButtonUnchecked to MaterialTheme.colorScheme.onSurfaceVariant
                }
                Icon(icon, contentDescription = workflowStateLabel(agent.state), tint = tint, modifier = Modifier.size(14.dp))
                Spacer(Modifier.width(6.dp))
                val parts = listOfNotNull(
                    agent.phase?.let { "$it: ${agent.label}" } ?: agent.label,
                    workflowStateLabel(agent.state),
                    agent.agentType,
                    agent.model,
                    agent.tokens?.let { stringResource(R.string.bg_tokens, compactNumber(it)) },
                )
                Text(parts.joinToString(" · "), style = MaterialTheme.typography.bodySmall, maxLines = 1, overflow = TextOverflow.Ellipsis)
            }
        }
    }
}

@Composable
private fun workflowStateLabel(state: WorkflowAgentState): String = stringResource(
    when (state) {
        WorkflowAgentState.Start -> R.string.bg_workflow_agent_start
        WorkflowAgentState.Progress -> R.string.bg_workflow_agent_progress
        WorkflowAgentState.Done -> R.string.bg_workflow_agent_done
        WorkflowAgentState.Error -> R.string.bg_workflow_agent_error
        WorkflowAgentState.Unknown -> R.string.bg_workflow_agent_unknown
    },
)

/** "最後のツール: Read · ツール 7 回 · 18.4k トークン" from the harness's own figures (progress, else usage). */
@Composable
private fun progressText(task: BackgroundTask): String? {
    val progress = task.progress
    val usage = task.usage
    val parts = buildList {
        progress?.lastToolName?.let { add(stringResource(R.string.bg_last_tool, it)) }
        (progress?.toolUses ?: usage?.toolUses)?.let { add(stringResource(R.string.bg_tool_uses, it.toInt())) }
        (progress?.tokens ?: usage?.totalTokens)?.let { add(stringResource(R.string.bg_tokens, compactNumber(it))) }
    }
    return parts.takeIf { it.isNotEmpty() }?.joinToString(" · ")
}

/**
 * The chip under an item whose work goes on as a background task: the task's live status
 * ("バックグラウンドで実行中 · 3 分" / "バックグラウンド: 完了"); tapping it shows the task in the
 * バックグラウンド section. Without the task (not loaded) it only says the work went on.
 */
@Composable
fun BackgroundChip(taskId: String, task: BackgroundTask?, onOpen: (String) -> Unit) {
    val now = rememberNow(LocalAppPolicy.current.display.workingTickMs)
    val res = LocalResources.current
    val text = when {
        task == null -> stringResource(R.string.bg_chip_unknown)
        task.status == BackgroundTaskStatus.Running ->
            stringResource(R.string.bg_chip_running, InteractionTexts.duration(res, (now - task.startedAt).coerceAtLeast(0)))
        else -> stringResource(R.string.bg_chip_ended, backgroundStatusWord(task.status))
    }
    val color = task?.let { backgroundStatusColor(it.status) } ?: MaterialTheme.statusColors.running
    val openLabel = stringResource(R.string.bg_chip_open)
    Surface(
        shape = RoundedCornerShape(50),
        color = color.copy(alpha = CHIP_ALPHA),
        modifier = Modifier
            .padding(top = 4.dp)
            .then(if (task != null) Modifier.clickable(onClickLabel = openLabel) { onOpen(taskId) } else Modifier)
            .testTag(BackgroundTags.chip(taskId)),
    ) {
        Row(Modifier.padding(horizontal = 10.dp, vertical = 3.dp), verticalAlignment = Alignment.CenterVertically) {
            if (task?.status == BackgroundTaskStatus.Running) {
                CircularProgressIndicator(Modifier.size(12.dp), strokeWidth = 1.5.dp, color = color)
            } else {
                Icon(task?.let { backgroundKindIcon(it.kind) } ?: Icons.Outlined.AutoAwesome, null, Modifier.size(14.dp), tint = color)
            }
            Spacer(Modifier.width(6.dp))
            Text(text, style = MaterialTheme.typography.labelSmall, color = color, maxLines = 1)
        }
    }
}

/**
 * What a stop or archive confirmation adds while background work runs: "N 件のバックグラウンド作業
 * も止まります" with the tasks' titles (up to `display.dialogTaskTitles`, then "ほか n 件").
 * [count] is the number that runs; [titles] those known on this device (may be fewer).
 */
@Composable
fun RunningBackgroundNote(count: Int, titles: List<String>) {
    if (count <= 0) return
    val limit = LocalAppPolicy.current.display.dialogTaskTitles
    Column(verticalArrangement = Arrangement.spacedBy(2.dp)) {
        Text(stringResource(R.string.stop_background_tasks, count), style = MaterialTheme.typography.bodyMedium, fontWeight = FontWeight.Medium)
        titles.take(limit).forEach { title ->
            Text("・$title", style = MaterialTheme.typography.bodySmall, maxLines = 2, overflow = TextOverflow.Ellipsis)
        }
        val more = count - titles.take(limit).size
        if (more > 0 && titles.isNotEmpty()) Text(stringResource(R.string.stop_background_more, more), style = MaterialTheme.typography.bodySmall)
    }
}

/** The kind of a task as an icon. */
fun backgroundKindIcon(kind: BackgroundTaskKind): ImageVector = when (kind) {
    BackgroundTaskKind.Agent -> Icons.Outlined.Groups
    BackgroundTaskKind.Shell -> Icons.Outlined.Terminal
    BackgroundTaskKind.Workflow -> Icons.Outlined.AccountTree
    BackgroundTaskKind.Monitor -> Icons.Outlined.Visibility
    BackgroundTaskKind.Remote -> Icons.Outlined.Cloud
    BackgroundTaskKind.Scheduled -> Icons.Outlined.Schedule
    BackgroundTaskKind.Other, BackgroundTaskKind.Unknown -> Icons.Outlined.AutoAwesome
}

@Composable
fun backgroundKindLabel(kind: BackgroundTaskKind): String = stringResource(
    when (kind) {
        BackgroundTaskKind.Agent -> R.string.bg_kind_agent
        BackgroundTaskKind.Shell -> R.string.bg_kind_shell
        BackgroundTaskKind.Workflow -> R.string.bg_kind_workflow
        BackgroundTaskKind.Monitor -> R.string.bg_kind_monitor
        BackgroundTaskKind.Remote -> R.string.bg_kind_remote
        BackgroundTaskKind.Scheduled -> R.string.bg_kind_scheduled
        BackgroundTaskKind.Other, BackgroundTaskKind.Unknown -> R.string.bg_kind_other
    },
)

/** 完了 / 失敗 / 停止 / 失われました (実行中 for a running task). */
@Composable
fun backgroundStatusWord(status: BackgroundTaskStatus): String = stringResource(
    when (status) {
        BackgroundTaskStatus.Running -> R.string.activity_running
        BackgroundTaskStatus.Completed -> R.string.bg_status_completed
        BackgroundTaskStatus.Failed -> R.string.bg_status_failed
        BackgroundTaskStatus.Stopped -> R.string.bg_status_stopped
        BackgroundTaskStatus.Lost -> R.string.bg_status_lost
        BackgroundTaskStatus.Unknown -> R.string.bg_status_ended
    },
)

/** A running task: "実行中 · 3 分 12 秒" (since its current run started); an ended one: its status and how long it ran. */
@Composable
private fun backgroundStatusText(task: BackgroundTask, now: Long): String {
    val res = LocalResources.current
    if (task.status == BackgroundTaskStatus.Running) {
        return stringResource(R.string.bg_status_running, InteractionTexts.duration(res, (now - task.startedAt).coerceAtLeast(0)))
    }
    val word = backgroundStatusWord(task.status)
    val ranFor = task.endedAt?.let { InteractionTexts.duration(res, (it - task.startedAt).coerceAtLeast(0)) } ?: return word
    return stringResource(R.string.bg_status_after, word, ranFor)
}

/** Why a task ended when the daemon, not the harness, ended it (nothing for `harness`). */
@Composable
private fun endReasonText(reason: BackgroundEndReason?): String? = when (reason) {
    null, BackgroundEndReason.Harness -> null
    BackgroundEndReason.ThreadStopped -> stringResource(R.string.bg_end_thread_stopped)
    BackgroundEndReason.IdleStop -> stringResource(R.string.bg_end_idle_stop)
    BackgroundEndReason.DaemonShutdown -> stringResource(R.string.bg_end_daemon_shutdown)
    BackgroundEndReason.SystemShutdown -> stringResource(R.string.bg_end_system_shutdown)
    BackgroundEndReason.ForcedStop -> stringResource(R.string.bg_end_forced_stop)
    BackgroundEndReason.ProcessReplaced -> stringResource(R.string.bg_end_process_replaced)
    BackgroundEndReason.ProcessExited -> stringResource(R.string.bg_end_process_exited)
    BackgroundEndReason.DaemonRestarted -> stringResource(R.string.bg_end_daemon_restarted)
    BackgroundEndReason.Unknown -> stringResource(R.string.bg_end_unknown)
}

/** The colour of a task's status (`null`: the default text colour). Lost is an error. */
@Composable
fun backgroundStatusColor(status: BackgroundTaskStatus): Color? = when (status) {
    BackgroundTaskStatus.Running -> MaterialTheme.statusColors.running
    BackgroundTaskStatus.Failed, BackgroundTaskStatus.Lost -> MaterialTheme.statusColors.error
    BackgroundTaskStatus.Stopped -> MaterialTheme.statusColors.needsApproval
    BackgroundTaskStatus.Completed -> MaterialTheme.statusColors.connected
    BackgroundTaskStatus.Unknown -> null
}

/** Indentation per level of a task launched by another running task. */
private val DEPTH_INDENT = 16.dp

/** Deeper levels are drawn at this depth (the phone's width is limited). */
private const val MAX_DEPTH = 3

/** Lines of a summary (the harness's or the result's) before it is cut. */
private const val SUMMARY_LINES = 4

private const val CHIP_ALPHA = 0.14f
