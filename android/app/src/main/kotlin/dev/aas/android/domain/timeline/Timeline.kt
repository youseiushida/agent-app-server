package dev.aas.android.domain.timeline

import dev.aas.android.domain.composer.ComposerText
import dev.aas.android.protocol.AasJson
import dev.aas.android.protocol.BackgroundTask
import dev.aas.android.protocol.BackgroundTaskStatus
import dev.aas.android.protocol.Delivery
import dev.aas.android.protocol.InputPart
import dev.aas.android.protocol.Interaction
import dev.aas.android.protocol.Item
import dev.aas.android.protocol.ItemStatus
import dev.aas.android.protocol.Methods
import dev.aas.android.protocol.Turn
import dev.aas.android.protocol.TurnStatus
import dev.aas.android.protocol.TurnStartParams
import dev.aas.android.sync.OutboxEntry
import dev.aas.android.sync.ThreadState
import kotlinx.serialization.SerializationException

/** A `turn/start` of this thread still in the outbox: shown as a message waiting to be sent. */
data class PendingInput(
    val clientRequestId: String,
    /** The message text; `null` when the stored params could not be read (shown generically). */
    val text: String?,
    val images: Int,
    val mentions: List<String>,
    val delivery: Delivery,
    val createdAtMs: Long,
    /** Non-definitive failures so far (the request is retried). */
    val failures: Int,
    val lastError: String?,
    /** The harness the message waits for (the server answered `harnessUnavailable`), if any. */
    val waitingForHarness: String? = null,
) {
    companion object {
        /** The `turn/start` entries among a thread's outbox entries, oldest first. */
        fun of(entries: List<OutboxEntry>): List<PendingInput> = entries.filter { it.method == Methods.TurnStart.name }.map { entry ->
            val params = try {
                AasJson.decodeFromJsonElement(TurnStartParams.serializer(), entry.params)
            } catch (e: SerializationException) {
                null
            } catch (e: IllegalArgumentException) {
                null
            }
            PendingInput(
                clientRequestId = entry.clientRequestId,
                // As the daemon will write it: mentions as "@path" tokens in their place.
                text = params?.input?.let { ComposerText.textOf(it) },
                images = params?.input?.count { it is InputPart.Image } ?: 0,
                mentions = params?.input?.filterIsInstance<InputPart.Mention>()?.map { it.path }.orEmpty(),
                delivery = params?.delivery ?: Delivery.Auto,
                createdAtMs = entry.createdAtMs,
                failures = entry.failures,
                lastError = entry.lastError,
                waitingForHarness = entry.waitingForHarness,
            )
        }
    }
}

/** One row of the thread's conversation (top to bottom). */
sealed interface TimelineRow {
    /** Stable across updates: the list keeps its position by it. */
    val key: String

    /** Older turns exist on the server. */
    data object LoadOlder : TimelineRow {
        override val key = "older"
    }

    /**
     * The start of a turn (its number and model). A turn the agent started by itself says why when
     * the harness told (`Turn.trigger`, e.g. "バックグラウンド作業の完了を受けて"); such a turn
     * that finished without any item is this divider alone.
     */
    data class TurnStart(val turn: Turn) : TimelineRow {
        override val key = "turn-${turn.id}"
    }

    /** A single item (a message, a plan, a notice, or an activity item outside a group). */
    data class ItemRow(val item: Item) : TimelineRow {
        override val key = "item-${item.id}"
    }

    /**
     * Consecutive activity items (reasoning, commands, file changes, tool calls) folded into one
     * block (docs/ux/codex-desktop.md §3.1). When [expanded], its items follow as
     * [GroupedItem] rows.
     */
    data class ActivityGroup(val groupKey: String, val items: List<Item>, val expanded: Boolean) : TimelineRow {
        override val key = "group-$groupKey"

        /** An item of the group is still in progress. */
        val inProgress: Boolean get() = items.any { it.status == ItemStatus.InProgress }
    }

    data class GroupedItem(val item: Item, val groupKey: String) : TimelineRow {
        override val key = "item-${item.id}"
    }

    /** An approval or a question: a card while pending, a one-line record once closed. */
    data class InteractionRow(val interaction: Interaction) : TimelineRow {
        override val key = "interaction-${interaction.id}"
    }

    /** The running turn: 作業中 with what it is doing ([current], the latest item in progress). */
    data class Working(val turn: Turn, val current: Item?) : TimelineRow {
        override val key = "working-${turn.id}"
    }

    /** The end of a finished turn: status, time worked, usage, diff. */
    data class TurnEnd(val turn: Turn) : TimelineRow {
        override val key = "turn-end-${turn.id}"
    }

    /** A message in the outbox (sent when connected). */
    data class Pending(val input: PendingInput) : TimelineRow {
        override val key = "pending-${input.clientRequestId}"
    }

