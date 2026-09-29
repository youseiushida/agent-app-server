package dev.aas.android.sync

import dev.aas.android.protocol.BackgroundTask
import dev.aas.android.protocol.BackgroundTaskEnded
import dev.aas.android.protocol.Event
import dev.aas.android.protocol.EventEnvelope
import dev.aas.android.protocol.Interaction
import dev.aas.android.protocol.InteractionStatus
import dev.aas.android.protocol.Item
import dev.aas.android.protocol.Operation
import dev.aas.android.protocol.OperationStatus
import dev.aas.android.protocol.QueuedInput
import dev.aas.android.protocol.StreamBatch
import dev.aas.android.protocol.Thread
import dev.aas.android.protocol.ThreadReadResult
import dev.aas.android.protocol.TurnStatus
import dev.aas.android.protocol.WORKSPACE_STREAM
import dev.aas.android.protocol.WorkspaceSnapshotResult
import dev.aas.android.protocol.appendDelta
import dev.aas.android.protocol.threadIdOfStream
import dev.aas.android.protocol.threadStream

/**
 * Applies protocol data to a [SyncTx]. Stateless; the engine owns the transactions.
 *
 * Within one stream events arrive in order, but the workspace stream and a thread stream may
 * report facts about the same entity in either order. Two rules keep the local state from
 * going backwards, both based on explicit protocol state:
 *
 * * **Thread summaries** are ordered by [Thread.head], which grows with every summary change
 *   (protocol.md §3.1). A summary with a smaller head than the stored one is older and is
 *   ignored; an equal head is the same summary.
 * * **Interactions** never return to `pending` once resolved or expired (the server's state
 *   machine is `pending → resolved | expired`, design.md §8).
 *
 * Signals ([SyncSignal]) are collected for transitions the app may notify about; because the
 * engine never applies an event twice, each transition is reported once.
 */
internal object EventApplier {
    /** Result of [applyBatch]. */
    data class BatchOutcome(
        /** Events applied (after the cursor), in order. */
        val applied: List<EventEnvelope>,
        /** The stream's cursor moved (events were applied, or an empty batch moved it to its head). */
        val cursorMoved: Boolean,
        /**
         * Set when a merged delta (`seqFrom..seq`) started at or before the cursor: part of it
         * is already applied and it cannot be split, so the stream must be read again. Events
         * from it on were not applied and the cursor stops before it.
         */
        val overlapAtSeq: Long?,
    )

    /**
     * Applies the events of [batch] that lie after the stream's cursor and advances the cursor
     * to the last applied one — in the caller's transaction, so both commit together
     * (protocol.md §2.1, §7.1–7.2). A stream without a cursor has no base to apply to (its
     * snapshot or `thread/read` is not stored yet): the batch is ignored.
     *
     * A batch without events says that nothing is left to deliver between the cursor and its
     * `head` (the stream's last events were removed by the server's retention): the cursor moves
     * to the head, so the heartbeat's head no longer looks ahead of it (protocol.md §2.1, §7.2).
     *
     * [warn] reports data that had to be corrected to be stored (see [uniqueQueued]).
     *
     * [liveAfter] is the stream's head when this connection subscribed to it (from the
     * `subscribe` answer; `null` while it is not known): events up to it are the catch-up from the
     * stored cursor, later ones happen now. Transient requests of the harness say which
     * ([SyncSignal.ComposerInsert.live]).
     */
    suspend fun applyBatch(
        tx: SyncTx,
        batch: StreamBatch,
        signals: MutableList<SyncSignal>,
        warn: (String) -> Unit,
        liveAfter: Long? = null,
    ): BatchOutcome {
        val cursor = tx.cursor(batch.stream) ?: return BatchOutcome(emptyList(), cursorMoved = false, overlapAtSeq = null)
        if (batch.events.isEmpty()) {
            if (batch.head <= cursor) return BatchOutcome(emptyList(), cursorMoved = false, overlapAtSeq = null)
            tx.setCursor(batch.stream, batch.head)
            return BatchOutcome(emptyList(), cursorMoved = true, overlapAtSeq = null)
        }
        var last = cursor
        val applied = ArrayList<EventEnvelope>(batch.events.size)
        var overlap: Long? = null
        for (env in batch.events) {
            if (env.seq <= last) continue
            val from = env.seqFrom
            if (from != null && from <= last) {
                overlap = env.seq
                break
            }
            apply(tx, batch.stream, env, signals, warn, live = liveAfter != null && env.seq > liveAfter)
            applied += env
            last = env.seq
        }
        if (last > cursor) tx.setCursor(batch.stream, last)
        return BatchOutcome(applied, cursorMoved = last > cursor, overlapAtSeq = overlap)
    }

