package dev.aas.android.sync

import dev.aas.android.protocol.ErrorKind
import dev.aas.android.protocol.Event
import dev.aas.android.protocol.Item
import dev.aas.android.protocol.Methods
import dev.aas.android.protocol.RpcMessage
import dev.aas.android.protocol.ThreadReadResult
import dev.aas.android.protocol.WORKSPACE_STREAM
import dev.aas.android.protocol.WorkspaceSnapshotResult
import dev.aas.android.protocol.threadStream
import kotlinx.coroutines.CompletableDeferred
import kotlinx.coroutines.delay
import kotlinx.serialization.json.jsonObject
import kotlinx.serialization.json.jsonPrimitive
import java.io.File
import java.util.concurrent.atomic.AtomicBoolean
import kotlin.test.Test
import kotlin.test.assertEquals
import kotlin.test.assertNotNull
import kotlin.test.assertTrue

/**
 * Following threads: empty batches after retention, a thread that cannot be loaded, opening
 * and closing while the connection is being set up or live.
 */
class SyncEngineThreadsTest {
    private fun snapshot(head: Long) = WorkspaceSnapshotResult(emptyList(), emptyList(), listOf(Samples.thread("thr_1", head = 3), Samples.thread("thr_2", head = 3)), emptyList(), emptyList(), head)

    private fun threadRead(id: String, head: Long, vararg items: Item) = ThreadReadResult(
        thread = Samples.thread(id, head = head),
        turns = listOf(Samples.turn("trn_$id", threadId = id)),
        items = items.toList(),
        interactions = emptyList(),
        queued = emptyList(),
        head = head,
        hasMoreBefore = false,
    )

    private fun fixtureText(path: String): String {
        val root = System.getProperty("aas.fixtures") ?: error("system property aas.fixtures is not set (run through Gradle)")
        return File(root, path).readText()
    }

    @Test
    fun theEmptyBatchFixtureMovesTheCursorToItsHead() = withFixture { f ->
        f.server.snapshot = snapshot(5)
        repeat(5) { f.server.append(WORKSPACE_STREAM, Event.CommandsChanged) }
        f.connect()
        f.awaitOnline()
        assertEquals(5L, f.store.state.value.cursors[WORKSPACE_STREAM])
        // fixtures/protocol/notifications/stream_batch_empty.json: {stream: workspace, head: 400, events: []}.
        f.server.lastConnection.sendRaw(fixtureText("notifications/stream_batch_empty.json"))
        eventually(what = "cursor at the head") { f.store.state.value.cursors[WORKSPACE_STREAM]?.takeIf { it == 400L } }
        eventually(what = "the status mirrors it") { f.engine.status.value.cursors[WORKSPACE_STREAM]?.takeIf { it == 400L } }
        // An empty batch below the cursor changes nothing.
        f.server.lastConnection.batch(dev.aas.android.protocol.StreamBatch(WORKSPACE_STREAM, 7, emptyList()))
        delay(100)
        assertEquals(400L, f.store.state.value.cursors[WORKSPACE_STREAM])
    }

    @Test
    fun afterRetentionTheResumedStreamCatchesUpInsteadOfStallingForever() =
        withFixture(EngineFixture().also { it.server.clientTimeoutMs = 1_000 }) { f ->
            f.server.snapshot = snapshot(0)
            f.server.threadReads["thr_1"] = threadRead("thr_1", 10, Samples.agentMessage("itm_1", "hi", turnId = "trn_thr_1"))
            f.connect()
            f.awaitOnline()
            val state = f.engine.openThread("thr_1")
            eventually(what = "live") { state.value.takeIf { it.sync == ThreadSync.Live } }
            val stream = threadStream("thr_1")
            assertEquals(10L, f.store.state.value.cursors[stream])
            // Offline overnight: the thread's trailing `native` events 11..14 were removed by
            // retention, the head stays at 14.
            f.server.lastConnection.kill()
            f.server.reportedHeads[stream] = 14
            eventually(what = "reconnected") { f.server.connections.size.takeIf { it == 2 && f.engine.status.value.isOnline } }
            eventually(what = "cursor at the head") { f.store.state.value.cursors[stream]?.takeIf { it == 14L } }
            val subscribesAfterResume = f.server.requestsFor("subscribe").size
            // Heartbeats report the head for longer than the client timeout: nothing is stalled.
            val conn = f.server.lastConnection
            val until = System.currentTimeMillis() + 2_500
            while (System.currentTimeMillis() < until) {
                conn.heartbeat(mapOf(WORKSPACE_STREAM to 0, stream to 14))
                delay(100)
            }
            assertEquals(0, f.engine.status.value.stallResubscribes)
            assertEquals(subscribesAfterResume, f.server.requestsFor("subscribe").size, "no resubscription")
            assertEquals(2, f.server.connections.size)
            assertEquals(listOf("itm_1"), state.value.items.map { it.id }, "the content is unchanged")
        }

