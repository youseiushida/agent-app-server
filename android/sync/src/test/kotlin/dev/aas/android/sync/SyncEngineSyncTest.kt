package dev.aas.android.sync

import dev.aas.android.protocol.DeltaField
import dev.aas.android.protocol.ErrorKind
import dev.aas.android.protocol.Event
import dev.aas.android.protocol.InteractionStatus
import dev.aas.android.protocol.Item
import dev.aas.android.protocol.ItemStatus
import dev.aas.android.protocol.Methods
import dev.aas.android.protocol.StreamBatch
import dev.aas.android.protocol.ThreadReadResult
import dev.aas.android.protocol.TurnStatus
import dev.aas.android.protocol.WORKSPACE_STREAM
import dev.aas.android.protocol.WorkspaceSnapshotResult
import dev.aas.android.protocol.threadStream
import kotlinx.coroutines.delay
import kotlinx.serialization.json.JsonObject
import kotlinx.serialization.json.JsonPrimitive
import kotlinx.serialization.json.jsonObject
import kotlinx.serialization.json.jsonPrimitive
import kotlin.test.Test
import kotlin.test.assertEquals
import kotlin.test.assertNull
import kotlin.test.assertTrue

/** Initial sync, resubscription, open threads and the flows the UI collects. */
class SyncEngineSyncTest {
    private fun snapshot(head: Long, vararg projects: String) = WorkspaceSnapshotResult(
        emptyList(), projects.map { Samples.project(it) }, listOf(Samples.thread("thr_1", head = 3)), emptyList(), emptyList(), head,
    )

    private fun threadRead(head: Long, vararg items: Item) = ThreadReadResult(
        thread = Samples.thread("thr_1", head = head),
        turns = listOf(Samples.turn("trn_1")),
        items = items.toList(),
        interactions = emptyList(),
        queued = emptyList(),
        head = head,
        hasMoreBefore = false,
    )

    @Test
    fun firstSyncAppliesTheSnapshotThenLiveBatches() = withFixture { f ->
        f.server.snapshot = snapshot(5, "prj_1")
        repeat(5) { f.server.append(WORKSPACE_STREAM, Event.CommandsChanged) } // history up to the snapshot head
        f.connect()
        f.awaitOnline()
        val s = f.store.state.value
        assertEquals(setOf("prj_1"), s.projects.keys)
        assertEquals(5L, s.cursors[WORKSPACE_STREAM])
        assertEquals("epoch-1", s.epoch)
        assertNull(f.server.requestsFor("initialize").single().second.params!!.jsonObject["lastKnownEpoch"], "first run has no epoch")
        assertEquals(mapOf(WORKSPACE_STREAM to 5L), subscriptionsOf(f.server.requestsFor("subscribe").single()))
        assertTrue(f.server.requests.indexOf(f.server.requestsFor("workspace/snapshot").single()) < f.server.requests.indexOf(f.server.requestsFor("subscribe").single()))
        // The flows the UI collects.
        val ws = f.engine.workspace.value
        assertTrue(ws.synced)
        assertEquals(listOf("prj_1"), ws.projects.map { it.id })
        assertEquals(listOf(ThreadEntry(Samples.thread("thr_1", head = 3), unread = false)), ws.threads, "threads of the snapshot count as read")
        assertTrue(f.engine.status.value.lastSyncAtMs != null)
        // Only events after the snapshot head arrive; a live event updates store and flow.
        val env = f.server.append(WORKSPACE_STREAM, Event.ProjectUpserted(Samples.project("prj_2")))
        f.server.lastConnection.pushNew(WORKSPACE_STREAM)
        eventually(what = "live project") { f.engine.workspace.value.projects.find { it.id == "prj_2" } }
        assertEquals(env.seq, f.store.state.value.cursors[WORKSPACE_STREAM])
        assertEquals(env.seq, f.engine.status.value.cursors[WORKSPACE_STREAM])
        assertEquals(1, f.server.requestsFor("workspace/snapshot").size)
    }

