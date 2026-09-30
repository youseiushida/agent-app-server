package dev.aas.android.domain

import dev.aas.android.data.Draft
import dev.aas.android.data.DraftImage
import dev.aas.android.data.UploadedImage
import dev.aas.android.protocol.Attachment
import dev.aas.android.protocol.Harness
import dev.aas.android.protocol.Item
import dev.aas.android.protocol.ItemStatus
import dev.aas.android.protocol.Thread
import dev.aas.android.protocol.Turn
import dev.aas.android.protocol.TurnStatus
import dev.aas.android.protocol.Workspace
import dev.aas.android.protocol.WorkspaceSpec

/** 「ここから分岐」 and 「このプロンプトを編集」 of a turn. */
data class ForkChoices(val here: Boolean, val editPrompt: Boolean) {
    val any: Boolean get() = here || editPrompt

    companion object {
        val None = ForkChoices(here = false, editPrompt = false)
    }
}

/** What the end of a turn whose resume failed offers (`Turn.error.kind` `resumeFailed`). */
data class ResumeFailedChoices(val retry: Boolean, val fork: Boolean)

/** 「実装する」 and 「新しいスレッドで実装」 of a proposed plan. */
data class PlanChoices(val implement: Boolean, val newThread: Boolean) {
    val any: Boolean get() = implement || newThread

    companion object {
        val None = PlanChoices(implement = false, newThread = false)
    }
}

/**
 * Which actions on a thread's turns and items apply, from the protocol's explicit state only:
 * the harness's capabilities and features, the turns' `forkable` anchors, statuses and error
 * kinds, and the items' `backgroundable` flag (protocol.md §3.1 「ハーネスの機能」, §4
 * `thread/fork`).
 */
object ThreadActions {
    /**
     * The fork choices of [turn] (protocol.md §4 `thread/fork`): the harness forks, the thread has
     * a native session and no turn runs. 「ここから分岐」 (`atTurnId`): the thread's last turn
     * (a fork of the whole session), or any turn with a recorded anchor on a harness with
     * `forkAtTurn`. 「このプロンプトを編集」 (`before: true`, the prompt into the new thread's
     * composer): `forkAtTurn`, a turn with a prompt ([hasPrompt]), and an anchor unless it is the
     * thread's first turn (before the first turn is a new session).
     */
    fun fork(thread: Thread?, harness: Harness?, turn: Turn, hasPrompt: Boolean): ForkChoices {
        if (thread == null || harness == null || thread.archived || !harness.capabilities.fork) return ForkChoices.None
        if (thread.nativeSessionId == null) return ForkChoices.None
        if (thread.lastTurn?.status == TurnStatus.Running || turn.status == TurnStatus.Running) return ForkChoices.None
        val atTurn = harness.features.forkAtTurn
        val last = thread.lastTurn?.id == turn.id
        return ForkChoices(
            here = last || (atTurn && turn.forkable),
            editPrompt = atTurn && hasPrompt && (turn.index == 0 || turn.forkable),
        )
    }

    /**
     * The choices after a failed resume ([turn] ended `resumeFailed`), on the thread's latest turn
     * only: 再試行 sends the turn's prompt again ([hasPrompt]); 新しいスレッドに分岐 when the
     * harness forks sessions another process holds (`features.forkWhileHeld`) and the thread has
     * a session to fork. `null` for any other turn.
     */
    fun resumeFailed(thread: Thread?, harness: Harness?, turn: Turn, hasPrompt: Boolean): ResumeFailedChoices? {
        if (turn.error?.kind != ErrorTexts.RESUME_FAILED || thread == null || thread.archived) return null
        if (thread.lastTurn?.id != turn.id) return null
        val fork = harness != null && harness.capabilities.fork && harness.features.forkWhileHeld && thread.nativeSessionId != null
        return ResumeFailedChoices(retry = hasPrompt, fork = fork)
    }

    /**
     * Where 新しいスレッドに分岐 after the failed resume of [failed] branches (design.md §9.6
     * 「持たれているセッション」). With `forkAtTurn`: at the last turn before [failed] that the agent
     * ran, when its anchor was recorded and every turn between it and [failed] ended `resumeFailed`
     * too (none of them reached the agent). The fork then carries its own anchor, which a harness
     * that cannot read a held session needs (Devin forks a held session only at a recorded step),
     * and the failed prompts stay out of the new thread's history. `null`: a fork of the whole
     * session (no `forkAtTurn`, the turn is not in [turns], no such turn, or its anchor was not
     * recorded; a fork at an earlier anchor would silently drop a turn that ran).
     */
    fun resumeFailedForkPoint(harness: Harness?, turns: List<Turn>, failed: Turn): Turn? {
        if (harness?.features?.forkAtTurn != true) return null
        var i = turns.indexOfFirst { it.id == failed.id } - 1
        if (i < -1) return null
        while (i >= 0 && turns[i].error?.kind == ErrorTexts.RESUME_FAILED) i--
        return turns.getOrNull(i)?.takeIf { it.forkable }
    }

    /**
     * The choices of a proposed plan ([Item.ProposedPlan], `features.planMode`): on the latest
     * turn's completed plan once that turn ended. 「実装する」 with the harness's own text
     * (`implementPrompt`); 「新しいスレッドで実装」 with its preamble (`newThreadPreamble`), in the
     * thread's own working tree ([newThreadWorkspace]).
     */
    fun proposedPlan(thread: Thread?, harness: Harness?, item: Item.ProposedPlan): PlanChoices {
        val feature = harness?.features?.planMode ?: return PlanChoices.None
        if (thread == null || thread.archived) return PlanChoices.None
        val lastTurn = thread.lastTurn ?: return PlanChoices.None
        if (lastTurn.id != item.turnId || lastTurn.status == TurnStatus.Running || item.status != ItemStatus.Completed) return PlanChoices.None
        return PlanChoices(
            implement = feature.implementPrompt != null,
            newThread = feature.newThreadPreamble != null,
        )
    }

    /**
     * Where the new thread implementing [thread]'s plan works: where [thread] works, since the plan
     * was written about that working tree. The project's folder for a thread there (`local`, which
     * every daemon takes); otherwise the thread's own workspace (`thread`: its worktree, shared
     * like a fork's; protocol.md §4 `thread/create`).
     */
    fun newThreadWorkspace(thread: Thread): WorkspaceSpec =
        if (thread.workspace is Workspace.Local) WorkspaceSpec.Local else WorkspaceSpec.Thread(thread.id)

    /** 裏に回す applies: the harness said this running item can move now, and it offers the move. */
    fun canMoveToBackground(harness: Harness?, item: Item): Boolean =
        harness?.features?.moveToBackground == true && item.backgroundable && item.status == ItemStatus.InProgress

    /** The first input of [turn]: its user message as a composer draft (text, mentions, images), or `null`. */
    fun promptOf(turn: Turn, items: List<Item>): Draft? {
        val message = items.firstOrNull { it.turnId == turn.id && it is Item.UserMessage } as? Item.UserMessage ?: return null
        val images = message.attachments.filterIsInstance<Attachment.Image>().map { DraftImage(null, UploadedImage(it.blobId, it.mime, null)) }
        return Draft(message.text, message.mentions.map { it.path }.toSet(), images)
    }

    /** The text a new thread implementing [plan] starts with: the harness's preamble, a blank line, the plan. */
    fun newThreadInput(preamble: String, plan: Item.ProposedPlan): String = preamble + PLAN_SEPARATOR + plan.text

    /** Between the preamble and the plan (protocol.md §3.1 `planMode.newThreadPreamble`). */
    private const val PLAN_SEPARATOR = "\n\n"
}
