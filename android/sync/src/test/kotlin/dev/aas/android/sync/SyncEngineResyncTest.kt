package dev.aas.android.sync

import dev.aas.android.protocol.AasJson
import dev.aas.android.protocol.Disposition
import dev.aas.android.protocol.ErrorKind
import dev.aas.android.protocol.Event
import dev.aas.android.protocol.Harness
import dev.aas.android.protocol.HarnessFeatures
import dev.aas.android.protocol.HarnessListResult
import dev.aas.android.protocol.Methods
import dev.aas.android.protocol.PlanModeFeature
import dev.aas.android.protocol.RpcMessage
import dev.aas.android.protocol.StoredModels
import dev.aas.android.protocol.ThreadReadResult
import dev.aas.android.protocol.TurnStartResult
import dev.aas.android.protocol.WORKSPACE_STREAM
import dev.aas.android.protocol.WorkspaceSnapshotResult
import kotlinx.serialization.json.JsonObject
import kotlinx.serialization.json.buildJsonObject
import kotlinx.serialization.json.jsonObject
import kotlinx.serialization.json.jsonPrimitive
import kotlinx.serialization.json.put
import java.util.concurrent.CopyOnWriteArrayList
import kotlin.test.Test
import kotlin.test.assertEquals
import kotlin.test.assertTrue

/**
 * Data stored by another build of the app is read again (docs/android.md 6.1): the store records
 * the [StoredModels.VERSION] it was written with, and a store of another version (an app update,
 * or data from before versions were recorded) gets a full resync on the next connection, keeping
 * the outbox and the unread marks. A resumed connection reads the daemon's harnesses again
 * (`harness/list`) once it is established, so what changed on the daemon while the phone was
 * away shows — without the session waiting for the daemon's harness probes, and without the list
 * undoing a newer `harness/updated` or a snapshot.
 */
class SyncEngineResyncTest {
    /** Claude as an older build stored it: the build did not know `features`, so none were kept. */
    private val claudeAsStored = Samples.harness("claude")

    /** Claude as the daemon reports it. */
    private val claude = claudeAsStored.copy(features = HarnessFeatures(planMode = PlanModeFeature(), rename = true))

    /** The daemon's workspace head: its snapshot's, and what `subscribe` reports (its log is empty). */
    private val head = 9L

    private fun EngineFixture.serve(vararg harnesses: Harness) {
        server.snapshot = snapshot(*harnesses)
        server.reportedHeads[WORKSPACE_STREAM] = head
    }

    private fun snapshot(vararg harnesses: Harness) = WorkspaceSnapshotResult(
        harnesses = harnesses.toList(),
        projects = listOf(Samples.project("prj_1")),
        threads = listOf(Samples.thread("thr_1", head = 7, harnessId = "claude"), Samples.thread("thr_2", head = 7, harnessId = "claude")),
        pendingInteractions = emptyList(),
        operations = emptyList(),
        head = head,
    )

    /** What an older build left: same epoch and cursors, harnesses without features, one thread unread, one request waiting. */
    private suspend fun EngineFixture.storeOfAnOlderBuild(modelVersion: Int?) {
        store.transaction { tx ->
            tx.setEpoch(server.epoch)
            modelVersion?.let { tx.setModelVersion(it) }
            tx.setCursor(WORKSPACE_STREAM, 5)
            tx.replaceHarnesses(listOf(claudeAsStored))
            tx.upsertProject(Samples.project("prj_1"))
            tx.upsertThread(Samples.thread("thr_1", head = 7, harnessId = "claude"))
            tx.setViewState("thr_1", ThreadViewState(lastViewedHead = 3))
            tx.upsertThread(Samples.thread("thr_2", head = 7, harnessId = "claude"))
            tx.setViewState("thr_2", ThreadViewState(lastViewedHead = 7))
            tx.addOutbox(
                OutboxEntry(
                    "crid-kept",
                    Methods.TurnStart.name,
                    buildJsonObject {
                        put("clientRequestId", "crid-kept")
                        put("threadId", "thr_1")
                        put("input", AasJson.parseToJsonElement("""[{"type":"text","text":"hello"}]"""))
                    },
                    1,
                ),
            )
        }
    }

