package dev.aas.android.sync

import dev.aas.android.protocol.BackgroundProgress
import dev.aas.android.protocol.BackgroundResult
import dev.aas.android.protocol.BackgroundTaskKind
import dev.aas.android.protocol.BackgroundTaskStatus
import dev.aas.android.protocol.Harness
import dev.aas.android.protocol.HarnessKind
import dev.aas.android.protocol.InteractionStatus
import dev.aas.android.protocol.OperationStatus
import dev.aas.android.protocol.WORKSPACE_STREAM
import dev.aas.android.protocol.threadStream
import kotlinx.coroutines.CancellationException
import kotlinx.coroutines.runBlocking
import kotlinx.serialization.json.JsonObject
import kotlinx.serialization.json.JsonPrimitive
import kotlin.test.Test
import kotlin.test.assertEquals
import kotlin.test.assertFailsWith
import kotlin.test.assertFalse
import kotlin.test.assertNull
import kotlin.test.assertTrue

/**
 * The [SyncStore] contract as tests. Every implementation (the in-memory one here, Room in the
 * app) extends this class and provides [newStore]; the engine relies on exactly these rules.
 */
abstract class SyncStoreContract {
    /** A new, empty store. */
    abstract fun newStore(): SyncStore

    private fun test(block: suspend (SyncStore) -> Unit): Unit = runBlocking { block(newStore()) }

    private fun entry(crid: String, threadId: String? = null) = OutboxEntry(
        clientRequestId = crid,
        method = "turn/start",
        params = JsonObject(buildMap {
            put("clientRequestId", JsonPrimitive(crid))
            threadId?.let { put("threadId", JsonPrimitive(it)) }
        }),
        createdAtMs = 1,
    )

    @Test
    fun aFailingTransactionLeavesNoTrace() = test { store ->
        store.transaction { it.setCursor(WORKSPACE_STREAM, 1) }
        assertFailsWith<IllegalStateException> {
            store.transaction { tx ->
                tx.setCursor(WORKSPACE_STREAM, 5)
                tx.upsertProject(Samples.project("prj_1"))
                tx.addOutbox(entry("c1"))
                error("boom")
            }
        }
        store.transaction { tx ->
            assertEquals(1L, tx.cursor(WORKSPACE_STREAM))
            assertTrue(tx.projects().isEmpty())
            assertTrue(tx.outbox().isEmpty())
        }
    }

    @Test
    fun aCancelledTransactionLeavesNoTrace() = test { store ->
        assertFailsWith<CancellationException> {
            store.transaction { tx ->
                tx.setEpoch("e1")
                throw CancellationException("cancelled mid-way")
            }
        }
        store.transaction { assertNull(it.epoch()) }
    }

    @Test
    fun aTransactionSeesItsOwnWrites() = test { store ->
        store.transaction { tx ->
            tx.upsertThread(Samples.thread("thr_1", head = 3))
            tx.setCursor(threadStream("thr_1"), 7)
            assertEquals(3L, tx.thread("thr_1")?.head)
            assertEquals(mapOf(threadStream("thr_1") to 7L), tx.cursors())
        }
    }

    @Test
    fun metadataRoundTrips() = test { store ->
        store.transaction { tx ->
            assertNull(tx.epoch())
            assertNull(tx.modelVersion())
            assertNull(tx.cursor(WORKSPACE_STREAM))
            assertNull(tx.lastSyncAtMs())
            tx.setEpoch("e1")
            tx.setModelVersion(7)
            tx.setCursor(WORKSPACE_STREAM, 12)
            tx.setLastSyncAtMs(99)
        }
        store.transaction { tx ->
            assertEquals("e1", tx.epoch())
            assertEquals(7, tx.modelVersion())
            assertEquals(12L, tx.cursor(WORKSPACE_STREAM))
            assertEquals(99L, tx.lastSyncAtMs())
            assertEquals(ThreadMeta(), tx.threadMeta("thr_unknown"))
            assertNull(tx.viewState("thr_unknown"))
        }
    }

