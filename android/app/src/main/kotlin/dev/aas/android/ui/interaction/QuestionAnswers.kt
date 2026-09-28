package dev.aas.android.ui.interaction

import dev.aas.android.protocol.InteractionResolution
import dev.aas.android.protocol.Question
import dev.aas.android.protocol.QuestionAnswer

/** The user's answer to one question while the sheet is open. */
data class DraftAnswer(val choiceIds: Set<String> = emptySet(), val text: String = "") {
    fun toggle(choiceId: String, multiSelect: Boolean): DraftAnswer = when {
        !multiSelect -> copy(choiceIds = setOf(choiceId))
        choiceId in choiceIds -> copy(choiceIds = choiceIds - choiceId)
        else -> copy(choiceIds = choiceIds + choiceId)
    }
}

/**
 * Validation and assembly of a question interaction's answer (mirrors what the daemon checks
 * in `interaction/respond`: existing choices, one choice for single-select, free text only
 * where allowed).
 */
object QuestionAnswers {
    /** Whether [answer] may be sent for [question]. */
    fun isComplete(question: Question, answer: DraftAnswer): Boolean {
        val hasChoice = answer.choiceIds.isNotEmpty()
        val hasText = question.allowFreeText && answer.text.isNotBlank()
        return when {
            question.choices.isEmpty() && !question.allowFreeText -> true
            question.choices.isEmpty() -> hasText
            question.allowFreeText -> hasChoice || hasText
            else -> hasChoice
        }
    }

    /** The resolution for all [questions]; choices keep the order the server listed them in. */
    fun resolution(questions: List<Question>, answers: Map<String, DraftAnswer>): InteractionResolution.Question =
        InteractionResolution.Question(
            questions.map { question ->
                val draft = answers[question.id] ?: DraftAnswer()
                val known = question.choices.map { it.id }
                val chosen = known.filter { it in draft.choiceIds }
                val selected = if (question.multiSelect) chosen else chosen.take(1)
                QuestionAnswer(
                    questionId = question.id,
                    choiceIds = selected,
                    text = draft.text.trim().takeIf { question.allowFreeText && it.isNotEmpty() },
                )
            },
        )
}
