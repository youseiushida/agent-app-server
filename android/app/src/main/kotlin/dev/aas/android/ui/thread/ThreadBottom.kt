package dev.aas.android.ui.thread

import androidx.compose.animation.AnimatedVisibility
import androidx.compose.foundation.clickable
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.heightIn
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.foundation.lazy.items
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.outlined.Edit
import androidx.compose.material3.HorizontalDivider
import androidx.compose.material3.Icon
import androidx.compose.material3.IconButton
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Surface
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.runtime.Composable
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.saveable.rememberSaveable
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.platform.LocalResources
import androidx.compose.ui.res.stringResource
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.unit.dp
import dev.aas.android.R
import dev.aas.android.domain.InteractionTexts
import dev.aas.android.protocol.Interaction
import dev.aas.android.protocol.InteractionRequest
import dev.aas.android.protocol.Item
import dev.aas.android.protocol.PlanEntryStatus
import dev.aas.android.protocol.QueuedInput
import dev.aas.android.ui.icons.Bolt
import dev.aas.android.ui.icons.DeleteOutline
import dev.aas.android.ui.icons.ExpandLess
import dev.aas.android.ui.icons.ExpandMore
import dev.aas.android.ui.icons.PauseCircle
import dev.aas.android.ui.icons.Schedule
import dev.aas.android.ui.theme.statusColors

/**
 * The plan of the running turn as a pill above the composer (docs/ux/codex-desktop.md §3.2 todo
 * plan: "ステップ n / m"); tapping it shows the steps.
 */
@Composable
fun PlanPill(plan: Item.Plan) {
    var open by rememberSaveable(plan.id) { mutableStateOf(false) }
    val done = plan.entries.count { it.status == PlanEntryStatus.Completed }
    val current = plan.entries.firstOrNull { it.status == PlanEntryStatus.InProgress }
    Surface(color = MaterialTheme.colorScheme.surfaceContainerHigh, shape = RoundedCornerShape(12.dp), modifier = Modifier.fillMaxWidth().padding(horizontal = 8.dp, vertical = 2.dp)) {
        Column {
            Row(Modifier.fillMaxWidth().clickable { open = !open }.padding(horizontal = 12.dp, vertical = 6.dp), verticalAlignment = Alignment.CenterVertically) {
                Text(stringResource(R.string.plan_pill, done, plan.entries.size), style = MaterialTheme.typography.labelMedium)
                current?.let {
                    Spacer(Modifier.width(8.dp))
                    Text(it.text, style = MaterialTheme.typography.labelMedium, color = MaterialTheme.colorScheme.onSurfaceVariant, maxLines = 1, overflow = TextOverflow.Ellipsis, modifier = Modifier.weight(1f))
                } ?: Spacer(Modifier.weight(1f))
                Icon(if (open) Icons.Outlined.ExpandLess else Icons.Outlined.ExpandMore, contentDescription = stringResource(if (open) R.string.collapse else R.string.expand))
            }
            AnimatedVisibility(open) { PlanView(plan, Modifier.padding(horizontal = 4.dp, vertical = 4.dp)) }
        }
    }
}

/** The queue is paused after an interrupt or a failure (UX §2.5): 再開 runs it again. */
@Composable
fun PausedQueueBanner(count: Int, onResume: () -> Unit) {
    Surface(color = MaterialTheme.statusColors.needsApproval.copy(alpha = BANNER_ALPHA), modifier = Modifier.fillMaxWidth()) {
        Row(Modifier.padding(start = 16.dp, end = 8.dp, top = 4.dp, bottom = 4.dp), verticalAlignment = Alignment.CenterVertically) {
            Icon(Icons.Outlined.PauseCircle, null, Modifier.size(18.dp))
            Spacer(Modifier.width(8.dp))
            Text(stringResource(R.string.queue_paused, count), style = MaterialTheme.typography.bodyMedium, modifier = Modifier.weight(1f))
            TextButton(onClick = onResume) { Text(stringResource(R.string.queue_resume)) }
        }
    }
}

/**
 * Messages waiting in the daemon's queue (UX §2.5): each can be edited (`queue/update`), sent now
 * (`queue/steer`: into the running turn, or as a new turn) or removed (`queue/remove`).
 */
@Composable
fun QueuedList(
    queued: List<QueuedInput>,
    canSendNow: Boolean,
    onEdit: (QueuedInput) -> Unit,
    onSendNow: (QueuedInput) -> Unit,
    onRemove: (QueuedInput) -> Unit,
) {
    var open by rememberSaveable { mutableStateOf(true) }
    Surface(color = MaterialTheme.colorScheme.surfaceContainerLow, modifier = Modifier.fillMaxWidth()) {
        Column {
            Row(Modifier.fillMaxWidth().clickable { open = !open }.padding(horizontal = 16.dp, vertical = 4.dp), verticalAlignment = Alignment.CenterVertically) {
                Icon(Icons.Outlined.Schedule, null, Modifier.size(16.dp), tint = MaterialTheme.colorScheme.onSurfaceVariant)
                Spacer(Modifier.width(8.dp))
                Text(stringResource(R.string.queue_title, queued.size), style = MaterialTheme.typography.labelLarge, modifier = Modifier.weight(1f))
                Icon(if (open) Icons.Outlined.ExpandLess else Icons.Outlined.ExpandMore, contentDescription = stringResource(if (open) R.string.collapse else R.string.expand))
            }
            AnimatedVisibility(open) {
                LazyColumn(Modifier.heightIn(max = QUEUE_MAX_HEIGHT)) {
                    items(queued, key = { it.id }) { item ->
                        Row(Modifier.fillMaxWidth().padding(start = 16.dp, end = 4.dp), verticalAlignment = Alignment.CenterVertically) {
                            Text(item.preview, style = MaterialTheme.typography.bodyMedium, maxLines = 2, overflow = TextOverflow.Ellipsis, modifier = Modifier.weight(1f))
                            IconButton(onClick = { onEdit(item) }) { Icon(Icons.Outlined.Edit, contentDescription = stringResource(R.string.queue_edit)) }
                            if (canSendNow) {
                                IconButton(onClick = { onSendNow(item) }) { Icon(Icons.Filled.Bolt, contentDescription = stringResource(R.string.queue_send_now)) }
                            }
                            IconButton(onClick = { onRemove(item) }) { Icon(Icons.Outlined.DeleteOutline, contentDescription = stringResource(R.string.queue_remove)) }
                        }
                    }
                }
            }
            HorizontalDivider()
        }
    }
}

