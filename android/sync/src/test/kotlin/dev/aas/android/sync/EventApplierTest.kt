package dev.aas.android.sync

import dev.aas.android.protocol.ContextUsage
import dev.aas.android.protocol.DeltaField
import dev.aas.android.protocol.DiffSummary
import dev.aas.android.protocol.Event
import dev.aas.android.protocol.EventEnvelope
import dev.aas.android.protocol.InteractionStatus
import dev.aas.android.protocol.Item
import dev.aas.android.protocol.ItemStatus
import dev.aas.android.protocol.OperationStatus
import dev.aas.android.protocol.StreamBatch
import dev.aas.android.protocol.ThreadReadResult
import dev.aas.android.protocol.TurnStatus
import dev.aas.android.protocol.Usage
import dev.aas.android.protocol.WORKSPACE_STREAM
import dev.aas.android.protocol.WorkspaceSnapshotResult
import dev.aas.android.protocol.threadStream
import kotlinx.coroutines.runBlocking
import kotlinx.serialization.json.JsonObject
import kotlinx.serialization.json.JsonPrimitive
import kotlin.test.Test
import kotlin.test.assertEquals
import kotlin.test.assertIs
import kotlin.test.assertNull
import kotlin.test.assertTrue

class EventApplierTest {
    private val stream = threadStream("thr_1")
    private val store = InMemorySyncStore()
    private val signals = mutableListOf<SyncSignal>()

    private fun env(seq: Long, event: Event, seqFrom: Long? = null) = EventEnvelope(seq, seqFrom, 1_000 + seq, event)

    private fun test(block: suspend () -> Any?) {
        runBlocking { block() }
    }

    private suspend fun batch(stream: String, vararg events: EventEnvelope): EventApplier.BatchOutcome =
        store.transaction { EventApplier.applyBatch(it, StreamBatch(stream, events.maxOf { e -> e.seq }, events.toList()), signals) }

    private fun text(id: String) = (store.state.value.items[id]!!.item as Item.AgentMessage).text

    private suspend fun seedThread(cursor: Long, vararg items: Item) {
        store.transaction { tx ->
            tx.upsertThread(Samples.thread("thr_1", head = cursor))
            tx.upsertTurn(Samples.turn("trn_1"))
            items.forEachIndexed { i, item -> tx.upsertItem(StoredItem(item, ItemPosition(0, i - items.size.toLong()))) }
            tx.setCursor(stream, cursor)
        }
    }

    @Test
    fun eventsAtOrBeforeTheCursorAreSkippedAndTheCursorAdvancesInTheSameTransaction() = test {
        seedThread(10, Samples.agentMessage("itm_1", "a"))
        val commitsBefore = store.commits
        val outcome = batch(
            stream,
            env(9, Event.ItemDelta("itm_1", DeltaField.Text, "old")),
            env(10, Event.ItemDelta("itm_1", DeltaField.Text, "old")),
            env(11, Event.ItemDelta("itm_1", DeltaField.Text, "b")),
            env(13, Event.ItemDelta("itm_1", DeltaField.Text, "c")),
        )
        assertEquals(listOf(11L, 13L), outcome.applied.map { it.seq })
        assertEquals("abc", text("itm_1"))
        assertEquals(13L, store.state.value.cursors[stream])
        assertEquals(commitsBefore + 1, store.commits, "events and cursor in one transaction")
        // The same batch again (a replay) changes nothing.
        val again = batch(stream, env(11, Event.ItemDelta("itm_1", DeltaField.Text, "b")), env(13, Event.ItemDelta("itm_1", DeltaField.Text, "c")))
        assertTrue(again.applied.isEmpty())
        assertEquals("abc", text("itm_1"))
    }

    @Test
    fun aMergedDeltaAfterTheCursorAppliesAsOneEvent() = test {
        seedThread(10, Samples.agentMessage("itm_1", "Hel"))
        val outcome = batch(stream, env(15, Event.ItemDelta("itm_1", DeltaField.Text, "lo"), seqFrom = 11))
        assertNull(outcome.overlapAtSeq)
        assertEquals("Hello", text("itm_1"))
        assertEquals(15L, store.state.value.cursors[stream])
    }