    /**
     * Applies one event of [stream]. The caller filtered `seq <= cursor` already. [live]: the
     * event happened after this connection subscribed (see [applyBatch]).
     */
    suspend fun apply(
        tx: SyncTx,
        stream: String,
        envelope: EventEnvelope,
        signals: MutableList<SyncSignal>,
        warn: (String) -> Unit,
        live: Boolean = false,
    ) {
        when (val e = envelope.event) {
            // ----- workspace -----
            is Event.ProjectUpserted -> tx.upsertProject(e.project)
            is Event.ProjectRemoved -> tx.removeProject(e.projectId)
            is Event.ThreadUpserted -> upsertThread(tx, e.thread, signals)
            is Event.ThreadRemoved -> removeThread(tx, e.threadId, signals)
            is Event.InteractionPending -> upsertInteraction(tx, e.interaction, signals)
            is Event.InteractionClosed -> {
                val existing = tx.interaction(e.interactionId)
                if (existing != null && existing.status == InteractionStatus.Pending && e.status != InteractionStatus.Pending) {
                    // The workspace event carries only the new status; the full resolution
                    // arrives with the thread's `interaction/resolved` or `thread/read`.
                    tx.upsertInteraction(existing.copy(status = e.status))
                    signals += SyncSignal.InteractionClosed(e.interactionId, e.threadId, e.status)
                }
            }
            is Event.HarnessUpdated -> tx.upsertHarness(e.harness)
            is Event.OperationUpdated -> upsertOperation(tx, e.operation, signals)
            // ----- thread -----
            is Event.ThreadUpdated -> upsertThread(tx, e.thread, signals)
            is Event.TurnStarted -> tx.upsertTurn(e.turn)
            is Event.TurnCompleted -> tx.upsertTurn(e.turn)
            is Event.TurnDiffUpdated -> tx.turn(e.turnId)?.let { tx.upsertTurn(it.copy(diff = e.diff)) }
            is Event.TurnUsageUpdated -> tx.turn(e.turnId)?.let { tx.upsertTurn(it.copy(usage = e.usage)) }
            is Event.ItemStarted -> upsertItem(tx, e.item, envelope.seq)
            is Event.ItemUpdated -> upsertItem(tx, e.item, envelope.seq)
            is Event.ItemCompleted -> upsertItem(tx, e.item, envelope.seq)
            // A delta for an item that is not stored cannot be applied (its start precedes the
            // loaded history); its final content arrives with item/completed or thread/read.
            is Event.ItemDelta -> tx.item(e.itemId)?.let { tx.upsertItem(it.copy(item = it.item.appendDelta(e.field, e.text))) }
            is Event.InteractionRequested -> upsertInteraction(tx, e.interaction, signals)
            is Event.InteractionResolved -> upsertInteraction(tx, e.interaction, signals)
            is Event.InteractionExpired -> upsertInteraction(tx, e.interaction, signals)
            is Event.BackgroundTaskUpdated -> upsertBackgroundTask(tx, e.task, signals)
            is Event.QueueUpdated -> threadIdOfStream(stream)?.let { tx.replaceQueued(it, uniqueQueued(it, e.queued, warn)) }
            Event.CommandsChanged -> threadIdOfStream(stream)?.let { id ->
                val meta = tx.threadMeta(id)
                tx.setThreadMeta(id, meta.copy(commandsVersion = meta.commandsVersion + 1))
            }
            // Nothing to store: the thread's summary (`thread/updated`) carries the new session id,
            // and the running turn a notice. The open thread's screen says it happened.
            is Event.NativeSessionChanged -> threadIdOfStream(stream)?.let {
                signals += SyncSignal.NativeSessionChanged(it, e.previousNativeSessionId, e.nativeSessionId)
            }
            // A request for the open thread's composer: relayed, never stored (protocol.md §5).
            is Event.ComposerInsert -> threadIdOfStream(stream)?.let { signals += SyncSignal.ComposerInsert(it, e.text, live) }
            // Raw harness events are not displayed; unknown types are ignored (protocol.md §7.6).
            is Event.Native, is Event.Unknown -> Unit
        }
    }