/**
 * Pending approvals and questions pinned above the composer (UX §8.2), so a card scrolled out of
 * view is not missed. Tapping goes to the card (a question opens its sheet).
 */
@Composable
fun InteractionBanner(pending: List<Interaction>, onOpen: (Interaction) -> Unit) {
    val first = pending.firstOrNull() ?: return
    val res = LocalResources.current
    val question = first.request is InteractionRequest.Question
    val color = if (question) MaterialTheme.statusColors.needsInput else MaterialTheme.statusColors.needsApproval
    Surface(color = color.copy(alpha = BANNER_ALPHA), modifier = Modifier.fillMaxWidth().clickable { onOpen(first) }) {
        Row(Modifier.padding(horizontal = 16.dp, vertical = 8.dp), verticalAlignment = Alignment.CenterVertically) {
            Column(Modifier.weight(1f)) {
                Text(
                    stringResource(if (question) R.string.banner_question else R.string.banner_approval),
                    style = MaterialTheme.typography.labelLarge,
                    color = color,
                )
                Text(InteractionTexts.summary(res, first.request), style = MaterialTheme.typography.bodyMedium, maxLines = 1, overflow = TextOverflow.Ellipsis)
            }
            if (pending.size > 1) Text(stringResource(R.string.banner_more, pending.size - 1), style = MaterialTheme.typography.labelSmall)
            Spacer(Modifier.width(8.dp))
            Text(stringResource(if (question) R.string.question_answer else R.string.banner_show), style = MaterialTheme.typography.labelLarge, color = MaterialTheme.colorScheme.primary)
        }
    }
}

/**
 * Text the harness asked to put into the composer (`composer/insert`) that waits for the user
 * (the composer had text, or it arrived with the catch-up or while the screen was not shown):
 * into the composer, in place of its text or after it, or dismissed. Never sent by itself.
 */
@Composable
fun ComposerInsertOffer(text: String, composerEmpty: Boolean, onReplace: () -> Unit, onAppend: () -> Unit, onDismiss: () -> Unit) {
    Surface(color = MaterialTheme.colorScheme.secondaryContainer, shape = RoundedCornerShape(12.dp), modifier = Modifier.fillMaxWidth().padding(horizontal = 8.dp, vertical = 4.dp)) {
        Column(Modifier.padding(start = 12.dp, end = 4.dp, top = 8.dp)) {
            Text(stringResource(R.string.composer_insert_title), style = MaterialTheme.typography.labelLarge, color = MaterialTheme.colorScheme.onSecondaryContainer)
            Text(text, style = MaterialTheme.typography.bodyMedium, maxLines = OFFER_PREVIEW_LINES, overflow = TextOverflow.Ellipsis, modifier = Modifier.padding(end = 8.dp, top = 2.dp))
            Row(Modifier.fillMaxWidth(), horizontalArrangement = Arrangement.End) {
                TextButton(onClick = onDismiss) { Text(stringResource(R.string.composer_insert_dismiss)) }
                if (composerEmpty) {
                    TextButton(onClick = onReplace) { Text(stringResource(R.string.composer_insert_put)) }
                } else {
                    TextButton(onClick = onAppend) { Text(stringResource(R.string.composer_insert_append)) }
                    TextButton(onClick = onReplace) { Text(stringResource(R.string.composer_insert_replace)) }
                }
            }
        }
    }
}

/**
 * The harness loads a project's own resources (extensions, prompts, skills) only in projects the
 * user trusts (`features.projectTrust`): asked here, per project, until decided (never decided by
 * the app). The status sheet changes it later.
 */
@Composable
fun ProjectTrustBanner(harnessName: String, onTrust: () -> Unit, onDistrust: () -> Unit) {
    Surface(color = MaterialTheme.statusColors.needsInput.copy(alpha = BANNER_ALPHA), modifier = Modifier.fillMaxWidth()) {
        Column(Modifier.padding(start = 16.dp, end = 8.dp, top = 8.dp)) {
            Text(stringResource(R.string.trust_title, harnessName), style = MaterialTheme.typography.labelLarge)
            Text(stringResource(R.string.trust_body, harnessName), style = MaterialTheme.typography.bodySmall, modifier = Modifier.padding(end = 8.dp, top = 2.dp))
            Row(Modifier.fillMaxWidth(), horizontalArrangement = Arrangement.End) {
                TextButton(onClick = onDistrust) { Text(stringResource(R.string.trust_no)) }
                TextButton(onClick = onTrust) { Text(stringResource(R.string.trust_yes)) }
            }
        }
    }
}

private const val BANNER_ALPHA = 0.16f

/** Lines of an offered composer text shown before the ellipsis: enough to recognise it. */
private const val OFFER_PREVIEW_LINES = 3

/** Height of the queue list before it scrolls: about four messages. */
private val QUEUE_MAX_HEIGHT = 200.dp
