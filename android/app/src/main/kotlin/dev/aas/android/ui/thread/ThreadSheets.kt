package dev.aas.android.ui.thread

import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.navigationBarsPadding
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.text.selection.SelectionContainer
import androidx.compose.foundation.verticalScroll
import androidx.compose.material3.CircularProgressIndicator
import androidx.compose.material3.ExperimentalMaterial3Api
import androidx.compose.material3.ListItem
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.ModalBottomSheet
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.material3.rememberModalBottomSheetState
import androidx.compose.runtime.Composable
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.res.stringResource
import androidx.compose.ui.unit.dp
import dev.aas.android.R
import dev.aas.android.domain.ErrorTexts
import dev.aas.android.domain.composer.HarnessSettings
import dev.aas.android.protocol.ContextUsage
import dev.aas.android.protocol.Harness
import dev.aas.android.protocol.Thread
import dev.aas.android.protocol.ThreadStatus
import dev.aas.android.protocol.Workspace
import dev.aas.android.ui.common.asString
import dev.aas.android.ui.components.CopyIconButton
import dev.aas.android.ui.components.MarkdownText
import dev.aas.android.ui.components.SectionHeader
import dev.aas.android.ui.components.relativeTime
import dev.aas.android.ui.theme.statusColors

/**
 * `/status` (docs/ux/codex-desktop.md §8.5): the thread id, the harness's native session id, the
 * context use, the process state (`idle`: no process), its modes, its background work
 * (`Thread.background`), where the thread works, the project's trust decision for harnesses that
 * ask for one ([trust], [onSetTrust]), and the harness's own status in its sections and words
 * ([harnessStatus], `thread/harnessStatus`).
 */