    @Test
    fun aMergedDeltaOverlappingTheCursorStopsTheBatch() = test {
        seedThread(12, Samples.agentMessage("itm_1", "abc"))
        val outcome = batch(
            stream,
            env(13, Event.ItemDelta("itm_1", DeltaField.Text, "d")),
            env(16, Event.ItemDelta("itm_1", DeltaField.Text, "XYZ"), seqFrom = 12),
            env(17, Event.ItemDelta("itm_1", DeltaField.Text, "!")),
        )
        assertEquals(16L, outcome.overlapAtSeq)
        assertEquals(listOf(13L), outcome.applied.map { it.seq })
        assertEquals("abcd", text("itm_1"), "nothing from the overlapping event on")
        assertEquals(13L, store.state.value.cursors[stream])
    }

    @Test
    fun aStreamWithoutACursorIsNotApplied() = test {
        val outcome = batch(WORKSPACE_STREAM, env(1, Event.ProjectUpserted(Samples.project("prj_1"))))
        assertTrue(outcome.applied.isEmpty())
        assertTrue(store.state.value.projects.isEmpty())
        assertNull(store.state.value.cursors[WORKSPACE_STREAM])
    }

    @Test
    fun threadSummariesAreOrderedByHeadNotByClock() = test {
        store.transaction { it.setCursor(WORKSPACE_STREAM, 0); it.setCursor(stream, 0) }
        // The thread stream delivered a newer summary (head 20) before the workspace stream's older one.
        batch(stream, env(21, Event.ThreadUpdated(Samples.thread("thr_1", head = 20, title = "new").copy(updatedAt = 1))))
        batch(WORKSPACE_STREAM, env(5, Event.ThreadUpserted(Samples.thread("thr_1", head = 10, title = "old").copy(updatedAt = 999))))
        assertEquals("new", store.state.value.threads["thr_1"]!!.title, "a larger updatedAt does not win over a larger head")
        batch(WORKSPACE_STREAM, env(6, Event.ThreadUpserted(Samples.thread("thr_1", head = 20, title = "new"))))
        batch(WORKSPACE_STREAM, env(7, Event.ThreadUpserted(Samples.thread("thr_1", head = 30, title = "newer"))))
        assertEquals("newer", store.state.value.threads["thr_1"]!!.title)
    }

    @Test
    fun threadReadDoesNotRollBackANewerSummary() = test {
        store.transaction { it.upsertThread(Samples.thread("thr_1", head = 40, title = "from workspace")) }
        val read = ThreadReadResult(Samples.thread("thr_1", head = 30, title = "stale"), emptyList(), emptyList(), emptyList(), emptyList(), 35, false)
        store.transaction { EventApplier.applyThreadRead(it, read, signals) }
        assertEquals("from workspace", store.state.value.threads["thr_1"]!!.title)
        assertEquals(35L, store.state.value.cursors[stream])
    }

    @Test
    fun resolvedInteractionsNeverReturnToPending() = test {
        store.transaction { it.setCursor(WORKSPACE_STREAM, 0); it.setCursor(stream, 0) }
        batch(stream, env(5, Event.InteractionResolved(Samples.approval("int_1", status = InteractionStatus.Resolved))))
        batch(WORKSPACE_STREAM, env(9, Event.InteractionPending(Samples.approval("int_1"))))
        assertEquals(InteractionStatus.Resolved, store.state.value.interactions["int_1"]!!.status)
        batch(WORKSPACE_STREAM, env(10, Event.InteractionClosed("int_1", "thr_1", InteractionStatus.Expired)))
        assertEquals(InteractionStatus.Resolved, store.state.value.interactions["int_1"]!!.status, "closed only changes pending ones")
        assertTrue(signals.none { it is SyncSignal.InteractionPending }, "never reported as pending: $signals")
    }

    @Test
    fun aPendingInteractionIsSignalledOnceAndClosedOnce() = test {
        store.transaction { it.setCursor(WORKSPACE_STREAM, 0); it.setCursor(stream, 0); it.upsertThread(Samples.thread("thr_1")) }
        batch(WORKSPACE_STREAM, env(1, Event.InteractionPending(Samples.approval("int_1"))))
        batch(stream, env(4, Event.InteractionRequested(Samples.approval("int_1"))))
        val pending = signals.filterIsInstance<SyncSignal.InteractionPending>()
        assertEquals(1, pending.size, "the same interaction from both streams: $signals")
        assertEquals("thr_1", pending.single().thread?.id)
        batch(WORKSPACE_STREAM, env(2, Event.InteractionClosed("int_1", "thr_1", InteractionStatus.Resolved)))
        batch(stream, env(5, Event.InteractionResolved(Samples.approval("int_1", status = InteractionStatus.Resolved))))
        assertEquals(listOf(SyncSignal.InteractionClosed("int_1", "thr_1", InteractionStatus.Resolved)), signals.filterIsInstance<SyncSignal.InteractionClosed>())
        val stored = store.state.value.interactions["int_1"]!!
        assertEquals(InteractionStatus.Resolved, stored.status)
        assertNull(stored.resolvedAt, "the workspace event carries no time; nothing is made up")
    }