    private fun EngineFixture.answerTurnStarts() {
        server.onRequest = { _, msg ->
            if (msg.method == Methods.TurnStart.name) {
                AasJson.encodeToJsonElement(TurnStartResult.serializer(), TurnStartResult(Disposition.Started, turnId = "trn_1"))
            } else {
                JsonObject(emptyMap())
            }
        }
    }

    private fun EngineFixture.unread(threadId: String): Boolean =
        engine.workspace.value.threads.first { it.thread.id == threadId }.unread

    @Test
    fun aStoreFromBeforeModelVersionsIsReadAgainKeepingTheOutboxAndTheUnreadMarks() = withFixture { f ->
        f.storeOfAnOlderBuild(modelVersion = null)
        f.serve(claude)
        f.answerTurnStarts()
        f.connect()
        f.awaitOnline()
        assertEquals(1, f.server.requestsFor(Methods.WorkspaceSnapshot.name).size, "read again although epoch and cursor match")
        assertEquals(
            f.server.epoch,
            f.server.requestsFor(Methods.Initialize.name).single().second.params!!.jsonObject["lastKnownEpoch"]!!.jsonPrimitive.content,
        )
        assertEquals(claude.features, f.engine.workspace.value.harnesses.single().features, "what the older build dropped is back")
        assertEquals(StoredModels.VERSION, f.store.state.value.modelVersion)
        assertEquals(mapOf(WORKSPACE_STREAM to head), subscriptionsOf(f.server.requestsFor(Methods.Subscribe.name).single()))
        assertTrue(f.unread("thr_1"), "an unread thread stays unread")
        assertTrue(!f.unread("thr_2"))
        // The request of the older build is still sent, after the resync.
        eventually(what = "the kept request sent") { f.server.requestsFor(Methods.TurnStart.name).singleOrNull() }
        assertEquals("crid-kept", crid(f.server.requestsFor(Methods.TurnStart.name).single()))
        eventually(what = "outbox empty") { f.engine.outbox.value.takeIf { it.isEmpty() } }

        // Recorded: the next connection resumes (no second snapshot) and reads the harnesses again.
        f.server.lastConnection.kill()
        eventually(what = "reconnected") { f.engine.status.value.takeIf { it.isOnline && it.reconnects >= 1 } }
        assertEquals(1, f.server.requestsFor(Methods.WorkspaceSnapshot.name).size, "the recorded version is trusted")
        eventually(what = "harness/list") { f.server.requestsFor(Methods.HarnessList.name).singleOrNull() }
    }

    @Test
    fun aStoreOfAnotherVersionIsReadAgain() = withFixture { f ->
        // A newer build's store after a downgrade (debug builds only): not this build's shape either.
        f.storeOfAnOlderBuild(modelVersion = StoredModels.VERSION + 1)
        f.serve(claude)
        f.answerTurnStarts()
        f.connect()
        f.awaitOnline()
        assertEquals(1, f.server.requestsFor(Methods.WorkspaceSnapshot.name).size)
        assertEquals(StoredModels.VERSION, f.store.state.value.modelVersion)
        assertTrue(f.server.requestsFor(Methods.HarnessList.name).isEmpty(), "the snapshot carries the harnesses")
    }

    @Test
    fun aStoreOfThisVersionResumes() = withFixture { f ->
        f.storeOfAnOlderBuild(modelVersion = StoredModels.VERSION)
        f.serve(claude)
        f.answerTurnStarts()
        f.connect()
        f.awaitOnline()
        assertTrue(f.server.requestsFor(Methods.WorkspaceSnapshot.name).isEmpty(), "resumed from the stored cursor")
        assertEquals(mapOf(WORKSPACE_STREAM to 5L), subscriptionsOf(f.server.requestsFor(Methods.Subscribe.name).single()))
        assertTrue(f.unread("thr_1"))
    }