    /**
     * Replaces the local workspace with a snapshot (first sync, epoch change, or a server head
     * behind the cursor): wipe (the outbox stays), epoch, entities, workspace cursor. Threads
     * present at the snapshot count as read. Pending interactions are reported as signals —
     * they need the user's attention whether or not they were known before.
     */
    suspend fun applySnapshot(tx: SyncTx, epoch: String, snapshot: WorkspaceSnapshotResult, signals: MutableList<SyncSignal>) {
        tx.wipeSyncedData()
        tx.setEpoch(epoch)
        tx.replaceHarnesses(snapshot.harnesses)
        snapshot.projects.forEach { tx.upsertProject(it) }
        val threads = snapshot.threads.associateBy { it.id }
        for (thread in snapshot.threads) {
            tx.upsertThread(thread)
            tx.setViewState(thread.id, ThreadViewState(lastViewedHead = thread.head))
        }
        for (interaction in snapshot.pendingInteractions) {
            tx.upsertInteraction(interaction)
            if (interaction.status == InteractionStatus.Pending) {
                signals += SyncSignal.InteractionPending(interaction, threads[interaction.threadId])
            }
        }
        snapshot.operations.forEach { tx.upsertOperation(it) }
        tx.setCursor(WORKSPACE_STREAM, snapshot.head)
    }

    /**
     * Replaces the cached content of a thread with a `thread/read` result (its latest page)
     * and sets the thread stream's cursor to the read's head, so the subscription that follows
     * continues exactly after it (protocol.md §2, step 4).
     */
    suspend fun applyThreadRead(tx: SyncTx, read: ThreadReadResult, signals: MutableList<SyncSignal>, warn: (String) -> Unit) {
        val id = read.thread.id
        upsertThread(tx, read.thread, signals)
        // The tasks stored before the read are not new to this device (the read replaces them).
        val stored = tx.backgroundTasksOf(id).map { it.id }.toSet()
        tx.clearThreadContent(id)
        read.turns.forEach { tx.upsertTurn(it) }
        storeReadItems(tx, read)
        read.backgroundTasks.forEach { upsertBackgroundTask(tx, it, signals, known = it.id in stored) }
        read.interactions.forEach { upsertInteraction(tx, it, signals) }
        tx.replaceQueued(id, uniqueQueued(id, read.queued, warn))
        tx.setThreadMeta(id, tx.threadMeta(id).copy(hasMoreBefore = read.hasMoreBefore))
        tx.setCursor(threadStream(id), read.head)
    }

    /**
     * A thread's queue with each input once. The screens key the queue's rows by input id, and
     * Compose's lazy lists fail on a key given twice, so an id the server listed twice (it never
     * should: the queue is the daemon's own list) must not reach them; it is kept at its first
     * position and reported through [warn]. The other lists of the store are keyed by id
     * already (a second copy replaces the first).
     */
    private fun uniqueQueued(threadId: String, queued: List<QueuedInput>, warn: (String) -> Unit): List<QueuedInput> {
        val unique = queued.distinctBy { it.id }
        if (unique.size != queued.size) {
            val twice = queued.groupingBy { it.id }.eachCount().filterValues { it > 1 }.keys
            warn("the queue of $threadId lists ${twice.joinToString()} more than once; each input is kept once, at its first position")
        }
        return unique
    }

    /** Adds an older page of a thread (what is stored stays; the cursor is unchanged). */
    suspend fun applyOlderPage(tx: SyncTx, read: ThreadReadResult, signals: MutableList<SyncSignal>) {
        val id = read.thread.id
        read.turns.forEach { if (tx.turn(it.id) == null) tx.upsertTurn(it) }
        storeReadItems(tx, read)
        // Tasks already stored are as current as the stream made them: the page's copy may be
        // older (a running task it lists can have ended since, by an event applied first).
        read.backgroundTasks.forEach { if (tx.backgroundTask(it.id) == null) upsertBackgroundTask(tx, it, signals, known = false) }
        read.interactions.forEach { upsertInteraction(tx, it, signals) }
        tx.setThreadMeta(id, tx.threadMeta(id).copy(hasMoreBefore = read.hasMoreBefore))
    }

    /** Items of a read, positioned in the order the server returned them (turn order, then occurrence). */
    private suspend fun storeReadItems(tx: SyncTx, read: ThreadReadResult) {
        val turnIndex = read.turns.associate { it.id to it.index }
        val n = read.items.size.toLong()
        read.items.forEachIndexed { i, item ->
            if (tx.item(item.id) != null) return@forEachIndexed
            val index = turnIndex[item.turnId] ?: tx.turn(item.turnId)?.index ?: ItemPosition.UNKNOWN_TURN
            tx.upsertItem(StoredItem(item, ItemPosition(index, i - n)))
        }
    }