    @Test
    fun turnFinishedIsSignalledWhenTheLastTurnEnds() = test {
        store.transaction { it.setCursor(WORKSPACE_STREAM, 0) }
        val running = Samples.turnSummary("trn_1", 0, TurnStatus.Running)
        val done = Samples.turnSummary("trn_1", 0, TurnStatus.Completed)
        // A thread first seen with a finished turn (import, fork) is not "finished" now.
        batch(WORKSPACE_STREAM, env(1, Event.ThreadUpserted(Samples.thread("thr_0", head = 1, lastTurn = done))))
        batch(WORKSPACE_STREAM, env(2, Event.ThreadUpserted(Samples.thread("thr_1", head = 1, lastTurn = running))))
        assertTrue(signals.isEmpty(), "$signals")
        batch(WORKSPACE_STREAM, env(3, Event.ThreadUpserted(Samples.thread("thr_1", head = 5, lastTurn = done))))
        // The same summary again from the thread stream is not a second finish.
        store.transaction { it.setCursor(stream, 0) }
        batch(stream, env(6, Event.ThreadUpdated(Samples.thread("thr_1", head = 5, lastTurn = done))))
        val finished = signals.filterIsInstance<SyncSignal.TurnFinished>()
        assertEquals(1, finished.size, "$signals")
        assertEquals(TurnStatus.Completed, finished.single().turn.status)
        // The next turn ends while its running summary was missed: still reported.
        batch(WORKSPACE_STREAM, env(4, Event.ThreadUpserted(Samples.thread("thr_1", head = 9, lastTurn = Samples.turnSummary("trn_2", 1, TurnStatus.Failed)))))
        assertEquals(2, signals.filterIsInstance<SyncSignal.TurnFinished>().size)
    }

    @Test
    fun operationFinishedIsSignalledOnItsTransition() = test {
        store.transaction { it.setCursor(WORKSPACE_STREAM, 0) }
        batch(
            WORKSPACE_STREAM,
            env(1, Event.OperationUpdated(Samples.operation("op_1", OperationStatus.Running))),
            env(2, Event.OperationUpdated(Samples.operation("op_1", OperationStatus.Running).copy(progress = "Receiving objects:  42%"))),
            env(3, Event.OperationUpdated(Samples.operation("op_1", OperationStatus.Cancelled))),
        )
        assertEquals(listOf(OperationStatus.Cancelled), signals.filterIsInstance<SyncSignal.OperationFinished>().map { it.operation.status })
        assertNull(store.state.value.operations["op_1"]!!.progress)
    }

    @Test
    fun turnEventsUpdateTheStoredTurn() = test {
        seedThread(0)
        val usage = Usage(inputTokens = 10, outputTokens = 3, context = ContextUsage(1200, 8000))
        batch(
            stream,
            env(1, Event.TurnUsageUpdated("trn_1", usage)),
            env(2, Event.TurnCompleted(Samples.turn("trn_1", status = TurnStatus.Completed).copy(usage = usage))),
            env(3, Event.TurnDiffUpdated("trn_1", DiffSummary(2, 40, 3))),
        )
        val turn = store.state.value.turns["trn_1"]!!
        assertEquals(ContextUsage(1200, 8000), turn.usage?.context)
        assertEquals(TurnStatus.Completed, turn.status)
        assertEquals(DiffSummary(2, 40, 3), turn.diff)
    }

    @Test
    fun liveItemsSortAfterReadItemsOfTheSameTurn() = test {
        val read = ThreadReadResult(
            Samples.thread("thr_1", head = 5),
            listOf(Samples.turn("trn_0", index = 0, status = TurnStatus.Completed), Samples.turn("trn_1", index = 1)),
            listOf(
                Samples.agentMessage("itm_a", "a", turnId = "trn_0", status = ItemStatus.Completed),
                Samples.agentMessage("itm_b", "b", turnId = "trn_1"),
            ),
            emptyList(), emptyList(), 5, true,
        )
        store.transaction { EventApplier.applyThreadRead(it, read, signals) }
        batch(stream, env(6, Event.ItemStarted(Samples.agentMessage("itm_c", "c", turnId = "trn_1"))))
        // An older page arrives later and sorts before everything.
        val older = ThreadReadResult(
            Samples.thread("thr_1", head = 5),
            listOf(Samples.turn("trn_m", index = -1, status = TurnStatus.Completed)),
            listOf(Samples.agentMessage("itm_old", "o", turnId = "trn_m", status = ItemStatus.Completed)),
            emptyList(), emptyList(), 6, false,
        )
        store.transaction { EventApplier.applyOlderPage(it, older, signals) }
        assertEquals(listOf("itm_old", "itm_a", "itm_b", "itm_c"), store.itemsOf("thr_1").map { it.item.id })
        assertEquals(6L, store.state.value.cursors[stream], "an older page leaves the cursor")
        assertEquals(false, store.state.value.meta["thr_1"]!!.hasMoreBefore)
    }

