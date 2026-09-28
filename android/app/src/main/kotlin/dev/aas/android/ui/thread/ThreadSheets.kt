package dev.aas.android.ui.thread

import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.navigationBarsPadding
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.verticalScroll
import androidx.compose.material3.ExperimentalMaterial3Api
import androidx.compose.material3.ListItem
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.ModalBottomSheet
import androidx.compose.material3.Text
import androidx.compose.material3.rememberModalBottomSheetState
import androidx.compose.runtime.Composable
import androidx.compose.ui.Modifier
import androidx.compose.ui.res.stringResource
import androidx.compose.ui.unit.dp
import dev.aas.android.R
import dev.aas.android.domain.composer.HarnessSettings
import dev.aas.android.protocol.ContextUsage
import dev.aas.android.protocol.Harness
import dev.aas.android.protocol.Thread
import dev.aas.android.protocol.ThreadStatus
import dev.aas.android.protocol.Workspace
import dev.aas.android.ui.components.CopyIconButton
import dev.aas.android.ui.components.relativeTime

/**
 * `/status` (docs/ux/codex-desktop.md §8.5): the thread id, the harness's native session id, the
 * context use, the process state (`idle`: no process), its background work (`Thread.background`)
 * and where the thread works.
 */
@OptIn(ExperimentalMaterial3Api::class)
@Composable
fun StatusSheet(thread: Thread, harness: Harness?, context: ContextUsage?, onDismiss: () -> Unit) {
    ModalBottomSheet(onDismissRequest = onDismiss, sheetState = rememberModalBottomSheetState(skipPartiallyExpanded = true)) {
        Column(Modifier.fillMaxWidth().navigationBarsPadding().verticalScroll(rememberScrollState())) {
            Text(stringResource(R.string.status_title), style = MaterialTheme.typography.titleLarge, modifier = Modifier.padding(horizontal = 24.dp))
            Spacer(Modifier.height(8.dp))
            InfoRow(stringResource(R.string.status_thread_id), thread.id, copy = true)
            InfoRow(stringResource(R.string.status_native_session), thread.nativeSessionId ?: stringResource(R.string.status_native_none), copy = thread.nativeSessionId != null)
            InfoRow(stringResource(R.string.status_harness), HarnessSettings.label(harness, thread.settings).ifEmpty { thread.harnessId })
            HarnessSettings.permission(harness, thread.settings)?.let { InfoRow(stringResource(R.string.status_permission), it.label) }
            InfoRow(stringResource(R.string.status_process), processLabel(thread.status))
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
            thread.lastError?.let { InfoRow(stringResource(R.string.status_last_error), "${it.message} (${it.kind})") }
            InfoRow(stringResource(R.string.status_created), relativeTime(thread.createdAt))
            Spacer(Modifier.height(24.dp))
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