    @Test
    fun wipeKeepsOnlyTheOutbox() = test { store ->
        store.transaction { tx ->
            tx.setEpoch("e1")
            tx.setModelVersion(1)
            tx.setCursor(WORKSPACE_STREAM, 3)
            tx.setLastSyncAtMs(5)
            tx.replaceHarnesses(listOf(Harness("fake", HarnessKind.Fake, "Fake", available = true)))
            tx.upsertProject(Samples.project("prj_1"))
            tx.upsertThread(Samples.thread("thr_1"))
            tx.upsertTurn(Samples.turn("trn_1"))
            tx.upsertItem(StoredItem(Samples.agentMessage("itm_1", "a"), ItemPosition(0, 1)))
            tx.upsertInteraction(Samples.approval("int_1"))
            tx.upsertBackgroundTask(Samples.backgroundTask("bgt_1"))
            tx.replaceQueued("thr_1", listOf(Samples.queued("que_1")))
            tx.upsertOperation(Samples.operation("op_1", OperationStatus.Running))
            tx.setThreadMeta("thr_1", ThreadMeta(hasMoreBefore = true, commandsVersion = 2))
            tx.setViewState("thr_1", ThreadViewState(4, true))
            tx.addOutbox(entry("c1", "thr_1"))
            tx.wipeSyncedData()
            assertNull(tx.epoch())
            assertNull(tx.modelVersion())
            assertTrue(tx.cursors().isEmpty())
            assertNull(tx.lastSyncAtMs())
            assertTrue(tx.harnesses().isEmpty())
            assertTrue(tx.projects().isEmpty())
            assertTrue(tx.threads().isEmpty())
            assertTrue(tx.turnsOf("thr_1").isEmpty())
            assertTrue(tx.itemsOf("thr_1").isEmpty())
            assertTrue(tx.interactionsOf("thr_1").isEmpty())
            assertTrue(tx.backgroundTasksOf("thr_1").isEmpty())
            assertNull(tx.backgroundTask("bgt_1"))
            assertTrue(tx.queued("thr_1").isEmpty())
            assertTrue(tx.operations().isEmpty())
            assertEquals(ThreadMeta(), tx.threadMeta("thr_1"))
            assertTrue(tx.viewStates().isEmpty())
            assertEquals(listOf("c1"), tx.outbox().map { it.clientRequestId })
        }
    }

    @Test
    fun removeThreadRemovesEverythingOfThatThreadOnly() = test { store ->
        store.transaction { tx ->
            for (t in listOf("thr_1", "thr_2")) {
                tx.upsertThread(Samples.thread(t))
                tx.upsertTurn(Samples.turn("trn_$t", threadId = t))
                tx.upsertItem(StoredItem(Samples.agentMessage("itm_$t", "a", threadId = t, turnId = "trn_$t"), ItemPosition(0, 1)))
                tx.upsertInteraction(Samples.approval("int_$t", threadId = t))
                tx.upsertBackgroundTask(Samples.backgroundTask("bgt_$t", threadId = t, turnId = "trn_$t"))
                tx.replaceQueued(t, listOf(Samples.queued("que_$t", threadId = t)))
                tx.setThreadMeta(t, ThreadMeta(commandsVersion = 1))
                tx.setViewState(t, ThreadViewState(1))
                tx.setCursor(threadStream(t), 9)
            }
            tx.removeThread("thr_1")
        }
        store.transaction { tx ->
            assertNull(tx.thread("thr_1"))
            assertTrue(tx.turnsOf("thr_1").isEmpty())
            assertNull(tx.item("itm_thr_1"))
            assertNull(tx.interaction("int_thr_1"))
            assertNull(tx.backgroundTask("bgt_thr_1"))
            assertEquals(listOf("bgt_thr_2"), tx.backgroundTasksOf("thr_2").map { it.id })
            assertTrue(tx.queued("thr_1").isEmpty())
            assertEquals(ThreadMeta(), tx.threadMeta("thr_1"))
            assertNull(tx.viewState("thr_1"))
            assertNull(tx.cursor(threadStream("thr_1")))
            assertEquals(listOf("int_thr_2"), tx.pendingInteractions().map { it.id })
            assertEquals(1, tx.turnsOf("thr_2").size)
            assertEquals(9L, tx.cursor(threadStream("thr_2")))
        }
    }

    @Test
    fun clearThreadContentKeepsInteractionsSummaryMetaAndCursor() = test { store ->
        store.transaction { tx ->
            tx.upsertThread(Samples.thread("thr_1"))
            tx.upsertTurn(Samples.turn("trn_1"))
            tx.upsertItem(StoredItem(Samples.agentMessage("itm_1", "a"), ItemPosition(0, 1)))
            tx.upsertInteraction(Samples.approval("int_1"))
            tx.upsertBackgroundTask(Samples.backgroundTask("bgt_1"))
            tx.upsertBackgroundTask(Samples.backgroundTask("bgt_other", threadId = "thr_2"))
            tx.replaceQueued("thr_1", listOf(Samples.queued("que_1")))
            tx.setThreadMeta("thr_1", ThreadMeta(commandsVersion = 3))
            tx.setCursor(threadStream("thr_1"), 4)
            tx.clearThreadContent("thr_1")
            assertTrue(tx.turnsOf("thr_1").isEmpty())
            assertTrue(tx.itemsOf("thr_1").isEmpty())
            // A fresh thread/read brings the tasks of its turns and every running one again.
            assertTrue(tx.backgroundTasksOf("thr_1").isEmpty())
            assertEquals(listOf("bgt_other"), tx.backgroundTasksOf("thr_2").map { it.id })
            assertTrue(tx.queued("thr_1").isEmpty())
            assertEquals(1, tx.interactionsOf("thr_1").size)
            assertEquals("thr_1", tx.thread("thr_1")?.id)
            assertEquals(3, tx.threadMeta("thr_1").commandsVersion)
            assertEquals(4L, tx.cursor(threadStream("thr_1")))
        }
    }

