package dev.aas.android.ui.interaction

import androidx.compose.foundation.horizontalScroll
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.material.icons.Icons
import androidx.compose.material3.AlertDialog
import androidx.compose.material3.Button
import androidx.compose.material3.Card
import androidx.compose.material3.CardDefaults
import androidx.compose.material3.DropdownMenu
import androidx.compose.material3.DropdownMenuItem
import androidx.compose.material3.Icon
import androidx.compose.material3.IconButton
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.OutlinedButton
import androidx.compose.material3.OutlinedTextField
import androidx.compose.material3.Surface
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.runtime.Composable
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.saveable.rememberSaveable
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.res.stringResource
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.unit.dp
import dev.aas.android.R
import dev.aas.android.protocol.AasJson
import dev.aas.android.protocol.ApprovalOption
import dev.aas.android.protocol.ApprovalOptionKind
import dev.aas.android.protocol.FileChangeKind
import dev.aas.android.protocol.Interaction
import dev.aas.android.protocol.InteractionRequest
import dev.aas.android.protocol.InteractionResolution
import dev.aas.android.protocol.Subject
import dev.aas.android.ui.common.LocalAppContainer
import dev.aas.android.ui.common.LocalAppPolicy
import dev.aas.android.ui.icons.MoreHoriz
import dev.aas.android.ui.theme.codeStyle
import dev.aas.android.ui.theme.statusColors
import kotlinx.coroutines.delay
import kotlinx.serialization.json.JsonElement

/**
 * A pending approval or question, answered in place (the 要対応 tab and the thread screen).
 *
 * * Approval: the request, what it is about, and two large buttons 拒否 / 許可 (the one-time
 *   allow when offered); every other option (この会話で許可, 常に許可, 理由を付けて拒否, 中止 …)
 *   is in the ⋯ menu. The buttons arm only after [dev.aas.android.AppPolicy.interactionArmDelayMs].
 * * Question: the first prompt and 回答する (opens [QuestionSheet]) / スキップ (`dismissed`).
 * * [responsePending]: an answer is in the outbox (sent when connected); buttons are disabled.
 */
@Composable
fun InteractionCard(
    interaction: Interaction,
    responsePending: Boolean,
    onRespond: (InteractionResolution) -> Unit,
    onOpenQuestion: () -> Unit,
    modifier: Modifier = Modifier,
    header: (@Composable () -> Unit)? = null,
) {
    val request = interaction.request
    val accent = if (request is InteractionRequest.Question) MaterialTheme.statusColors.needsInput else MaterialTheme.statusColors.needsApproval
    Card(
        modifier = modifier.fillMaxWidth(),
        colors = CardDefaults.cardColors(containerColor = accent.copy(alpha = CARD_TINT_ALPHA)),
    ) {
        Column(Modifier.padding(16.dp)) {
            header?.invoke()
            when (request) {
                is InteractionRequest.Approval -> ApprovalContent(interaction.id, request, responsePending, onRespond)
                is InteractionRequest.Question -> QuestionSummary(request, responsePending, onOpenQuestion, onSkip = { onRespond(InteractionResolution.Dismissed) })
                is InteractionRequest.Unknown -> UnknownContent(request, responsePending, onDismiss = { onRespond(InteractionResolution.Dismissed) })
            }
        }
    }
}