@OptIn(ExperimentalMaterial3Api::class)
@Composable
fun StatusSheet(
    thread: Thread,
    harness: Harness?,
    context: ContextUsage?,
    onDismiss: () -> Unit,
    harnessStatus: HarnessStatusState = HarnessStatusState.Idle,
    trust: Boolean? = null,
    onSetTrust: ((Boolean) -> Unit)? = null,
) {
    ModalBottomSheet(onDismissRequest = onDismiss, sheetState = rememberModalBottomSheetState(skipPartiallyExpanded = true)) {
        Column(Modifier.fillMaxWidth().navigationBarsPadding().verticalScroll(rememberScrollState())) {
            Text(stringResource(R.string.status_title), style = MaterialTheme.typography.titleLarge, modifier = Modifier.padding(horizontal = 24.dp))
            Spacer(Modifier.height(8.dp))
            InfoRow(stringResource(R.string.status_thread_id), thread.id, copy = true)
            InfoRow(stringResource(R.string.status_native_session), thread.nativeSessionId ?: stringResource(R.string.status_native_none), copy = thread.nativeSessionId != null)
            InfoRow(stringResource(R.string.status_harness), HarnessSettings.label(harness, thread.settings).ifEmpty { thread.harnessId })
            HarnessSettings.permission(harness, thread.settings)?.let { InfoRow(stringResource(R.string.status_permission), it.label) }
            InfoRow(stringResource(R.string.status_process), processLabel(thread.status))
            val features = harness?.features
            if (features?.planMode != null || features?.fastModeModels?.isNotEmpty() == true || thread.modes.plan || thread.modes.fast) {
                val modes = buildList {
                    if (thread.modes.plan) add(stringResource(R.string.status_mode_plan))
                    if (thread.modes.fast) {
                        add(thread.fastModeState?.let { stringResource(R.string.status_mode_fast_state, it) } ?: stringResource(R.string.status_mode_fast))
                    }
                }
                InfoRow(stringResource(R.string.status_modes), modes.joinToString("\n").ifEmpty { stringResource(R.string.status_modes_none) })
            }
            val background = thread.background
            if (background.running > 0 || background.lastEnded != null) {
                val parts = buildList {
                    add(stringResource(R.string.status_background_value, background.running))
                    background.lastEnded?.let { add(stringResource(R.string.status_background_last, it.title, backgroundStatusWord(it.status))) }
                }
                InfoRow(stringResource(R.string.status_background), parts.joinToString("\n"))
            }
            InfoRow(
                stringResource(R.string.status_context),
                context?.let { stringResource(R.string.status_context_value, compactNumber(it.usedTokens), compactNumber(it.windowTokens), contextPercent(it)) }
                    ?: stringResource(R.string.status_context_unreported),
            )
            InfoRow(stringResource(R.string.status_usage), usageText(thread.usage))
            InfoRow(stringResource(R.string.status_cwd), thread.cwd, copy = true)
            when (val workspace = thread.workspace) {
                is Workspace.Worktree -> InfoRow(stringResource(R.string.status_worktree), stringResource(R.string.status_worktree_value, workspace.branch, workspace.baseRef))
                Workspace.Local -> InfoRow(stringResource(R.string.status_workspace), stringResource(R.string.workspace_local))
                is Workspace.Unknown -> InfoRow(stringResource(R.string.status_workspace), workspace.kind)
            }
            thread.forkedFrom?.let { InfoRow(stringResource(R.string.status_forked_from), it.threadId) }
            if (thread.queuedInputs > 0) InfoRow(stringResource(R.string.status_queue), stringResource(if (thread.queuePaused) R.string.status_queue_paused else R.string.status_queue_running, thread.queuedInputs))
            thread.lastError?.let { InfoRow(stringResource(R.string.status_last_error), ErrorTexts.turnError(it.kind, it.message).asString()) }
            InfoRow(stringResource(R.string.status_created), relativeTime(thread.createdAt))
            if (harness != null && harness.features.projectTrust && onSetTrust != null) {
                ListItem(
                    overlineContent = { Text(stringResource(R.string.status_trust, harness.displayName)) },
                    headlineContent = {
                        Text(stringResource(when (trust) { true -> R.string.status_trust_yes; false -> R.string.status_trust_no; null -> R.string.status_trust_undecided }))
                    },
                    trailingContent = {
                        TextButton(onClick = { onSetTrust(trust != true) }) { Text(stringResource(if (trust == true) R.string.trust_no else R.string.trust_yes)) }
                    },
                )
            }
            if (harness != null && harness.features.status) HarnessStatusSection(harnessStatus)
            Spacer(Modifier.height(24.dp))
        }
    }
}

/** The harness's own status (`thread/harnessStatus`): its sections and rows as it words them. */
@Composable
private fun HarnessStatusSection(status: HarnessStatusState) {
    SectionHeader(stringResource(R.string.status_harness_section))
    when (status) {
        HarnessStatusState.Idle, HarnessStatusState.Loading -> StatusNote(stringResource(R.string.status_harness_loading))
        HarnessStatusState.Offline -> StatusNote(stringResource(R.string.status_harness_offline))
        is HarnessStatusState.Failed -> StatusNote(stringResource(R.string.status_harness_failed, status.message.asString()))
        is HarnessStatusState.Loaded -> {
            StatusNote(stringResource(if (status.result.live) R.string.status_harness_live else R.string.status_harness_idle))
            if (status.result.sections.all { it.rows.isEmpty() }) StatusNote(stringResource(R.string.status_harness_empty))
            status.result.sections.forEach { section ->
                if (section.rows.isNotEmpty()) {
                    Text(section.title, style = MaterialTheme.typography.titleSmall, modifier = Modifier.padding(start = 16.dp, end = 16.dp, top = 8.dp))
                    section.rows.forEach { row -> InfoRow(row.label, row.value) }
                }
            }
        }
    }
}

@Composable
private fun StatusNote(text: String) {
    Text(text, style = MaterialTheme.typography.bodySmall, color = MaterialTheme.colorScheme.onSurfaceVariant, modifier = Modifier.padding(horizontal = 16.dp, vertical = 4.dp))
}