    @Test
    fun aReconnectResubscribesFromTheStoredCursors() = withFixture { f ->
        f.server.snapshot = snapshot(0, "prj_1")
        f.connect()
        f.awaitOnline()
        f.server.append(WORKSPACE_STREAM, Event.ProjectUpserted(Samples.project("prj_2")))
        f.server.lastConnection.pushNew(WORKSPACE_STREAM)
        eventually(what = "prj_2") { f.store.state.value.projects["prj_2"] }
        // The connection dies silently; meanwhile the server moves on.
        f.server.lastConnection.kill()
        f.server.append(WORKSPACE_STREAM, Event.ProjectUpserted(Samples.project("prj_3")))
        f.server.append(WORKSPACE_STREAM, Event.ProjectRemoved("prj_1"))
        eventually(what = "replayed events") { f.store.state.value.projects.takeIf { it.keys == setOf("prj_2", "prj_3") } }
        val second = f.server.requestsFor("subscribe").last { it.first == 1 }
        assertEquals(mapOf(WORKSPACE_STREAM to 1L), subscriptionsOf(second), "resubscribed from the stored cursor")
        assertEquals(1, f.server.requestsFor("workspace/snapshot").size, "no second snapshot without an epoch change")
        assertEquals("epoch-1", f.server.requestsFor("initialize").last().second.params!!.jsonObject["lastKnownEpoch"]!!.jsonPrimitive.content)
        assertEquals(3L, f.store.state.value.cursors[WORKSPACE_STREAM])
        eventually(what = "reconnect counted") { f.engine.status.value.reconnects.takeIf { it == 1 } }
    }

    @Test
    fun aWorkspaceHeadBehindTheCursorForcesAFullResync() = withFixture { f ->
        f.server.snapshot = snapshot(4, "prj_1")
        repeat(4) { f.server.append(WORKSPACE_STREAM, Event.CommandsChanged) }
        f.connect()
        f.awaitOnline()
        // The server comes back from a backup: same epoch, but its log ends before our cursor.
        f.server.log(WORKSPACE_STREAM).removeIf { it.seq > 2 }
        f.server.snapshot = snapshot(2, "prj_restored")
        f.server.lastConnection.kill()
        eventually(what = "resync") { f.store.state.value.projects.takeIf { it.keys == setOf("prj_restored") } }
        assertEquals(2, f.server.requestsFor("workspace/snapshot").size)
        assertEquals(2L, f.store.state.value.cursors[WORKSPACE_STREAM])
    }

    @Test
    fun openingAThreadReadsItThenSubscribesAfterItsHead() = withFixture { f ->
        f.server.snapshot = snapshot(0)
        f.server.threadReads["thr_1"] = threadRead(10, Samples.agentMessage("itm_1", "Hi"))
        f.connect()
        f.awaitOnline()
        val state = f.engine.openThread("thr_1")
        val stream = threadStream("thr_1")
        eventually(what = "live thread") { state.value.takeIf { it.sync == ThreadSync.Live } }
        assertEquals(listOf("itm_1"), state.value.items.map { it.id })
        val read = f.server.requestsFor("thread/read").single()
        val threadSub = f.server.requests.indexOfLast { it.second.method == "subscribe" }
        assertTrue(f.server.requests.indexOf(read) < threadSub, "thread/read comes before subscribe")
        assertEquals(mapOf(stream to 10L), subscriptionsOf(f.server.requests[threadSub]))

        f.server.append(stream, Event.ItemDelta("itm_1", DeltaField.Text, "!"), seq = 11)
        f.server.lastConnection.pushNew(stream)
        eventually(what = "delta in the flow") { (state.value.items.single() as Item.AgentMessage).text.takeIf { it == "Hi!" } }

        // Reconnect: the open thread is resubscribed from its cursor, not read again.
        f.server.lastConnection.kill()
        f.server.append(stream, Event.ItemCompleted(Samples.agentMessage("itm_1", "Hi!", status = ItemStatus.Completed)), seq = 12)
        eventually(what = "replayed item") { state.value.items.singleOrNull()?.takeIf { it.status == ItemStatus.Completed } }
        val resumed = f.server.requestsFor("subscribe").last { it.first == 1 }
        assertEquals(mapOf(WORKSPACE_STREAM to 0L, stream to 11L), subscriptionsOf(resumed))
        assertEquals(1, f.server.requestsFor("thread/read").size)
        eventually(what = "live again") { state.value.takeIf { it.sync == ThreadSync.Live } }
    }