    private suspend fun upsertItem(tx: SyncTx, item: Item, seq: Long) {
        val position = tx.item(item.id)?.position
            ?: ItemPosition(tx.turn(item.turnId)?.index ?: ItemPosition.UNKNOWN_TURN, seq)
        tx.upsertItem(StoredItem(item, position))
    }

    private suspend fun upsertThread(tx: SyncTx, thread: Thread, signals: MutableList<SyncSignal>) {
        val existing = tx.thread(thread.id)
        if (existing != null && thread.head < existing.head) return
        tx.upsertThread(thread)
        // A thread seen for the first time reports nothing (an import, a fork, another device's thread).
        if (existing == null) return
        val ended = thread.background.lastEnded
        if (ended != null && isLaterEnd(ended, existing.background.lastEnded)) {
            signals += SyncSignal.BackgroundTaskFinished(thread, ended)
        }
        val turn = thread.lastTurn ?: return
        if (!turn.status.isTerminal) return
        val before = existing.lastTurn
        if (before == null || before.id != turn.id || before.status == TurnStatus.Running) {
            signals += SyncSignal.TurnFinished(thread, turn)
        }
    }

    /**
     * Whether [ended] (a summary's `background.lastEnded`) is an end after [before] (the stored
     * summary's), in the server's order: `endedAt`, then task id (protocol.md §3.1). Every end
     * gets its own `endedAt` (the next run of the same task ends later), and notifications
     * follow `lastEnded` moving on to a later end. Anything else is not a new end: a later
     * summary of the same end (its status or title corrected, `endedAt` kept), or an earlier
     * end — a daemon from before `lastEnded` only moved forward falls back to an end reported
     * long before when the task that ended last starts a new run.
     */
    private fun isLaterEnd(ended: BackgroundTaskEnded, before: BackgroundTaskEnded?): Boolean =
        before == null || compareValuesBy(ended, before, { it.endedAt }, { it.taskId }) > 0

    /**
     * Stores a background task (always the whole task: a new run of the same task replaces it
     * too). When it was not stored before, pending interactions it asked that were reported
     * without it (their request arrived first, on the workspace stream) are reported again with
     * it ([SyncSignal.InteractionTaskKnown]). [known]: whether the task was stored on this device
     * before (`null`: look it up).
     */
    private suspend fun upsertBackgroundTask(
        tx: SyncTx,
        task: BackgroundTask,
        signals: MutableList<SyncSignal>,
        known: Boolean? = null,
    ) {
        val before = known ?: (tx.backgroundTask(task.id) != null)
        tx.upsertBackgroundTask(task)
        if (before) return
        tx.interactionsOf(task.threadId)
            .filter { it.status == InteractionStatus.Pending && it.backgroundTaskId == task.id }
            .forEach { signals += SyncSignal.InteractionTaskKnown(it, task) }
    }

    private suspend fun removeThread(tx: SyncTx, id: String, signals: MutableList<SyncSignal>) {
        val existed = tx.thread(id) != null
        tx.removeThread(id)
        if (existed) signals += SyncSignal.ThreadRemoved(id)
    }

    private suspend fun upsertInteraction(tx: SyncTx, interaction: Interaction, signals: MutableList<SyncSignal>) {
        val existing = tx.interaction(interaction.id)
        val wasPending = existing?.status == InteractionStatus.Pending
        if (existing != null && !wasPending && interaction.status == InteractionStatus.Pending) return
        tx.upsertInteraction(interaction)
        when {
            interaction.status == InteractionStatus.Pending && !wasPending -> signals += SyncSignal.InteractionPending(
                interaction,
                tx.thread(interaction.threadId),
                interaction.backgroundTaskId?.let { tx.backgroundTask(it) },
            )
            interaction.status != InteractionStatus.Pending && wasPending ->
                signals += SyncSignal.InteractionClosed(interaction.id, interaction.threadId, interaction.status)
        }
    }

    private suspend fun upsertOperation(tx: SyncTx, operation: Operation, signals: MutableList<SyncSignal>) {
        val before = tx.operation(operation.id)
        tx.upsertOperation(operation)
        if (before?.status == OperationStatus.Running && operation.status.isTerminal) {
            signals += SyncSignal.OperationFinished(operation)
        }
    }
}