    /**
     * The head of the バックグラウンド section (the thread's background tasks, docs/android.md
     * 30): how many run ([running]: not ambient; [ambient]: running but not activity) and how
     * many ended. When [expanded], the running tasks follow, then [BackgroundEndedHeader].
     */
    data class BackgroundHeader(val running: Int, val ambient: Int, val ended: Int, val expanded: Boolean) : TimelineRow {
        override val key = "bg-header"
    }

    /**
     * One background task of the section. [depth] indents a task launched by another running
     * task under it; [parentTitle] names the task that launched it (when known).
     */
    data class BackgroundTaskRow(val task: BackgroundTask, val depth: Int, val parentTitle: String?) : TimelineRow {
        override val key = backgroundTaskKey(task.id)
    }

    /** The ended tasks of the section, folded by default; when [expanded] they follow. */
    data class BackgroundEndedHeader(val count: Int, val expanded: Boolean) : TimelineRow {
        override val key = "bg-ended"
    }

    companion object {
        /** The row key of a background task (a backgrounded item's chip scrolls to it). */
        fun backgroundTaskKey(taskId: String) = "bg-task-$taskId"
    }
}

/** The kinds folded into activity groups. */
fun Item.isActivity(): Boolean =
    this is Item.Reasoning || this is Item.CommandExecution || this is Item.FileChangeItem || this is Item.ToolCall

object Timeline {
    /** [build]'s key of the バックグラウンド section's open state (default: open while a task runs). */
    const val BACKGROUND_SECTION = "bg-section"

    /** [build]'s key of the ended tasks' open state (default: folded). */
    const val BACKGROUND_ENDED = "bg-ended"

    /**
     * The rows of a thread, top to bottom.
     *
     * * Each loaded turn: its start, then its items and interactions in the order they happened
     *   (item `startedAt` / interaction `createdAt`, both the daemon's clock), then 作業中 while
     *   it runs or its end summary once finished. A turn the agent started by itself for a
     *   reason the harness named (`trigger`) that completed without items is its start alone.
     * * Interactions that belong to no turn (asked by a background task, or while no turn ran)
     *   follow the loaded turn that started last before they were asked; before the first
     *   loaded turn they come first.
     * * Two or more consecutive activity items form a group; an interaction or any other item
     *   ends the group. A group is expanded when [expanded] says so, otherwise when it is the
     *   last group of a running turn (the live activity stays visible).
     * * Items and interactions of turns that are not loaded (a live event before its turn) come
     *   after the turns, then the バックグラウンド section ([backgroundRows]), then the messages
     *   still in the outbox.
     */
    fun build(state: ThreadState, pending: List<PendingInput>, expanded: Map<String, Boolean>): List<TimelineRow> {
        val rows = ArrayList<TimelineRow>()
        if (state.hasMoreBefore) rows += TimelineRow.LoadOlder
        val itemsByTurn = state.items.groupBy { it.turnId }
        val interactionsByTurn = state.interactions.filter { it.turnId != null }.groupBy { it.turnId }
        val known = state.turns.map { it.id }.toSet()
        val turnless = state.interactions.filter { it.turnId == null }.sortedWith(compareBy<Interaction> { it.createdAt }.thenBy { it.id })
        val firstStart = state.turns.firstOrNull()?.startedAt
        val beforeTurns = if (firstStart == null) emptyList() else turnless.filter { it.createdAt < firstStart }
        beforeTurns.forEach { rows += TimelineRow.InteractionRow(it) }
        state.turns.forEachIndexed { index, turn ->
            rows += TimelineRow.TurnStart(turn)
            val items = itemsByTurn[turn.id].orEmpty()
            val entries = merge(items, interactionsByTurn[turn.id].orEmpty())
            val collapsed = turn.trigger != null && turn.status == TurnStatus.Completed && entries.isEmpty()
            appendEntries(rows, entries, expanded, running = turn.status == TurnStatus.Running)
            when {
                turn.status == TurnStatus.Running ->
                    rows += TimelineRow.Working(turn, items.lastOrNull { it.status == ItemStatus.InProgress && it !is Item.UserMessage })
                !collapsed -> rows += TimelineRow.TurnEnd(turn)
            }
            val nextStart = state.turns.getOrNull(index + 1)?.startedAt
            turnless.filter { it.createdAt >= turn.startedAt && (nextStart == null || it.createdAt < nextStart) }
                .forEach { rows += TimelineRow.InteractionRow(it) }
        }
        val orphanItems = state.items.filter { it.turnId !in known }
        val orphanInteractions = state.interactions.filter { it.turnId != null && it.turnId !in known } +
            if (firstStart == null) turnless else emptyList()
        appendEntries(rows, merge(orphanItems, orphanInteractions), expanded, running = false)
        rows += backgroundRows(state.backgroundTasks, expanded)
        pending.forEach { rows += TimelineRow.Pending(it) }
        return rows
    }

