package dev.aas.android.domain

import dev.aas.android.protocol.Interaction
import dev.aas.android.protocol.Project
import dev.aas.android.protocol.Thread
import dev.aas.android.sync.OutboxEntry
import dev.aas.android.sync.WorkspaceState
import dev.aas.android.protocol.JsonKeys
import dev.aas.android.protocol.Methods
import kotlinx.serialization.json.JsonPrimitive
import kotlinx.serialization.json.contentOrNull

/** A pending interaction with the context the inbox shows. */
data class InboxInteraction(
    val interaction: Interaction,
    val thread: Thread?,
    val project: Project?,
    /** An `interaction/respond` for it is still in the outbox (answered offline, not yet sent). */
    val responsePending: Boolean,
    /** The title of the background task that asked (`backgroundTaskId`), when stored on this device. */
    val backgroundTaskTitle: String? = null,
)

/** A thread listed in the inbox with its status. */
data class InboxThread(val thread: Thread, val project: Project?, val activity: ThreadActivity, val unread: Boolean)

/**
 * The 要対応 tab (UX §8.1: pending approvals and questions across threads, errors, unread):
 *
 * * [interactions]: every pending interaction, oldest first (answer them in the order asked).
 * * [errors]: unread threads whose last turn failed.
 * * [running]: threads with a running turn (to keep an eye on).
 * * [unread]: other unread threads, most recent activity first.
 *
 * Archived threads are left out. [badgeCount] (shown on the tab) counts what needs the user:
 * pending interactions and unread errors.
 */
data class InboxModel(
    val interactions: List<InboxInteraction>,
    val errors: List<InboxThread>,
    val running: List<InboxThread>,
    val unread: List<InboxThread>,
) {
    val badgeCount: Int get() = interactions.size + errors.size

    val isEmpty: Boolean get() = interactions.isEmpty() && errors.isEmpty() && running.isEmpty() && unread.isEmpty()

    companion object {
        val Empty = InboxModel(emptyList(), emptyList(), emptyList(), emptyList())

        fun build(workspace: WorkspaceState, outbox: List<OutboxEntry>): InboxModel {
            val projects = workspace.projects.associateBy { it.id }
            val threads = workspace.threads.associateBy { it.thread.id }
            val answering = respondingInteractionIds(outbox)
            val interactions = workspace.pendingInteractions.map { interaction ->
                val thread = threads[interaction.threadId]?.thread
                InboxInteraction(interaction, thread, thread?.let { projects[it.projectId] }, interaction.id in answering)
            }
            val errors = ArrayList<InboxThread>()
            val running = ArrayList<InboxThread>()
            val unread = ArrayList<InboxThread>()
            for (entry in workspace.threads) {
                val thread = entry.thread
                if (thread.archived) continue
                val activity = ThreadActivity.of(thread, workspace.pendingInteractions)
                val row = InboxThread(thread, projects[thread.projectId], activity, entry.unread)
                when {
                    // Threads waiting for an answer are already listed through their interaction.
                    activity == ThreadActivity.NeedsApproval || activity == ThreadActivity.NeedsInput -> Unit
                    activity == ThreadActivity.Error && entry.unread -> errors += row
                    activity.working -> running += row
                    entry.unread -> unread += row
                }
            }
            // workspace.threads is already sorted by last activity, newest first.
            return InboxModel(interactions, errors, running, unread)
        }

        /** Interactions with an `interaction/respond` still in the outbox. */
        fun respondingInteractionIds(outbox: List<OutboxEntry>): Set<String> = outbox
            .filter { it.method == Methods.InteractionRespond.name }
            .mapNotNull { (it.params[JsonKeys.INTERACTION_ID] as? JsonPrimitive)?.contentOrNull }
            .toSet()
    }
}