    @Test
    fun contentIsReturnedInItsOrder() = test { store ->
        store.transaction { tx ->
            tx.upsertTurn(Samples.turn("trn_b", index = 1))
            tx.upsertTurn(Samples.turn("trn_a", index = 0))
            // Positions: live events (positive seq) after read items (negative) of the same turn.
            tx.upsertItem(StoredItem(Samples.agentMessage("itm_live", "x", turnId = "trn_b"), ItemPosition(1, 50)))
            tx.upsertItem(StoredItem(Samples.agentMessage("itm_read2", "x", turnId = "trn_b"), ItemPosition(1, -1)))
            tx.upsertItem(StoredItem(Samples.agentMessage("itm_read1", "x", turnId = "trn_b"), ItemPosition(1, -2)))
            tx.upsertItem(StoredItem(Samples.agentMessage("itm_old", "x", turnId = "trn_a"), ItemPosition(0, -7)))
            tx.upsertItem(StoredItem(Samples.agentMessage("itm_orphan", "x", turnId = "trn_x"), ItemPosition(ItemPosition.UNKNOWN_TURN, 3)))
            tx.upsertInteraction(Samples.approval("int_2", createdAt = 5))
            tx.upsertInteraction(Samples.approval("int_1", createdAt = 2))
            // Replacing an item keeps the position it is given.
            tx.upsertItem(StoredItem(Samples.agentMessage("itm_read1", "updated", turnId = "trn_b"), ItemPosition(1, -2)))
        }
        store.transaction { tx ->
            assertEquals(listOf("trn_a", "trn_b"), tx.turnsOf("thr_1").map { it.id })
            assertEquals(listOf("itm_old", "itm_read1", "itm_read2", "itm_live", "itm_orphan"), tx.itemsOf("thr_1").map { it.item.id })
            assertEquals("updated", (tx.item("itm_read1")!!.item as dev.aas.android.protocol.Item.AgentMessage).text)
            assertEquals(listOf("int_1", "int_2"), tx.interactionsOf("thr_1").map { it.id })
        }
    }

    @Test
    fun backgroundTasksAreReplacedWholeAndListedByStart() = test { store ->
        val shell = Samples.backgroundTask("bgt_b", kind = BackgroundTaskKind.Shell, startedAt = 5)
        store.transaction { tx ->
            tx.upsertBackgroundTask(shell)
            tx.upsertBackgroundTask(Samples.backgroundTask("bgt_c", startedAt = 2))
            tx.upsertBackgroundTask(Samples.backgroundTask("bgt_a", startedAt = 5))
            tx.upsertBackgroundTask(Samples.backgroundTask("bgt_x", threadId = "thr_2", startedAt = 1))
        }
        val ended = shell.copy(
            status = BackgroundTaskStatus.Completed,
            endedAt = 9,
            progress = BackgroundProgress(lastToolName = "Bash", toolUses = 3),
            result = BackgroundResult(exitCode = 0, output = "ok\n"),
        )
        store.transaction { tx -> tx.upsertBackgroundTask(ended) }
        store.transaction { tx ->
            assertEquals(listOf("bgt_c", "bgt_a", "bgt_b"), tx.backgroundTasksOf("thr_1").map { it.id })
            assertEquals(ended, tx.backgroundTask("bgt_b"), "the whole task, as last written")
            assertNull(tx.backgroundTask("bgt_missing"))
        }
    }

    @Test
    fun pendingInteractionsAreThePendingOnes() = test { store ->
        store.transaction { tx ->
            tx.upsertInteraction(Samples.approval("int_1"))
            tx.upsertInteraction(Samples.approval("int_2", status = InteractionStatus.Resolved))
            tx.upsertInteraction(Samples.approval("int_3", threadId = "thr_2"))
            assertEquals(setOf("int_1", "int_3"), tx.pendingInteractions().map { it.id }.toSet())
        }
    }