    /**
     * The バックグラウンド section: nothing without tasks. Its header, and when open (by
     * [expanded], else while a task runs) the running tasks — each followed by the running tasks
     * it launched, indented — then the ended tasks folded under their own header (in the order
     * they ended, the latest nearest the composer).
     */
    fun backgroundRows(tasks: List<BackgroundTask>, expanded: Map<String, Boolean>): List<TimelineRow> {
        if (tasks.isEmpty()) return emptyList()
        val byId = tasks.associateBy { it.id }
        val running = tasks.filter { it.status == BackgroundTaskStatus.Running }
        val ended = tasks.filter { it.status != BackgroundTaskStatus.Running }
            .sortedWith(compareBy<BackgroundTask> { it.endedAt ?: Long.MAX_VALUE }.thenBy { it.id })
        val open = expanded[BACKGROUND_SECTION] ?: running.isNotEmpty()
        val endedOpen = expanded[BACKGROUND_ENDED] ?: false
        val rows = ArrayList<TimelineRow>()
        rows += TimelineRow.BackgroundHeader(
            running = running.count { !it.ambient },
            ambient = running.count { it.ambient },
            ended = ended.size,
            expanded = open,
        )
        if (!open) return rows
        for ((task, depth) in treeOrder(running)) {
            rows += TimelineRow.BackgroundTaskRow(task, depth, task.parentTaskId?.let { byId[it]?.title })
        }
        if (ended.isNotEmpty()) {
            rows += TimelineRow.BackgroundEndedHeader(ended.size, endedOpen)
            if (endedOpen) ended.forEach { rows += TimelineRow.BackgroundTaskRow(it, 0, it.parentTaskId?.let { id -> byId[id]?.title }) }
        }
        return rows
    }

    /**
     * Tasks in start order with each one's children (`parentTaskId` among [tasks]) right after
     * it, one level deeper. A task whose parent is not in [tasks] is a root.
     */
    private fun treeOrder(tasks: List<BackgroundTask>): List<Pair<BackgroundTask, Int>> {
        val ids = tasks.map { it.id }.toSet()
        val byStart = tasks.sortedWith(compareBy<BackgroundTask> { it.startedAt }.thenBy { it.id })
        val children = byStart.filter { it.parentTaskId in ids }.groupBy { it.parentTaskId }
        val out = ArrayList<Pair<BackgroundTask, Int>>(tasks.size)
        val placed = HashSet<String>()
        fun visit(task: BackgroundTask, depth: Int) {
            if (!placed.add(task.id)) return
            out += task to depth
            children[task.id].orEmpty().forEach { visit(it, depth + 1) }
        }
        byStart.filter { it.parentTaskId !in ids }.forEach { visit(it, 0) }
        // A cycle of parents (never reported by a harness) still shows every task once.
        byStart.forEach { visit(it, 0) }
        return out
    }

    private sealed interface Entry {
        data class OfItem(val item: Item) : Entry

        data class OfInteraction(val interaction: Interaction) : Entry
    }

    /** Items in their stored order with the interactions inserted by time (stable). */
    private fun merge(items: List<Item>, interactions: List<Interaction>): List<Entry> {
        if (interactions.isEmpty()) return items.map { Entry.OfItem(it) }
        val out = ArrayList<Entry>(items.size + interactions.size)
        val sorted = interactions.sortedWith(compareBy<Interaction> { it.createdAt }.thenBy { it.id })
        var next = 0
        for (item in items) {
            while (next < sorted.size && sorted[next].createdAt < item.startedAt) out += Entry.OfInteraction(sorted[next++])
            out += Entry.OfItem(item)
        }
        while (next < sorted.size) out += Entry.OfInteraction(sorted[next++])
        return out
    }

    private fun appendEntries(rows: MutableList<TimelineRow>, entries: List<Entry>, expanded: Map<String, Boolean>, running: Boolean) {
        // Runs of consecutive activity items.
        val runs = ArrayList<Pair<Int, Int>>()
        var k = 0
        while (k < entries.size) {
            val e = entries[k]
            if (e is Entry.OfItem && e.item.isActivity()) {
                var end = k
                while (end + 1 < entries.size && (entries[end + 1] as? Entry.OfItem)?.item?.isActivity() == true) end++
                if (end > k) runs += k to end
                k = end + 1
            } else {
                k++
            }
        }
        val lastRunStart = runs.lastOrNull()?.first
        var index = 0
        while (index < entries.size) {
            val run = runs.firstOrNull { it.first == index }
            if (run != null) {
                val items = (run.first..run.second).map { (entries[it] as Entry.OfItem).item }
                val groupKey = items.first().id
                val isLive = running && run.first == lastRunStart
                val open = expanded[groupKey] ?: isLive
                rows += TimelineRow.ActivityGroup(groupKey, items, open)
                if (open) items.forEach { rows += TimelineRow.GroupedItem(it, groupKey) }
                index = run.second + 1
                continue
            }
            when (val e = entries[index]) {
                is Entry.OfItem -> rows += TimelineRow.ItemRow(e.item)
                is Entry.OfInteraction -> rows += TimelineRow.InteractionRow(e.interaction)
            }
            index++
        }
    }
}