/**
 * `/btw`: a question beside the conversation and the running agent's answer (Markdown,
 * verbatim). Neither enters the conversation's history; the sheet says so.
 */
@OptIn(ExperimentalMaterial3Api::class)
@Composable
fun SideQuestionSheet(state: SideQuestionState, onDismiss: () -> Unit) {
    ModalBottomSheet(onDismissRequest = onDismiss, sheetState = rememberModalBottomSheetState(skipPartiallyExpanded = true)) {
        Column(Modifier.fillMaxWidth().navigationBarsPadding().verticalScroll(rememberScrollState()).padding(horizontal = 24.dp)) {
            Text(stringResource(R.string.btw_title), style = MaterialTheme.typography.titleLarge)
            Spacer(Modifier.height(8.dp))
            SelectionContainer { Text(state.question, style = MaterialTheme.typography.bodyLarge, color = MaterialTheme.colorScheme.onSurfaceVariant) }
            Spacer(Modifier.height(12.dp))
            when (val answer = state.answer) {
                SideAnswer.Waiting -> Row(verticalAlignment = Alignment.CenterVertically) {
                    CircularProgressIndicator(Modifier.size(18.dp), strokeWidth = 2.dp)
                    Spacer(Modifier.width(10.dp))
                    Text(stringResource(R.string.btw_thinking), style = MaterialTheme.typography.bodyMedium)
                }
                is SideAnswer.Answered -> {
                    val text = answer.answer
                    if (text == null) {
                        Text(stringResource(R.string.btw_no_answer), style = MaterialTheme.typography.bodyMedium, color = MaterialTheme.colorScheme.onSurfaceVariant)
                    } else {
                        SelectionContainer { MarkdownText(text) }
                    }
                    if (answer.synthetic) {
                        Text(stringResource(R.string.btw_synthetic), style = MaterialTheme.typography.labelSmall, color = MaterialTheme.colorScheme.onSurfaceVariant, modifier = Modifier.padding(top = 6.dp))
                    }
                }
                is SideAnswer.Failed -> Text(answer.message.asString(), style = MaterialTheme.typography.bodyMedium, color = MaterialTheme.statusColors.error)
            }
            Spacer(Modifier.height(12.dp))
            Text(stringResource(R.string.btw_note), style = MaterialTheme.typography.labelSmall, color = MaterialTheme.colorScheme.onSurfaceVariant)
            Row(Modifier.fillMaxWidth(), horizontalArrangement = Arrangement.End) {
                TextButton(onClick = onDismiss) { Text(stringResource(R.string.close)) }
            }
            Spacer(Modifier.height(16.dp))
        }
    }
}

@Composable
private fun InfoRow(label: String, value: String, copy: Boolean = false) {
    ListItem(
        overlineContent = { Text(label) },
        headlineContent = { Text(value) },
        trailingContent = if (copy) ({ CopyIconButton(value, R.string.copied_value) }) else null,
    )
}

/** Used tokens as a percentage of the window (both reported by the harness). */
fun contextPercent(context: ContextUsage): Int =
    if (context.windowTokens <= 0) 0 else ((context.usedTokens * PERCENT) / context.windowTokens).toInt().coerceIn(0, PERCENT.toInt())

@Composable
fun processLabel(status: ThreadStatus): String = stringResource(
    when (status) {
        ThreadStatus.Idle -> R.string.process_idle
        ThreadStatus.Queued -> R.string.process_queued
        ThreadStatus.Starting -> R.string.process_starting
        ThreadStatus.Ready -> R.string.process_ready
        ThreadStatus.Running -> R.string.process_running
        ThreadStatus.Stopping -> R.string.process_stopping
        ThreadStatus.Unknown -> R.string.unknown
    },
)

private const val PERCENT = 100L