    /**
     * The daemon was updated while the phone was away: its harnesses have other features and a
     * new harness is configured, and no `harness/updated` after the stored cursor says so (the
     * stream's events are of before). Once the resumed connection is established and caught up,
     * it lists them; a request that waited for a harness the list reports available is sent at
     * once.
     */
    @Test
    fun aResumedConnectionReadsTheHarnessesAgainAfterItResubscribed() = withFixture { f ->
        val codexDown = Samples.harness("codex", available = false, reason = "not logged in")
        f.serve(claudeAsStored, codexDown)
        f.server.onRequest = { _, msg ->
            if (msg.method == Methods.TurnStart.name && f.server.harnessList == null) {
                dev.aas.android.protocol.RpcError(
                    ErrorKind.HarnessUnavailable.code,
                    "harness codex is unavailable",
                    buildJsonObject {
                        put("kind", ErrorKind.HarnessUnavailable.wire)
                        put("harnessId", "codex")
                        put("reason", "not logged in")
                    },
                )
            } else {
                AasJson.encodeToJsonElement(TurnStartResult.serializer(), TurnStartResult(Disposition.Started, turnId = "trn_1"))
            }
        }
        f.connect()
        f.awaitOnline()
        assertTrue(f.server.requestsFor(Methods.HarnessList.name).isEmpty(), "the first sync's snapshot has the harnesses")
        f.engine.enqueue(Methods.TurnStart, turnStart("thr_1", "hello"))
        eventually(what = "waiting for codex") { f.engine.outbox.value.singleOrNull()?.waitingForHarness }

        val pi = Samples.harness("pi")
        val codexUp = codexDown.copy(available = true, unavailableReason = null)
        f.server.harnessList = listOf(claude, codexUp, pi)
        f.server.lastConnection.kill()
        eventually(what = "reconnected") { f.engine.status.value.takeIf { it.isOnline && it.reconnects >= 1 } }
        eventually(what = "the daemon's list, in its order") { f.engine.workspace.value.harnesses.takeIf { it == listOf(claude, codexUp, pi) } }
        val second = f.server.requests.filter { it.first == 1 }.map { it.second.method }
        assertTrue(
            second.indexOf(Methods.HarnessList.name) > second.indexOf(Methods.Subscribe.name),
            "harness/list after the resubscription: $second",
        )
        assertEquals(1, f.server.requestsFor(Methods.WorkspaceSnapshot.name).size, "resumed, not resynced")
        // Codex is available now: the waiting request goes without waiting for a retry delay.
        eventually(what = "sent again") { f.server.requestsFor(Methods.TurnStart.name).takeIf { it.size == 2 } }
        eventually(what = "outbox empty") { f.engine.outbox.value.takeIf { it.isEmpty() } }
    }

    /**
     * Holds `harness/list` like a daemon that just started: it answers only once its first harness
     * probes finished (up to its handshake timeout). [answer] replies to the last one held.
     */
    private class HeldHarnessLists(server: FakeServer) {
        val held = CopyOnWriteArrayList<Pair<FakeServer.Conn, RpcMessage>>()

        init {
            server.intercept = { conn, msg ->
                if (msg.method == Methods.HarnessList.name) {
                    held += conn to msg
                    true
                } else {
                    false
                }
            }
        }

        fun answer(vararg harnesses: Harness) {
            val (conn, msg) = held.last()
            conn.respond(msg, AasJson.encodeToJsonElement(HarnessListResult.serializer(), HarnessListResult(harnesses.toList())))
        }
    }

    private suspend fun EngineFixture.reconnectAndAwaitHeldList(lists: HeldHarnessLists) {
        server.lastConnection.kill()
        eventually(what = "reconnected") { engine.status.value.takeIf { it.isOnline && it.reconnects >= 1 } }
        eventually(what = "harness/list sent") { lists.held.singleOrNull() }
    }