    @Test
    fun aThreadThatCannotBeReadFailsAloneAndTheSessionGoesOn() = withFixture { f ->
        f.server.snapshot = snapshot(0)
        // The server cannot decode the stored rows of thr_1: `internal`.
        val broken = AtomicBoolean(true)
        f.server.intercept = { conn, msg ->
            val id = msg.params?.jsonObject?.get("threadId")?.jsonPrimitive?.content
            if (msg.method == Methods.ThreadRead.name && id == "thr_1" && broken.get()) {
                conn.error(msg, ErrorKind.Internal)
                true
            } else {
                false
            }
        }
        f.server.threadReads["thr_1"] = threadRead("thr_1", 6, Samples.agentMessage("itm_1", "recovered", turnId = "trn_thr_1"))
        f.server.onRequest = { _, _ -> dev.aas.android.protocol.AasJson.encodeToJsonElement(dev.aas.android.protocol.TurnStartResult.serializer(), dev.aas.android.protocol.TurnStartResult(dev.aas.android.protocol.Disposition.Started, turnId = "trn_x")) }
        // Opened before connecting and never read on this device: the setup has to read it.
        val state = f.engine.openThread("thr_1")
        f.engine.enqueue(Methods.TurnStart, turnStart("thr_2", "for another thread"))
        f.connect()
        f.awaitOnline()
        val failed = eventually(what = "failed") { state.value.takeIf { it.sync == ThreadSync.Failed } }
        assertTrue(failed.loadError!!.message.contains("internal"), "${failed.loadError}")
        // The rest of the session goes on: the outbox is sent and the workspace stays live.
        eventually(what = "outbox sent") { f.store.state.value.outbox.takeIf { it.isEmpty() } }
        f.server.append(WORKSPACE_STREAM, Event.ProjectUpserted(Samples.project("prj_live")))
        f.server.lastConnection.pushNew(WORKSPACE_STREAM)
        eventually(what = "workspace live") { f.engine.workspace.value.projects.find { it.id == "prj_live" } }
        delay(300)
        assertEquals(1, f.server.connections.size, "no reconnect loop")
        assertEquals(1, f.server.requestsFor("initialize").size)
        // Batches of the failed thread's stream are not applied without a base.
        f.server.lastConnection.batch(dev.aas.android.protocol.StreamBatch(threadStream("thr_1"), 9, listOf(dev.aas.android.protocol.EventEnvelope(9, null, 1_009, Event.CommandsChanged))))
        delay(100)
        assertEquals(null, f.store.state.value.cursors[threadStream("thr_1")])
        // Once the server can read it, a retry loads it.
        broken.set(false)
        f.engine.retryThread("thr_1")
        eventually(what = "live after the retry") { state.value.takeIf { it.sync == ThreadSync.Live } }
        assertEquals(null, state.value.loadError)
        assertEquals(listOf("itm_1"), state.value.items.map { it.id })
        assertEquals(6L, f.store.state.value.cursors[threadStream("thr_1")])
    }