    @Test
    fun threadReadReplacesContentButKeepsKnownInteractions() = test {
        seedThread(3, Samples.agentMessage("itm_stale", "x"))
        store.transaction { it.upsertInteraction(Samples.approval("int_ws")) }
        val read = ThreadReadResult(
            Samples.thread("thr_1", head = 8),
            listOf(Samples.turn("trn_2", index = 1)),
            listOf(Samples.agentMessage("itm_new", "y", turnId = "trn_2")),
            emptyList(),
            listOf(Samples.queued("que_1")),
            8, false,
        )
        store.transaction { EventApplier.applyThreadRead(it, read, signals) }
        val s = store.state.value
        assertEquals(setOf("itm_new"), s.items.keys)
        assertEquals(setOf("trn_2"), s.turns.keys)
        assertEquals(setOf("int_ws"), s.interactions.keys)
        assertEquals(listOf("que_1"), s.queued["thr_1"]!!.map { it.id })
        assertEquals(8L, s.cursors[stream])
    }

    @Test
    fun snapshotReplacesEverythingButTheOutbox() = test {
        store.transaction { tx ->
            tx.upsertThread(Samples.thread("thr_stale"))
            tx.setCursor(threadStream("thr_stale"), 50)
            tx.addOutbox(OutboxEntry("c1", "turn/start", JsonObject(mapOf("clientRequestId" to JsonPrimitive("c1"))), 1))
        }
        val snapshot = WorkspaceSnapshotResult(
            emptyList(), listOf(Samples.project("prj_1")), listOf(Samples.thread("thr_1", head = 7)),
            listOf(Samples.approval("int_1")), emptyList(), 12,
        )
        store.transaction { EventApplier.applySnapshot(it, "e2", snapshot, signals) }
        val s = store.state.value
        assertEquals(setOf("thr_1"), s.threads.keys)
        assertEquals(mapOf(WORKSPACE_STREAM to 12L), s.cursors)
        assertEquals(ThreadViewState(lastViewedHead = 7), s.viewStates["thr_1"], "threads present at the snapshot count as read")
        assertEquals(listOf("c1"), s.outbox.map { it.clientRequestId })
        assertEquals("e2", s.epoch)
        assertIs<SyncSignal.InteractionPending>(signals.single())
    }

    @Test
    fun queueAndCommandEventsApplyToTheirThread() = test {
        seedThread(0)
        batch(stream, env(1, Event.QueueUpdated(listOf(Samples.queued("que_1")))), env(2, Event.CommandsChanged), env(3, Event.CommandsChanged))
        assertEquals(listOf("que_1"), store.state.value.queued["thr_1"]!!.map { it.id })
        assertEquals(2, store.state.value.meta["thr_1"]!!.commandsVersion)
    }

    @Test
    fun threadRemovedDeletesItsDataAndIsSignalled() = test {
        seedThread(4, Samples.agentMessage("itm_1", "a"))
        store.transaction { it.setCursor(WORKSPACE_STREAM, 0) }
        batch(WORKSPACE_STREAM, env(1, Event.ThreadRemoved("thr_1")))
        val s = store.state.value
        assertTrue(s.threads.isEmpty() && s.items.isEmpty() && s.turns.isEmpty())
        assertNull(s.cursors[stream])
        assertEquals<List<SyncSignal>>(listOf(SyncSignal.ThreadRemoved("thr_1")), signals)
    }

    @Test
    fun unknownAndNativeEventsOnlyMoveTheCursor() = test {
        seedThread(0)
        val before = store.state.value
        val outcome = batch(
            stream,
            env(1, Event.Native("fake", JsonObject(emptyMap()))),
            env(2, Event.Unknown("thread/somethingNew", JsonObject(emptyMap()))),
        )
        assertEquals(2, outcome.applied.size)
        assertEquals(before.copy(cursors = before.cursors + (stream to 2L)), store.state.value)
    }
}
