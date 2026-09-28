package dev.aas.android.data

import androidx.test.core.app.ApplicationProvider
import androidx.test.ext.junit.runners.AndroidJUnit4
import dev.aas.android.data.db.AasDatabase
import dev.aas.android.data.db.RoomSyncStore
import dev.aas.android.protocol.WORKSPACE_STREAM
import dev.aas.android.sync.OutboxEntry
import dev.aas.android.sync.Samples
import dev.aas.android.sync.SyncStore
import dev.aas.android.sync.SyncStoreContract
import dev.aas.android.sync.ThreadViewState
import kotlinx.coroutines.async
import kotlinx.coroutines.awaitAll
import kotlinx.coroutines.delay
import kotlinx.coroutines.runBlocking
import kotlinx.coroutines.yield
import kotlinx.serialization.json.JsonObject
import kotlinx.serialization.json.JsonPrimitive
import org.junit.After
import org.junit.Test
import org.junit.runner.RunWith
import kotlin.test.assertEquals
import kotlin.test.assertFailsWith
import kotlin.test.assertNull
import kotlin.test.assertTrue

/**
 * The Room store against the `:sync` store contract (atomicity including cancellation, wipe
 * keeps the outbox, removal, ordering), plus what only a real database shows: serialized
 * read-modify-write transactions, durability across reopening the file, and the outbox order
 * surviving a reopen.
 */
@RunWith(AndroidJUnit4::class)
class RoomSyncStoreTest : SyncStoreContract() {
    private val databases = ArrayList<AasDatabase>()

    override fun newStore(): SyncStore {
        val db = AasDatabase.inMemory(ApplicationProvider.getApplicationContext())
        databases += db
        return RoomSyncStore(db)
    }

    @After
    fun closeDatabases() {
        databases.forEach { it.close() }
        ApplicationProvider.getApplicationContext<android.content.Context>().deleteDatabase(FILE_DB)
    }

    @Test
    fun transactionsAreSerialized() = runBlocking<Unit> {
        val store = newStore()
        // Each block reads the cursor, yields to the others, and writes it + 1: without
        // serialization increments would be lost.
        (1..CONCURRENT_BLOCKS).map {
            async(kotlinx.coroutines.Dispatchers.Default) {
                store.transaction { tx ->
                    val before = tx.cursor(WORKSPACE_STREAM) ?: 0
                    yield()
                    delay(1)
                    tx.setCursor(WORKSPACE_STREAM, before + 1)
                }
            }
        }.awaitAll()
        store.transaction { assertEquals(CONCURRENT_BLOCKS.toLong(), it.cursor(WORKSPACE_STREAM)) }
    }

    @Test
    fun committedDataSurvivesReopeningTheDatabaseFile() = runBlocking<Unit> {
        val context = ApplicationProvider.getApplicationContext<android.content.Context>()
        context.deleteDatabase(FILE_DB)
        val first = androidx.room.Room.databaseBuilder(context, AasDatabase::class.java, FILE_DB).build()
        RoomSyncStore(first).transaction { tx ->
            tx.setEpoch("e1")
            tx.setCursor(WORKSPACE_STREAM, 42)
            tx.upsertThread(Samples.thread("thr_1", head = 7))
            tx.setViewState("thr_1", ThreadViewState(5, markedUnread = true))
            tx.addOutbox(entry("c1"))
            tx.addOutbox(entry("c2"))
        }
        // A failed block after the commit changes nothing on disk.
        assertFailsWith<IllegalStateException> {
            RoomSyncStore(first).transaction { tx ->
                tx.removeOutbox("c1")
                error("boom")
            }
        }
        first.close()

        val second = androidx.room.Room.databaseBuilder(context, AasDatabase::class.java, FILE_DB).build()
        try {
            RoomSyncStore(second).transaction { tx ->
                assertEquals("e1", tx.epoch())
                assertEquals(42L, tx.cursor(WORKSPACE_STREAM))
                assertEquals(7L, tx.thread("thr_1")?.head)
                assertEquals(ThreadViewState(5, true), tx.viewState("thr_1"))
                assertEquals(listOf("c1", "c2"), tx.outbox().map { it.clientRequestId })
            }
        } finally {
            second.close()
        }
    }

    @Test
    fun wipeThenResyncKeepsOutboxOrderAndAppendsAfterIt() = runBlocking<Unit> {
        val store = newStore()
        store.transaction { tx ->
            tx.addOutbox(entry("c1"))
            tx.addOutbox(entry("c2"))
            tx.setEpoch("e1")
            tx.upsertProject(Samples.project("prj_1"))
        }
        store.transaction { tx ->
            tx.wipeSyncedData()
            tx.setEpoch("e2")
            tx.addOutbox(entry("c3"))
        }
        store.transaction { tx ->
            assertEquals("e2", tx.epoch())
            assertTrue(tx.projects().isEmpty())
            assertEquals(listOf("c1", "c2", "c3"), tx.outbox().map { it.clientRequestId })
            assertNull(tx.lastSyncAtMs())
        }
    }

    @Test
    fun outboxParamsRoundTripExactly() = runBlocking<Unit> {
        val store = newStore()
        val params = JsonObject(
            mapOf(
                "clientRequestId" to JsonPrimitive("c1"),
                "threadId" to JsonPrimitive("thr_1"),
                "input" to kotlinx.serialization.json.JsonArray(listOf(JsonObject(mapOf("type" to JsonPrimitive("text"), "text" to JsonPrimitive("日本語 ✓"))))),
            ),
        )
        store.transaction { it.addOutbox(OutboxEntry("c1", "turn/start", params, createdAtMs = 99, failures = 1, lastError = "draining", nextAttemptAtMs = 1234)) }
        store.transaction { tx ->
            val stored = tx.outbox().single()
            assertEquals(params, stored.params)
            assertEquals("thr_1", stored.threadId)
            assertEquals(OutboxEntry("c1", "turn/start", params, 99, 1, "draining", 1234), stored)
        }
    }

    private fun entry(crid: String) =
        OutboxEntry(crid, "turn/start", JsonObject(mapOf("clientRequestId" to JsonPrimitive(crid))), createdAtMs = 1)

    private companion object {
        const val CONCURRENT_BLOCKS = 50
        const val FILE_DB = "room-sync-store-test.db"
    }
}