    /**
     * The daemon holds `harness/list` until its harness probes finish (right after the PC or the
     * daemon restarted). The resumed session does not wait for it: it is online, a thread opened
     * meanwhile loads and the outbox is sent; the list is applied when it comes. (The call timeout
     * is beyond the test's patience, so a session that waited for the answer would never get
     * online here.)
     */
    @Test
    fun aHarnessListTheDaemonHoldsDoesNotHoldUpTheResumedSession() =
        withFixture(EngineFixture(config = TEST_CONFIG.copy(callTimeoutMs = ENGINE_TEST_TIMEOUT_MS))) { f ->
            f.serve(claudeAsStored)
            f.server.threadReads["thr_2"] = ThreadReadResult(
                thread = Samples.thread("thr_2", head = 7, harnessId = "claude"),
                turns = emptyList(),
                items = emptyList(),
                interactions = emptyList(),
                queued = emptyList(),
                head = 7,
                hasMoreBefore = false,
            )
            f.answerTurnStarts()
            val lists = HeldHarnessLists(f.server)
            f.connect()
            f.awaitOnline()
            f.reconnectAndAwaitHeldList(lists)

            val thread = f.engine.openThread("thr_2")
            eventually(what = "the thread opened meanwhile loads") { thread.value.takeIf { it.sync == ThreadSync.Live } }
            f.engine.enqueue(Methods.TurnStart, turnStart("thr_1", "hello"))
            eventually(what = "the outbox is sent") { f.server.requestsFor(Methods.TurnStart.name).singleOrNull() }
            assertEquals(listOf(claudeAsStored), f.engine.workspace.value.harnesses, "nothing listed yet")

            lists.answer(claude)
            eventually(what = "the list applied") { f.engine.workspace.value.harnesses.takeIf { it == listOf(claude) } }
            assertEquals(1, lists.held.size)
        }

    /**
     * A `harness/updated` applied after the list was requested may be newer than the list (the
     * daemon read its harnesses, then published a probe result, and the event overtook the
     * answer): that harness keeps the event's state, the others take the list's.
     */
    @Test
    fun aHarnessEventAfterTheRequestIsNotUndoneByTheList() = withFixture { f ->
        val codexDown = Samples.harness("codex", available = false, reason = "not logged in")
        val codexUp = codexDown.copy(available = true, unavailableReason = null)
        f.serve(claudeAsStored, codexDown)
        val lists = HeldHarnessLists(f.server)
        f.connect()
        f.awaitOnline()
        f.reconnectAndAwaitHeldList(lists)

        f.server.append(WORKSPACE_STREAM, Event.HarnessUpdated(claude), seq = head + 1)
        f.server.lastConnection.pushNew(WORKSPACE_STREAM)
        eventually(what = "the event applied") { f.engine.workspace.value.harnesses.takeIf { claude in it } }
        // Read before the event: Claude as it was, Codex as it is now.
        lists.answer(claudeAsStored, codexUp)
        eventually(what = "the list applied") { f.engine.workspace.value.harnesses.takeIf { codexUp in it } }
        assertEquals(listOf(claude, codexUp), f.engine.workspace.value.harnesses)
    }