    @Test
    fun workspaceEntitiesUpsertAndRemove() = test { store ->
        store.transaction { tx ->
            tx.replaceHarnesses(listOf(Harness("a", HarnessKind.Fake, "A", true), Harness("b", HarnessKind.Fake, "B", true)))
            tx.replaceHarnesses(listOf(Harness("c", HarnessKind.Fake, "C", true)))
            tx.upsertHarness(Harness("c", HarnessKind.Fake, "C2", false))
            tx.upsertProject(Samples.project("prj_1", name = "one"))
            tx.upsertProject(Samples.project("prj_1", name = "uno"))
            tx.upsertProject(Samples.project("prj_2"))
            tx.removeProject("prj_2")
            tx.upsertOperation(Samples.operation("op_1", OperationStatus.Running))
            tx.upsertOperation(Samples.operation("op_1", OperationStatus.Cancelled))
            tx.setViewState("thr_1", ThreadViewState(3, false))
        }
        store.transaction { tx ->
            assertEquals(listOf("C2"), tx.harnesses().map { it.displayName })
            assertEquals(listOf("uno"), tx.projects().map { it.name })
            assertEquals(OperationStatus.Cancelled, tx.operation("op_1")?.status)
            assertEquals(mapOf("thr_1" to ThreadViewState(3, false)), tx.viewStates())
        }
    }

    @Test
    fun theOutboxKeepsItsOrder() = test { store ->
        store.transaction { tx ->
            tx.addOutbox(entry("c1"))
            tx.addOutbox(entry("c2"))
            tx.addOutbox(entry("c3"))
            tx.updateOutbox(entry("c1").copy(failures = 2, lastError = "draining", nextAttemptAtMs = 77))
            tx.updateOutbox(entry("c3").copy(failures = 1, lastError = "not logged in", nextAttemptAtMs = 5, waitingForHarness = "codex"))
            tx.updateOutbox(entry("missing"))
        }
        store.transaction { tx ->
            val entries = tx.outbox()
            assertEquals(listOf("c1", "c2", "c3"), entries.map { it.clientRequestId })
            assertEquals(2, entries[0].failures)
            assertEquals(77L, entries[0].nextAttemptAtMs)
            assertEquals(null, entries[0].waitingForHarness)
            assertEquals(entry("c3").copy(failures = 1, lastError = "not logged in", nextAttemptAtMs = 5, waitingForHarness = "codex"), entries[2])
            // Released: the wait is cleared again.
            tx.updateOutbox(entries[2].copy(waitingForHarness = null, nextAttemptAtMs = 0))
            assertEquals(null, tx.outbox()[2].waitingForHarness)
            assertTrue(tx.removeOutbox("c2"))
            assertFalse(tx.removeOutbox("c2"))
            assertEquals(listOf("c1", "c3"), tx.outbox().map { it.clientRequestId })
            tx.clearOutbox()
            assertTrue(tx.outbox().isEmpty())
        }
    }

    /** A chained entry keeps the entry it waits for, and its params change when it is released (6.3). */
    @Test
    fun aChainedEntryKeepsWhatItWaitsFor() = test { store ->
        store.transaction { tx ->
            tx.addOutbox(entry("c1").copy(method = "thread/create"))
            tx.addOutbox(entry("c2").copy(method = "thread/update", after = "c1"))
            tx.addOutbox(entry("c3").copy(after = "c2"))
        }
        store.transaction { tx ->
            assertEquals(listOf(null, "c1", "c2"), tx.outbox().map { it.after })
            // Released with the created thread's id, in place.
            tx.updateOutbox(entry("c2", threadId = "thr_new").copy(method = "thread/update", after = null))
            tx.updateOutbox(entry("c3", threadId = "thr_new").copy(after = "c2"))
        }
        store.transaction { tx ->
            val entries = tx.outbox()
            assertEquals(listOf("c1", "c2", "c3"), entries.map { it.clientRequestId })
            assertEquals(listOf(null, null, "c2"), entries.map { it.after })
            assertEquals(listOf(null, "thr_new", "thr_new"), entries.map { it.threadId })
        }
    }

    @Test
    fun theQueueIsReplacedAsAWhole() = test { store ->
        store.transaction { tx ->
            tx.replaceQueued("thr_1", listOf(Samples.queued("que_1"), Samples.queued("que_2")))
            tx.replaceQueued("thr_1", listOf(Samples.queued("que_2")))
            assertEquals(listOf("que_2"), tx.queued("thr_1").map { it.id })
        }
    }
}
