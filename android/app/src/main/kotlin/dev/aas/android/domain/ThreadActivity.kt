package dev.aas.android.domain

import dev.aas.android.protocol.Interaction
import dev.aas.android.protocol.InteractionRequest
import dev.aas.android.protocol.InteractionStatus
import dev.aas.android.protocol.Thread
import dev.aas.android.protocol.ThreadStatus
import dev.aas.android.protocol.TurnStatus

/**
 * The five-word status vocabulary shared by every list, chip and notification
 * (docs/ux/codex-desktop.md §1.3, §8.1): 承認が必要 / 入力が必要 / エラー / 実行中 / 待機中.
 * Unread is a separate flag (per device).
 *
 * Every value is derived from explicit protocol state: pending interactions and their
 * `request.kind`, `Thread.status`, `lastTurn.status` and `lastError`.
 */
enum class ThreadActivity {
    /** A pending approval (`request.kind = approval`). */
    NeedsApproval,

    /** A pending question (`request.kind = question`). */
    NeedsInput,

    /** The last turn failed, or the thread recorded an error after its last turn started. */
    Error,

    /** A turn runs or is about to (`queued`, `starting`, `running`, `stopping`). */
    Running,

    /** Nothing happens (`idle` / `ready`). */
    Idle,
    ;

    /** Needs the user (the 要対応 tab counts these). */
    val needsAction: Boolean get() = this == NeedsApproval || this == NeedsInput || this == Error

    companion object {
        /**
         * The activity of [thread]. [pendingOfThread] are the pending interactions of the
         * workspace (other threads' entries are ignored), which arrive on the workspace stream
         * before or after the summary's `pendingInteractions` count changes.
         */
        fun of(thread: Thread, pendingOfThread: List<Interaction>): ThreadActivity {
            val pending = pendingOfThread.filter { it.threadId == thread.id && it.status == InteractionStatus.Pending }
            return when {
                pending.any { it.request is InteractionRequest.Approval } -> NeedsApproval
                pending.any { it.request is InteractionRequest.Question } -> NeedsInput
                // An interaction of a kind this client does not know still needs the user.
                pending.isNotEmpty() -> NeedsApproval
                isRunning(thread) -> Running
                hasError(thread) -> Error
                else -> Idle
            }
        }

        fun isRunning(thread: Thread): Boolean =
            thread.lastTurn?.status == TurnStatus.Running ||
                thread.status == ThreadStatus.Queued ||
                thread.status == ThreadStatus.Starting ||
                thread.status == ThreadStatus.Running ||
                thread.status == ThreadStatus.Stopping

        fun hasError(thread: Thread): Boolean {
            val last = thread.lastTurn
            if (last?.status == TurnStatus.Failed) return true
            val error = thread.lastError ?: return false
            return last == null || error.at >= last.startedAt
        }
    }
}