    @Test
    fun openingAThreadShowsItsStoredContentWhileTheSetupWaitsForTheServer() = withFixture { f ->
        f.store.transaction { tx ->
            tx.setEpoch("epoch-1")
            tx.setCursor(WORKSPACE_STREAM, 0)
            tx.upsertThread(Samples.thread("thr_1"))
            tx.upsertTurn(Samples.turn("trn_1"))
            tx.upsertItem(StoredItem(Samples.agentMessage("itm_1", "cached"), ItemPosition(0, -1)))
        }
        f.server.snapshot = snapshot(0)
        f.server.threadReads["thr_1"] = threadRead("thr_1", 4, Samples.agentMessage("itm_2", "fresh", turnId = "trn_thr_1"))
        // The resubscription of the setup gets no answer until released (a slow path, a daemon
        // waiting for its first harness probe).
        val release = CompletableDeferred<Unit>()
        val held = CompletableDeferred<Pair<FakeServer.Conn, RpcMessage>>()
        f.server.intercept = { conn, msg ->
            if (msg.method == Methods.Subscribe.name && !release.isCompleted && !held.isCompleted) {
                held.complete(conn to msg)
                true
            } else {
                false
            }
        }
        f.connect()
        val (conn, subscribe) = held.await()
        // The setup holds its lock now; opening and closing threads must not wait for it.
        val state = kotlinx.coroutines.withTimeout(1_000) { f.engine.openThread("thr_1") }
        assertEquals(listOf("itm_1"), state.value.items.map { it.id }, "the stored content at once")
        assertEquals(ThreadSync.Cached, state.value.sync)
        kotlinx.coroutines.withTimeout(1_000) { f.engine.closeThread("thr_1") }
        val again = kotlinx.coroutines.withTimeout(1_000) { f.engine.openThread("thr_1") }
        // Released: the setup completes and the thread is read and followed.
        release.complete(Unit)
        conn.respond(
            subscribe,
            dev.aas.android.protocol.AasJson.encodeToJsonElement(
                dev.aas.android.protocol.SubscribeResult.serializer(),
                dev.aas.android.protocol.SubscribeResult(listOf(dev.aas.android.protocol.SubscriptionStatus(WORKSPACE_STREAM, 0, dev.aas.android.protocol.SubscriptionState.Ok))),
            ),
        )
        f.awaitOnline()
        eventually(what = "live") { again.value.takeIf { it.sync == ThreadSync.Live } }
        assertEquals(listOf("itm_2"), again.value.items.map { it.id })
        assertEquals(1, f.server.connections.size)
    }

    @Test
    fun aThreadClosedAndReopenedWhileLiveIsReadAgain() = withFixture { f ->
        f.server.snapshot = snapshot(0)
        f.server.threadReads["thr_1"] = threadRead("thr_1", 3, Samples.agentMessage("itm_1", "a", turnId = "trn_thr_1"))
        f.connect()
        f.awaitOnline()
        val stream = threadStream("thr_1")
        val first = f.engine.openThread("thr_1")
        eventually(what = "live") { first.value.takeIf { it.sync == ThreadSync.Live } }
        f.engine.closeThread("thr_1")
        // While closed, the stream's batches are ignored, so its cursor falls behind.
        val missed = f.server.append(stream, Event.ItemCompleted(Samples.agentMessage("itm_1", "ab", turnId = "trn_thr_1")), seq = 4)
        f.server.lastConnection.batch(dev.aas.android.protocol.StreamBatch(stream, 4, listOf(missed)))
        f.server.threadReads["thr_1"] = threadRead("thr_1", 4, Samples.agentMessage("itm_1", "ab", turnId = "trn_thr_1"))
        val second = f.engine.openThread("thr_1")
        eventually(what = "read again") { f.server.requestsFor(Methods.ThreadRead.name).takeIf { it.size == 2 } }
        eventually(what = "live again") { second.value.takeIf { it.sync == ThreadSync.Live } }
        assertEquals("ab", (second.value.items.single() as Item.AgentMessage).text)
        assertEquals(4L, f.store.state.value.cursors[stream])
        assertNotNull(f.engine.thread("thr_1"))
    }
}
