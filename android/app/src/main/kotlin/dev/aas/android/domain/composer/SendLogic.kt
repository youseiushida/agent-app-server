package dev.aas.android.domain.composer

import dev.aas.android.protocol.Delivery
import dev.aas.android.protocol.HarnessCapabilities
import dev.aas.android.protocol.QueuedInput
import dev.aas.android.protocol.Thread
import dev.aas.android.protocol.TurnStatus

/** What the send button does (docs/ux/codex-desktop.md §2.5, protocol.md §4 `turn/start`). */
enum class SendAction {
    /** No turn runs: a new turn (`turn/start`, `delivery: auto`). */
    Start,

    /** A turn runs: the message waits in the daemon's queue (`delivery: queue`). */
    Queue,

    /** A turn runs and the harness can steer: the message goes into it (`delivery: steer`). */
    Steer,

    /** A turn runs and the composer is empty: stop it (`turn/interrupt`). */
    Interrupt,
}

/** What happens to a follow-up sent while a turn runs, unless the user long-presses (設定). */
enum class FollowUpDelivery { Queue, Steer }

/** Why the send button is disabled. */
enum class SendBlock {
    /** The thread is not in the local store yet. */
    NotLoaded,

    /** Archived threads take no input (`invalidState`). */
    Archived,

    /** An image is still uploading. */
    Uploading,

    /** An image failed to upload; retry or remove it first. */
    UploadFailed,

    /**
     * Images are attached but the harness does not take images (`capabilities.images`): the
     * daemon would refuse the request (`capabilityUnsupported`). Remove them first (UX §2.1).
     */
    ImagesUnsupported,

    /** Nothing to send. */
    Empty,

    /**
     * The chosen model does not run in the chosen permission mode (`Model.permissionModes`): the
     * daemon would refuse the thread (`invalidParams`). Choose another mode or model first.
     */
    PermissionUnavailable,

    /** A `turn/interrupt` for this thread is already waiting for its answer. */
    Interrupting,
}

/** What the thread screen asks before sending (docs/android.md 25章). */
sealed interface SendConfirmation {
    /**
     * Sending starts a turn while the queue is paused, which resumes it: send (the [queued]
     * messages follow the new one), clear the queue first, or cancel.
     */
    data class PausedQueue(val queued: Int) : SendConfirmation

    /**
     * `/plan <request>` while [ahead] messages would start before the request: plan mode applies
     * from the next turn (`thread/update`, protocol.md §4), so they would run in plan mode too.
     * [canClear]: all of them wait in the daemon's queue, so clearing it first (`queue/remove`,
     * as for [PausedQueue]) makes the request the next turn.
     */
    data class PlanAhead(val ahead: Int, val canClear: Boolean) : SendConfirmation
}

/** The send button: its action, the long-press alternative and whether it is enabled. */
data class SendState(val primary: SendAction, val alternate: SendAction?, val blocked: SendBlock?) {
    val enabled: Boolean get() = blocked == null
}

object SendLogic {
    /**
     * A turn of the thread runs or waits to start (its `lastTurn` is `running`; the daemon
     * creates the turn when the input arrives, before the process is up).
     */
    fun turnActive(thread: Thread): Boolean = thread.lastTurn?.status == TurnStatus.Running

    /**
     * The send button for the current draft.
     *
     * * No turn: 送信 (a new turn).
     * * A turn and an empty draft: 停止.
     * * A turn and a draft: the preferred follow-up ([followUp]); long press gives the other one.
     *   Steering needs the harness capability `steer`; without it only the queue is offered.
     * * Images ([hasImages]) need the harness capability `images`.
     */
    fun state(
        thread: Thread?,
        capabilities: HarnessCapabilities?,
        hasContent: Boolean,
        uploading: Boolean,
        uploadFailed: Boolean,
        followUp: FollowUpDelivery,
        interruptPending: Boolean,
        hasImages: Boolean = false,
    ): SendState {
        if (thread == null) return SendState(SendAction.Start, null, SendBlock.NotLoaded)
        if (thread.archived) return SendState(SendAction.Start, null, SendBlock.Archived)
        val attachmentsBlock = attachmentsBlock(capabilities, uploading, uploadFailed, hasImages)
        if (!turnActive(thread)) {
            return SendState(SendAction.Start, null, attachmentsBlock ?: if (hasContent) null else SendBlock.Empty)
        }
        if (!hasContent && !uploading && !uploadFailed) {
            return SendState(SendAction.Interrupt, null, if (interruptPending) SendBlock.Interrupting else null)
        }
        val steerable = capabilities?.steer == true
        val primary = if (followUp == FollowUpDelivery.Steer && steerable) SendAction.Steer else SendAction.Queue
        val alternate = when {
            !steerable -> null
            primary == SendAction.Steer -> SendAction.Queue
            else -> SendAction.Steer
        }
        return SendState(primary, alternate, attachmentsBlock ?: if (hasContent) null else SendBlock.Empty)
    }

    /**
     * Why the attachments keep a draft from being sent, if they do: an upload still running or
     * failed, or images for a harness without `images` (the harness is known: [capabilities]).
     */
    fun attachmentsBlock(capabilities: HarnessCapabilities?, uploading: Boolean, uploadFailed: Boolean, hasImages: Boolean): SendBlock? = when {
        uploading -> SendBlock.Uploading
        uploadFailed -> SendBlock.UploadFailed
        hasImages && capabilities != null && !capabilities.images -> SendBlock.ImagesUnsupported
        else -> null
    }

    /** The `turn/start` delivery of a send action. */
    fun delivery(action: SendAction): Delivery = when (action) {
        SendAction.Start -> Delivery.Auto
        SendAction.Queue -> Delivery.Queue
        SendAction.Steer -> Delivery.Steer
        SendAction.Interrupt -> throw IllegalArgumentException("interrupt is not a delivery")
    }

    /**
     * Sending a new turn while the queue is paused (after an interrupt or a failure) also
     * resumes the queue (protocol.md §4 キューの進み方): the user confirms that, or clears the
     * queue first (docs/ux/codex-desktop.md §2.5 `pausedQueueSubmit`).
     */
    fun needsPausedQueueConfirmation(thread: Thread?, queued: List<QueuedInput>, action: SendAction): Boolean =
        action == SendAction.Start && thread != null && thread.queuePaused && queued.isNotEmpty()

    /**
     * How many of the thread's messages would start a turn before a message sent now — and so
     * after a mode change sent with it, which applies from the next turn (`thread/update`,
     * protocol.md §4). [pending] are the deliveries of this thread's messages still in the
     * outbox (they reach the daemon first, in order).
     *
     * * A turn runs: the daemon's queue, and the pending messages except steers (a steer goes
     *   into the running turn).
     * * No turn runs: the first pending message starts the next turn itself, and the rest of
     *   them and the queue (a paused queue resumes with a new turn) wait before the new message.
     *   Without pending messages the new message is the next turn.
     */
    fun messagesStartingBefore(thread: Thread, queued: List<QueuedInput>, pending: List<Delivery>): Int = when {
        turnActive(thread) -> queued.size + pending.count { it != Delivery.Steer }
        pending.isEmpty() -> 0
        else -> queued.size + pending.size - 1
    }

    /**
     * "今すぐ反映" of a queued input (`queue/steer`): while a turn runs it is steered into it
     * (needs `steer`); otherwise it starts a new turn, which any harness can do.
     */
    fun canSendQueuedNow(thread: Thread?, capabilities: HarnessCapabilities?): Boolean =
        thread != null && !thread.archived && (!turnActive(thread) || capabilities?.steer == true)
}
