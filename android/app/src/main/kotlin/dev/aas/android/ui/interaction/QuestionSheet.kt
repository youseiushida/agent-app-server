package dev.aas.android.ui.interaction

import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.imePadding
import androidx.compose.foundation.layout.navigationBarsPadding
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.selection.selectable
import androidx.compose.foundation.selection.toggleable
import androidx.compose.foundation.verticalScroll
import androidx.compose.material3.Button
import androidx.compose.material3.Checkbox
import androidx.compose.material3.ExperimentalMaterial3Api
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.ModalBottomSheet
import androidx.compose.material3.OutlinedTextField
import androidx.compose.material3.RadioButton
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.material3.rememberModalBottomSheetState
import androidx.compose.runtime.Composable
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableIntStateOf
import androidx.compose.runtime.mutableStateMapOf
import androidx.compose.runtime.saveable.listSaver
import androidx.compose.runtime.snapshots.SnapshotStateMap
import androidx.compose.runtime.saveable.rememberSaveable
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.res.stringResource
import androidx.compose.ui.semantics.Role
import androidx.compose.ui.unit.dp
import dev.aas.android.R
import dev.aas.android.protocol.Interaction
import dev.aas.android.protocol.InteractionRequest
import dev.aas.android.protocol.InteractionResolution
import dev.aas.android.protocol.InteractionStatus
import dev.aas.android.protocol.Question

/**
 * The answer sheet of a question interaction (UX §8.1: one page per question, choices, free
 * text, skip). It closes by itself when the interaction stops being pending — answered on
 * another device, or withdrawn by the harness (`interaction/expired`, `harnessCancelled`).
 * There is no countdown (design.md 範囲外).
 */
@OptIn(ExperimentalMaterial3Api::class)
@Composable
fun QuestionSheet(
    interaction: Interaction,
    onRespond: (InteractionResolution) -> Unit,
    onDismiss: () -> Unit,
) {
    val request = interaction.request as? InteractionRequest.Question ?: return
    LaunchedEffect(interaction.status) {
        if (interaction.status != InteractionStatus.Pending) onDismiss()
    }
    val sheetState = rememberModalBottomSheetState(skipPartiallyExpanded = true)
    var page by rememberSaveable(interaction.id) { mutableIntStateOf(0) }
    // Drafts survive rotation and process death while the sheet is open.
    val answers = rememberSaveable(interaction.id, saver = AnswersSaver) { mutableStateMapOf() }
    val questions = request.questions
    ModalBottomSheet(onDismissRequest = onDismiss, sheetState = sheetState) {
        Column(
            Modifier
                .fillMaxWidth()
                .padding(horizontal = 24.dp)
                .navigationBarsPadding()
                .imePadding()
                .verticalScroll(rememberScrollState()),
        ) {
            Text(request.title, style = MaterialTheme.typography.titleLarge)
            if (questions.size > 1) {
                Text(stringResource(R.string.question_page, page + 1, questions.size), style = MaterialTheme.typography.labelMedium)
            }
            Spacer(Modifier.height(16.dp))
            val question = questions.getOrNull(page)
            if (question != null) {
                QuestionPage(question, answers[question.id] ?: DraftAnswer()) { answers[question.id] = it }
            }
            Spacer(Modifier.height(16.dp))
            val complete = question == null || QuestionAnswers.isComplete(question, answers[question.id] ?: DraftAnswer())
            Row(verticalAlignment = Alignment.CenterVertically) {
                TextButton(onClick = {
                    onRespond(InteractionResolution.Dismissed)
                    onDismiss()
                }) { Text(stringResource(R.string.question_skip)) }
                Spacer(Modifier.weight(1f))
                if (page > 0) TextButton(onClick = { page-- }) { Text(stringResource(R.string.question_back)) }
                if (page < questions.lastIndex) {
                    Button(onClick = { page++ }, enabled = complete) { Text(stringResource(R.string.question_next)) }
                } else {
                    Button(
                        onClick = {
                            onRespond(QuestionAnswers.resolution(questions, answers.toMap()))
                            onDismiss()
                        },
                        enabled = complete && questions.all { QuestionAnswers.isComplete(it, answers[it.id] ?: DraftAnswer()) },
                    ) { Text(stringResource(R.string.question_send)) }
                }
            }
            Spacer(Modifier.height(16.dp))
        }
    }
}

@Composable
private fun QuestionPage(question: Question, answer: DraftAnswer, onChange: (DraftAnswer) -> Unit) {
    question.header?.let { Text(it, style = MaterialTheme.typography.labelLarge, color = MaterialTheme.colorScheme.primary) }
    Text(question.prompt, style = MaterialTheme.typography.bodyLarge)
    Spacer(Modifier.height(8.dp))
    for (choice in question.choices) {
        val selected = choice.id in answer.choiceIds
        val rowModifier = if (question.multiSelect) {
            Modifier.toggleable(value = selected, role = Role.Checkbox) { onChange(answer.toggle(choice.id, true)) }
        } else {
            Modifier.selectable(selected = selected, role = Role.RadioButton) { onChange(answer.toggle(choice.id, false)) }
        }
        Row(rowModifier.fillMaxWidth().padding(vertical = 6.dp), verticalAlignment = Alignment.CenterVertically) {
            if (question.multiSelect) Checkbox(checked = selected, onCheckedChange = null) else RadioButton(selected = selected, onClick = null)
            Column(Modifier.padding(start = 12.dp)) {
                Text(choice.label, style = MaterialTheme.typography.bodyLarge)
                choice.description?.let { Text(it, style = MaterialTheme.typography.bodySmall, color = MaterialTheme.colorScheme.onSurfaceVariant) }
            }
        }
    }
    if (question.allowFreeText) {
        Spacer(Modifier.height(8.dp))
        OutlinedTextField(
            value = answer.text,
            onValueChange = { onChange(answer.copy(text = it)) },
            label = { Text(stringResource(if (question.choices.isEmpty()) R.string.question_answer_text else R.string.question_other)) },
            placeholder = question.placeholder?.let { { Text(it) } },
            modifier = Modifier.fillMaxWidth(),
            minLines = 2,
        )
    }
}

/** Saves the drafts as `[questionId, choiceIds, text, …]` (types a Bundle holds). */
private val AnswersSaver = listSaver<SnapshotStateMap<String, DraftAnswer>, Any>(
    save = { map -> map.flatMap { (id, draft) -> listOf(id, ArrayList(draft.choiceIds), draft.text) } },
    restore = { saved ->
        mutableStateMapOf<String, DraftAnswer>().apply {
            for (chunk in saved.chunked(SAVED_FIELDS)) {
                val (id, choices, text) = chunk
                @Suppress("UNCHECKED_CAST")
                put(id as String, DraftAnswer((choices as List<String>).toSet(), text as String))
            }
        }
    },
)

/** Entries per draft in [AnswersSaver]'s list. */
private const val SAVED_FIELDS = 3