@Composable
private fun ApprovalContent(
    interactionId: String,
    request: InteractionRequest.Approval,
    responsePending: Boolean,
    onRespond: (InteractionResolution) -> Unit,
) {
    val armDelay = LocalAppContainer.current.policy.interactionArmDelayMs
    var armed by remember(interactionId) { mutableStateOf(armDelay == 0L) }
    LaunchedEffect(interactionId) {
        delay(armDelay)
        armed = true
    }
    var feedbackFor by rememberSaveable { mutableStateOf<String?>(null) }
    var menuOpen by remember { mutableStateOf(false) }
    val allow = ApprovalChoices.primaryAllow(request.options)
    val deny = ApprovalChoices.primaryDeny(request.options)
    val others = request.options.filter { it != allow && it != deny }
    val enabled = armed && !responsePending

    fun choose(option: ApprovalOption) {
        if (option.kind == ApprovalOptionKind.DenyWithFeedback) feedbackFor = option.id else onRespond(InteractionResolution.Approval(option.id))
    }

    Text(request.title, style = MaterialTheme.typography.titleMedium, fontWeight = FontWeight.SemiBold)
    request.detail?.takeIf { it.isNotBlank() }?.let {
        Spacer(Modifier.height(4.dp))
        Text(it, style = MaterialTheme.typography.bodyMedium)
    }
    Spacer(Modifier.height(8.dp))
    SubjectView(request.subject)
    Spacer(Modifier.height(12.dp))
    if (responsePending) {
        Text(stringResource(R.string.interaction_answer_pending), style = MaterialTheme.typography.labelMedium, color = MaterialTheme.colorScheme.onSurfaceVariant)
        Spacer(Modifier.height(8.dp))
    }
    Row(verticalAlignment = Alignment.CenterVertically, horizontalArrangement = Arrangement.spacedBy(8.dp)) {
        if (deny != null) {
            OutlinedButton(onClick = { choose(deny) }, enabled = enabled, modifier = Modifier.weight(1f)) {
                Text(ApprovalChoices.label(deny), maxLines = 1, overflow = TextOverflow.Ellipsis)
            }
        }
        if (allow != null) {
            Button(onClick = { choose(allow) }, enabled = enabled, modifier = Modifier.weight(1f)) {
                Text(ApprovalChoices.label(allow), maxLines = 1, overflow = TextOverflow.Ellipsis)
            }
        }
        if (others.isNotEmpty()) {
            Box {
                IconButton(onClick = { menuOpen = true }, enabled = enabled) {
                    Icon(Icons.Outlined.MoreHoriz, contentDescription = stringResource(R.string.interaction_more_options))
                }
                DropdownMenu(expanded = menuOpen, onDismissRequest = { menuOpen = false }) {
                    for (option in others) {
                        DropdownMenuItem(
                            text = {
                                Column {
                                    Text(ApprovalChoices.label(option))
                                    if (option.label.isNotBlank()) {
                                        Text(option.label, style = MaterialTheme.typography.bodySmall, color = MaterialTheme.colorScheme.onSurfaceVariant)
                                    }
                                }
                            },
                            onClick = {
                                menuOpen = false
                                choose(option)
                            },
                        )
                    }
                }
            }
        }
    }
    feedbackFor?.let { optionId ->
        FeedbackDialog(
            onSend = { text ->
                feedbackFor = null
                onRespond(InteractionResolution.Approval(optionId, feedback = text))
            },
            onDismiss = { feedbackFor = null },
        )
    }
}

@Composable
private fun FeedbackDialog(onSend: (String) -> Unit, onDismiss: () -> Unit) {
    var text by rememberSaveable { mutableStateOf("") }
    AlertDialog(
        onDismissRequest = onDismiss,
        title = { Text(stringResource(R.string.interaction_feedback_title)) },
        text = {
            OutlinedTextField(
                value = text,
                onValueChange = { text = it },
                placeholder = { Text(stringResource(R.string.interaction_feedback_hint)) },
                minLines = 3,
                modifier = Modifier.fillMaxWidth(),
            )
        },
        confirmButton = { TextButton(onClick = { onSend(text.trim()) }, enabled = text.isNotBlank()) { Text(stringResource(R.string.interaction_deny_with_feedback)) } },
        dismissButton = { TextButton(onClick = onDismiss) { Text(stringResource(R.string.cancel)) } },
    )
}

/** What an approval is about: a command, file changes, a tool call, a plan, a description. */
@Composable
fun SubjectView(subject: Subject) {
    val display = LocalAppPolicy.current.display
    when (subject) {
        is Subject.Command -> {
            CodeBlock("$ ${subject.command}")
            subject.cwd?.let {
                Spacer(Modifier.height(4.dp))
                Text(stringResource(R.string.subject_cwd, it), style = MaterialTheme.typography.bodySmall, color = MaterialTheme.colorScheme.onSurfaceVariant)
            }
        }
        is Subject.FileChanges -> Column {
            Text(stringResource(R.string.subject_file_count, subject.changes.size), style = MaterialTheme.typography.labelLarge)
            for (change in subject.changes.take(display.approvalFiles)) {
                val counts = listOfNotNull(change.added?.let { "+$it" }, change.removed?.let { "-$it" }).joinToString(" ")
                Text(
                    "${fileKindMark(change.kind)} ${change.path}${change.movePath?.let { " → $it" }.orEmpty()}  $counts",
                    style = MaterialTheme.codeStyle,
                    maxLines = 1,
                    overflow = TextOverflow.Ellipsis,
                )
            }
            if (subject.changes.size > display.approvalFiles) {
                Text(stringResource(R.string.subject_more_files, subject.changes.size - display.approvalFiles), style = MaterialTheme.typography.bodySmall)
            }
        }
        is Subject.Tool -> Column {
            Text(stringResource(R.string.subject_tool, subject.name), style = MaterialTheme.typography.labelLarge)
            subject.input?.let {
                Spacer(Modifier.height(4.dp))
                CodeBlock(prettyJson(it), maxLines = display.approvalInputLines)
            }
        }
        is Subject.Plan -> Text(subject.text, style = MaterialTheme.typography.bodyMedium)
        is Subject.Permissions -> Text(subject.description, style = MaterialTheme.typography.bodyMedium)
        is Subject.Other -> Text(subject.description, style = MaterialTheme.typography.bodyMedium)
        is Subject.Unknown -> Text(stringResource(R.string.subject_unknown, subject.type), style = MaterialTheme.typography.bodyMedium)
    }
}