    /**
     * The list is requested only once the resumed workspace caught up to the head it subscribed
     * at, so no older event of the catch-up lands on top of it. Here the catch-up announces a
     * harness the daemon no longer has (its last `harness/updated` stays in the log); the list,
     * read after it, removes it.
     */
    @Test
    fun theListIsRequestedOnlyOnceTheCatchUpIsApplied() = withFixture { f ->
        f.serve(claude)
        val cursorsAtTheRequest = CopyOnWriteArrayList<Long>()
        f.server.intercept = { _, msg ->
            if (msg.method == Methods.HarnessList.name) cursorsAtTheRequest += f.store.state.value.cursors.getValue(WORKSPACE_STREAM)
            false
        }
        f.connect()
        f.awaitOnline()
        val gone = Samples.harness("gone")
        f.server.append(WORKSPACE_STREAM, Event.HarnessUpdated(gone), seq = head + 1)
        // The subscription reports a head two events further; those are delivered later.
        f.server.reportedHeads[WORKSPACE_STREAM] = head + 3
        f.server.lastConnection.kill()
        eventually(what = "reconnected") { f.engine.status.value.takeIf { it.isOnline && it.reconnects >= 1 } }
        eventually(what = "the catch-up's first part") { f.engine.workspace.value.harnesses.takeIf { gone in it } }
        f.server.append(WORKSPACE_STREAM, Event.ProjectUpserted(Samples.project("prj_2")), seq = head + 2)
        f.server.append(WORKSPACE_STREAM, Event.ProjectUpserted(Samples.project("prj_3")), seq = head + 3)
        f.server.lastConnection.pushNew(WORKSPACE_STREAM)
        eventually(what = "the list applied") { f.engine.workspace.value.harnesses.takeIf { it == listOf(claude) } }
        assertEquals(listOf(head + 3), cursorsAtTheRequest.toList(), "requested once the catch-up reached the subscribed head")
    }

    /**
     * A snapshot committed while the list is on its way (a workspace overlap read everything
     * again) is at least as new as the list: the list is not applied.
     */
    @Test
    fun aSnapshotWhileTheListIsOnItsWayWins() = withFixture { f ->
        f.serve(claudeAsStored)
        val lists = HeldHarnessLists(f.server)
        f.connect()
        f.awaitOnline()
        f.reconnectAndAwaitHeldList(lists)

        val pi = Samples.harness("pi")
        f.server.snapshot = snapshot(claude, pi).copy(head = head + 2)
        f.server.reportedHeads[WORKSPACE_STREAM] = head + 2
        // A merged delta that overlaps the cursor: the workspace is read again.
        f.server.append(WORKSPACE_STREAM, Event.ProjectUpserted(Samples.project("prj_2")), seq = head + 2, seqFrom = head - 1)
        f.server.lastConnection.pushNew(WORKSPACE_STREAM)
        eventually(what = "read again") { f.engine.workspace.value.harnesses.takeIf { it == listOf(claude, pi) } }
        assertEquals(2, f.server.requestsFor(Methods.WorkspaceSnapshot.name).size)

        lists.answer(claudeAsStored)
        eventually(what = "the list set aside") { f.logs.firstOrNull { it.contains("harness/list not applied") } }
        assertEquals(listOf(claude, pi), f.engine.workspace.value.harnesses)
    }

    @Test
    fun aRefusedHarnessListKeepsTheStoredListAndTheConnection() = withFixture { f ->
        f.serve(claudeAsStored)
        f.connect()
        f.awaitOnline()
        f.server.intercept = { conn, msg ->
            if (msg.method == Methods.HarnessList.name) {
                conn.send(dev.aas.android.protocol.RpcMessage(id = msg.id, error = FakeServer.rpcError(ErrorKind.Internal)))
                true
            } else {
                false
            }
        }
        f.server.harnessList = listOf(claude)
        f.server.lastConnection.kill()
        eventually(what = "reconnected") { f.engine.status.value.takeIf { it.isOnline && it.reconnects >= 1 } }
        val reported = eventually(what = "the failure reported") { f.engine.status.value.lastError?.takeIf { "harness/list" in it.message } }
        assertTrue(reported.message.contains("harness/list failed: internal"), reported.message)
        assertEquals(1, f.server.requestsFor(Methods.HarnessList.name).size)
        assertEquals(listOf(claudeAsStored), f.engine.workspace.value.harnesses, "the stored list stays")
        assertTrue(f.engine.status.value.isOnline, "the connection itself is fine")
        assertEquals(2, f.server.connections.size, "not reconnected for it")
    }
}