    @Test
    fun anOpenThreadIsCountedAndBatchesOfClosedThreadsAreIgnored() = withFixture { f ->
        f.server.snapshot = snapshot(0)
        f.server.threadReads["thr_1"] = threadRead(3, Samples.agentMessage("itm_1", "a"))
        f.connect()
        f.awaitOnline()
        f.engine.openThread("thr_1")
        f.engine.openThread("thr_1")
        eventually(what = "loaded") { f.store.state.value.cursors[threadStream("thr_1")] }
        f.engine.closeThread("thr_1")
        assertTrue(f.engine.thread("thr_1") != null, "still open once")
        f.engine.closeThread("thr_1")
        assertNull(f.engine.thread("thr_1"))
        eventually(what = "unsubscribe") { f.server.requestsFor("unsubscribe").firstOrNull() }
        // A batch that was already in flight when the thread closed.
        val env = f.server.append(threadStream("thr_1"), Event.ItemDelta("itm_1", DeltaField.Text, "b"), seq = 4)
        f.server.lastConnection.batch(StreamBatch(threadStream("thr_1"), 4, listOf(env)))
        delay(300)
        assertEquals("a", (f.store.state.value.items["itm_1"]!!.item as Item.AgentMessage).text)
        assertEquals(3L, f.store.state.value.cursors[threadStream("thr_1")])
        assertEquals(1, f.server.requestsFor("thread/read").size, "the second open did not read again")
    }

    @Test
    fun duplicateBatchesAreIgnoredAndMergedDeltasApply() = withFixture { f ->
        f.server.snapshot = snapshot(0)
        f.server.threadReads["thr_1"] = threadRead(10, Samples.agentMessage("itm_1", "Hel"))
        f.connect()
        f.awaitOnline()
        val state = f.engine.openThread("thr_1")
        eventually(what = "live") { state.value.takeIf { it.sync == ThreadSync.Live } }
        val stream = threadStream("thr_1")
        val merged = f.server.append(stream, Event.ItemDelta("itm_1", DeltaField.Text, "lo"), seq = 15, seqFrom = 11)
        val conn = f.server.lastConnection
        conn.batch(StreamBatch(stream, 15, listOf(merged)))
        conn.batch(StreamBatch(stream, 15, listOf(merged))) // replayed
        val bang = f.server.append(stream, Event.ItemDelta("itm_1", DeltaField.Text, "!"), seq = 16)
        conn.batch(StreamBatch(stream, 16, listOf(merged, bang)))
        eventually(what = "cursor 16") { f.store.state.value.cursors[stream]?.takeIf { it == 16L } }
        delay(100)
        assertEquals("Hello!", (state.value.items.single() as Item.AgentMessage).text)
        assertEquals(1, f.server.requestsFor("thread/read").size, "no reload for plain duplicates")
    }

    @Test
    fun aMergedDeltaOverlappingTheCursorReloadsTheThread() = withFixture { f ->
        f.server.snapshot = snapshot(0)
        f.server.threadReads["thr_1"] = threadRead(12, Samples.agentMessage("itm_1", "abc"))
        f.connect()
        f.awaitOnline()
        val state = f.engine.openThread("thr_1")
        eventually(what = "live") { state.value.takeIf { it.sync == ThreadSync.Live } }
        f.server.threadReads["thr_1"] = threadRead(14, Samples.agentMessage("itm_1", "abcde"))
        val stale = f.server.append(threadStream("thr_1"), Event.ItemDelta("itm_1", DeltaField.Text, "XYZ"), seq = 14, seqFrom = 11)
        f.server.lastConnection.batch(StreamBatch(threadStream("thr_1"), 14, listOf(stale)))
        eventually(what = "reload") { f.server.requestsFor("thread/read").takeIf { it.size == 2 } }
        eventually(what = "reloaded content") { (state.value.items.singleOrNull() as? Item.AgentMessage)?.text?.takeIf { it == "abcde" } }
        assertEquals(14L, f.store.state.value.cursors[threadStream("thr_1")])
        assertEquals(1, f.server.connections.size, "reloaded on the same connection")
    }