/** Monospace text on a tinted background, scrolling sideways instead of wrapping. */
@Composable
fun CodeBlock(text: String, modifier: Modifier = Modifier, maxLines: Int = Int.MAX_VALUE) {
    Surface(color = MaterialTheme.colorScheme.surfaceContainerHighest, shape = RoundedCornerShape(8.dp), modifier = modifier.fillMaxWidth()) {
        Text(
            text,
            style = MaterialTheme.codeStyle,
            maxLines = maxLines,
            overflow = TextOverflow.Ellipsis,
            softWrap = false,
            modifier = Modifier.horizontalScroll(rememberScrollState()).padding(horizontal = 12.dp, vertical = 8.dp),
        )
    }
}

@Composable
private fun QuestionSummary(
    request: InteractionRequest.Question,
    responsePending: Boolean,
    onOpen: () -> Unit,
    onSkip: () -> Unit,
) {
    Text(request.title, style = MaterialTheme.typography.titleMedium, fontWeight = FontWeight.SemiBold)
    request.questions.firstOrNull()?.let {
        Spacer(Modifier.height(4.dp))
        Text(it.prompt, style = MaterialTheme.typography.bodyMedium, maxLines = 3, overflow = TextOverflow.Ellipsis)
    }
    if (request.questions.size > 1) {
        Text(stringResource(R.string.question_count, request.questions.size), style = MaterialTheme.typography.bodySmall, color = MaterialTheme.colorScheme.onSurfaceVariant)
    }
    Spacer(Modifier.height(12.dp))
    if (responsePending) {
        Text(stringResource(R.string.interaction_answer_pending), style = MaterialTheme.typography.labelMedium, color = MaterialTheme.colorScheme.onSurfaceVariant)
        Spacer(Modifier.height(8.dp))
    }
    Row(verticalAlignment = Alignment.CenterVertically) {
        TextButton(onClick = onSkip, enabled = !responsePending) { Text(stringResource(R.string.question_skip)) }
        Spacer(Modifier.weight(1f))
        Button(onClick = onOpen, enabled = !responsePending) { Text(stringResource(R.string.question_answer)) }
    }
}

@Composable
private fun UnknownContent(request: InteractionRequest.Unknown, responsePending: Boolean, onDismiss: () -> Unit) {
    Text(request.title.ifEmpty { stringResource(R.string.interaction_unknown_kind, request.kind) }, style = MaterialTheme.typography.titleMedium)
    Spacer(Modifier.height(4.dp))
    Text(stringResource(R.string.interaction_unknown_body), style = MaterialTheme.typography.bodyMedium)
    Spacer(Modifier.height(8.dp))
    Row {
        Spacer(Modifier.weight(1f))
        TextButton(onClick = onDismiss, enabled = !responsePending) { Text(stringResource(R.string.interaction_dismiss)) }
    }
}

/** The approval options shown as the two main buttons and their app labels. */
object ApprovalChoices {
    fun primaryAllow(options: List<ApprovalOption>): ApprovalOption? =
        options.firstOrNull { it.kind == ApprovalOptionKind.AllowOnce }
            ?: options.firstOrNull { it.kind == ApprovalOptionKind.AllowForSession }
            ?: options.firstOrNull { it.kind == ApprovalOptionKind.AllowAlways }

    fun primaryDeny(options: List<ApprovalOption>): ApprovalOption? =
        options.firstOrNull { it.kind == ApprovalOptionKind.Deny } ?: options.firstOrNull { it.kind == ApprovalOptionKind.Abort }

    @Composable
    fun label(option: ApprovalOption): String = when (option.kind) {
        ApprovalOptionKind.AllowOnce -> stringResource(R.string.approval_allow_once)
        ApprovalOptionKind.AllowForSession -> stringResource(R.string.approval_allow_session)
        ApprovalOptionKind.AllowAlways -> stringResource(R.string.approval_allow_always)
        ApprovalOptionKind.Deny -> stringResource(R.string.approval_deny)
        ApprovalOptionKind.DenyWithFeedback -> stringResource(R.string.interaction_deny_with_feedback)
        ApprovalOptionKind.Abort -> stringResource(R.string.approval_abort)
        ApprovalOptionKind.Unknown -> option.label
    }
}

private fun fileKindMark(kind: FileChangeKind): String = when (kind) {
    FileChangeKind.Add -> "A"
    FileChangeKind.Delete -> "D"
    FileChangeKind.Update -> "M"
    FileChangeKind.Move -> "R"
    FileChangeKind.Unknown -> "?"
}

private fun prettyJson(element: JsonElement): String = PrettyJson.encodeToString(JsonElement.serializer(), element)

private val PrettyJson = kotlinx.serialization.json.Json(from = AasJson) { prettyPrint = true }

private const val CARD_TINT_ALPHA = 0.10f