    @Test
    fun anEpochChangeWipesLocalStateButKeepsTheOutbox() = withFixture { f ->
        f.store.transaction { tx ->
            tx.setEpoch("epoch-old")
            tx.setCursor(WORKSPACE_STREAM, 99)
            tx.setCursor(threadStream("thr_stale"), 40)
            tx.upsertProject(Samples.project("prj_stale"))
            tx.addOutbox(OutboxEntry("crid-kept", Methods.TurnStart.name, JsonObject(mapOf("clientRequestId" to JsonPrimitive("crid-kept"), "threadId" to JsonPrimitive("thr_x"))), 1))
        }
        f.server.snapshot = snapshot(3, "prj_new")
        f.server.onRequest = { _, _ -> FakeServer.rpcError(ErrorKind.NotFound) }
        f.connect()
        f.awaitOnline()
        assertEquals("epoch-old", f.server.requestsFor("initialize").single().second.params!!.jsonObject["lastKnownEpoch"]!!.jsonPrimitive.content)
        assertEquals(setOf("prj_new"), f.store.state.value.projects.keys)
        assertEquals(mapOf(WORKSPACE_STREAM to 3L), f.store.state.value.cursors)
        assertEquals("epoch-1", f.store.state.value.epoch)
        // The kept entry is sent after the resync and dropped on its definitive error.
        eventually(what = "outbox sent") { f.server.requestsFor("turn/start").firstOrNull() }
        assertTrue(f.server.requests.indexOf(f.server.requestsFor("turn/start").single()) > f.server.requests.indexOf(f.server.requestsFor("subscribe").single()))
        eventually(what = "outbox empty") { f.store.state.value.outbox.takeIf { it.isEmpty() } }
        eventually(what = "failure reported") { f.results.filterIsInstance<OutboxResult.Failed>().firstOrNull() }
    }

    @Test
    fun deletedThreadsAreDroppedWhenOpenedOrResubscribed() = withFixture { f ->
        f.server.snapshot = snapshot(0)
        f.connect()
        f.awaitOnline()
        assertTrue(f.store.state.value.threads.containsKey("thr_1"))
        val state = f.engine.openThread("thr_1") // no thread/read for it: notFound
        eventually(what = "removed") { state.value.takeIf { it.sync == ThreadSync.Removed } }
        assertTrue(f.store.state.value.threads.isEmpty())
        assertEquals(listOf(SyncSignal.ThreadRemoved("thr_1")), f.signals.toList())

        // A thread whose subscription is refused on a resume.
        f.store.transaction { it.upsertThread(Samples.thread("thr_2")) }
        f.server.threadReads["thr_2"] = ThreadReadResult(Samples.thread("thr_2", head = 1), emptyList(), emptyList(), emptyList(), emptyList(), 1, false)
        val second = f.engine.openThread("thr_2")
        eventually(what = "thr_2 live") { second.value.takeIf { it.sync == ThreadSync.Live } }
        f.server.notFoundStreams += threadStream("thr_2")
        f.server.lastConnection.kill()
        eventually(what = "thr_2 removed") { second.value.takeIf { it.sync == ThreadSync.Removed } }
        assertNull(f.store.state.value.threads["thr_2"])
    }

    @Test
    fun signalsReportNewInteractionsAndFinishedTurns() = withFixture { f ->
        f.server.snapshot = snapshot(0)
        f.connect()
        f.awaitOnline()
        val conn = f.server.lastConnection
        f.server.append(WORKSPACE_STREAM, Event.ThreadUpserted(Samples.thread("thr_1", head = 4, lastTurn = Samples.turnSummary("trn_1", 0, TurnStatus.Running))))
        f.server.append(WORKSPACE_STREAM, Event.InteractionPending(Samples.approval("int_1")))
        f.server.append(WORKSPACE_STREAM, Event.InteractionClosed("int_1", "thr_1", InteractionStatus.Resolved))
        f.server.append(WORKSPACE_STREAM, Event.ThreadUpserted(Samples.thread("thr_1", head = 9, lastTurn = Samples.turnSummary("trn_1", 0, TurnStatus.Completed))))
        conn.pushNew(WORKSPACE_STREAM)
        eventually(what = "signals") { f.signals.takeIf { it.size == 3 } }
        val (pending, closed, finished) = f.signals.toList()
        assertEquals("int_1", (pending as SyncSignal.InteractionPending).interaction.id)
        assertEquals(SyncSignal.InteractionClosed("int_1", "thr_1", InteractionStatus.Resolved), closed)
        assertEquals(TurnStatus.Completed, (finished as SyncSignal.TurnFinished).turn.status)
        assertTrue(f.engine.workspace.value.pendingInteractions.isEmpty())
        // A replay after a reconnect signals nothing again.
        conn.kill()
        eventually(what = "reconnected") { f.server.connections.size.takeIf { it == 2 && f.engine.status.value.isOnline } }
        f.server.lastConnection.batch(StreamBatch(WORKSPACE_STREAM, f.server.head(WORKSPACE_STREAM), f.server.log(WORKSPACE_STREAM).toList()))
        delay(200)
        assertEquals(3, f.signals.size, "${f.signals}")
    }

    @Test
    fun unreadFollowsTheSummaryHeadAndTheLocalMarks() = withFixture { f ->
        f.server.snapshot = snapshot(0)
        f.connect()
        f.awaitOnline()
        fun entry() = f.engine.workspace.value.threads.single()
        assertEquals(false, entry().unread)
        f.server.append(WORKSPACE_STREAM, Event.ThreadUpserted(Samples.thread("thr_1", head = 8)))
        f.server.lastConnection.pushNew(WORKSPACE_STREAM)
        eventually(what = "unread") { entry().takeIf { it.unread } }
        f.engine.markViewed("thr_1")
        assertEquals(false, entry().unread)
        f.engine.markUnread("thr_1")
        assertEquals(true, entry().unread)
        f.engine.markViewed("thr_1")
        assertEquals(false, entry().unread)
    }

    @Test
    fun theStoredStateIsShownBeforeAnyConnection() = withFixture { f ->
        f.store.transaction { tx ->
            tx.setEpoch("epoch-1")
            tx.setCursor(WORKSPACE_STREAM, 3)
            tx.setLastSyncAtMs(1234)
            tx.upsertProject(Samples.project("prj_cached"))
            tx.upsertThread(Samples.thread("thr_1"))
            tx.upsertTurn(Samples.turn("trn_1"))
            tx.upsertItem(StoredItem(Samples.agentMessage("itm_1", "cached"), ItemPosition(0, -1)))
        }
        f.engine.start() // no credentials: nothing to connect to
        eventually(what = "not paired") { f.engine.status.value.connection.takeIf { it == ConnectionState.NotPaired } }
        assertEquals(listOf("prj_cached"), f.engine.workspace.value.projects.map { it.id })
        assertEquals(1234L, f.engine.status.value.lastSyncAtMs)
        val state = f.engine.openThread("thr_1")
        assertEquals(ThreadSync.Cached, state.value.sync)
        assertEquals(listOf("itm_1"), state.value.items.map { it.id })
    }

    @Test
    fun olderPagesLoadBeforeTheCachedTurns() = withFixture { f ->
        f.server.snapshot = snapshot(0)
        f.server.threadReads["thr_1"] = ThreadReadResult(
            Samples.thread("thr_1", head = 5), listOf(Samples.turn("trn_5", index = 5)),
            listOf(Samples.agentMessage("itm_5", "new", turnId = "trn_5")), emptyList(), emptyList(), 5, true,
        )
        f.connect()
        f.awaitOnline()
        val state = f.engine.openThread("thr_1")
        eventually(what = "live") { state.value.takeIf { it.sync == ThreadSync.Live } }
        assertTrue(state.value.hasMoreBefore)
        f.server.intercept = { conn, msg ->
            val p = msg.params?.jsonObject
            if (msg.method == "thread/read" && p?.get("beforeTurnIndex") != null) {
                assertEquals(5, p["beforeTurnIndex"]!!.jsonPrimitive.content.toInt())
                conn.respond(
                    msg,
                    dev.aas.android.protocol.AasJson.encodeToJsonElement(
                        ThreadReadResult.serializer(),
                        ThreadReadResult(
                            Samples.thread("thr_1", head = 5), listOf(Samples.turn("trn_4", index = 4, status = TurnStatus.Completed)),
                            listOf(Samples.agentMessage("itm_4", "old", turnId = "trn_4", status = ItemStatus.Completed)),
                            emptyList(), emptyList(), 5, false,
                        ),
                    ),
                )
                true
            } else {
                false
            }
        }
        assertEquals(false, f.engine.loadOlder("thr_1"))
        assertEquals(listOf("itm_4", "itm_5"), state.value.items.map { it.id })
        assertEquals(listOf(4, 5), state.value.turns.map { it.index })
        assertEquals(false, state.value.hasMoreBefore)
    }
}
