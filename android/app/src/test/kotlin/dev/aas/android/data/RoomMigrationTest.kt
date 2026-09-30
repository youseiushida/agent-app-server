package dev.aas.android.data

import android.content.Context
import androidx.test.core.app.ApplicationProvider
import androidx.test.ext.junit.runners.AndroidJUnit4
import dev.aas.android.data.db.AasDatabase
import dev.aas.android.data.db.RoomSyncStore
import dev.aas.android.protocol.AasJson
import dev.aas.android.protocol.ClientInfo
import dev.aas.android.protocol.Disposition
import dev.aas.android.protocol.Harness
import dev.aas.android.protocol.HarnessFeatures
import dev.aas.android.protocol.Methods
import dev.aas.android.protocol.PlanModeFeature
import dev.aas.android.protocol.StoredModels
import dev.aas.android.protocol.TurnStartResult
import dev.aas.android.protocol.WorkspaceSnapshotResult
import dev.aas.android.sync.Credentials
import dev.aas.android.sync.FakeServer
import dev.aas.android.sync.SyncEngine
import dev.aas.android.sync.eventually
import dev.aas.android.testing.FAST_SYNC
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.SupervisorJob
import kotlinx.coroutines.cancel
import okhttp3.OkHttpClient
import kotlin.test.assertNull
import dev.aas.android.protocol.Thread
import dev.aas.android.protocol.WORKSPACE_STREAM
import dev.aas.android.sync.Samples
import dev.aas.android.sync.OutboxEntry
import kotlinx.coroutines.runBlocking
import kotlinx.serialization.json.Json
import kotlinx.serialization.json.JsonObject
import kotlinx.serialization.json.JsonPrimitive
import kotlinx.serialization.json.jsonArray
import kotlinx.serialization.json.jsonObject
import kotlinx.serialization.json.jsonPrimitive
import org.junit.After
import org.junit.Before
import org.junit.Test
import org.junit.runner.RunWith
import java.io.File
import kotlin.test.assertEquals

/**
 * Every upgrade step against the exported schema it starts from (docs/android.md 15.2): the
 * database is created exactly as the older version exported it (`schemas/…/<n>.json`), filled
 * the way that version wrote it, then opened by the current app. Room runs the migrations and
 * refuses to open when the result differs from the current schema, so a passing test means the
 * migration is complete and kept the user's data (the outbox above all).
 */
@RunWith(AndroidJUnit4::class)
class RoomMigrationTest {
    private val context: Context = ApplicationProvider.getApplicationContext()

    @Before
    @After
    fun deleteDatabase() {
        context.deleteDatabase(AasDatabase.FILE_NAME)
    }

    /** Creates the database of schema [version] from its exported description and runs [fill] on it. */
    private fun createExported(version: Int, fill: (android.database.sqlite.SQLiteDatabase) -> Unit) {
        val file = File("schemas/${AasDatabase::class.java.name}/$version.json")
        val schema = Json.parseToJsonElement(file.readText()).jsonObject.getValue("database").jsonObject
        val db = context.openOrCreateDatabase(AasDatabase.FILE_NAME, Context.MODE_PRIVATE, null)
        try {
            for (entity in schema.getValue("entities").jsonArray) {
                val table = entity.jsonObject.getValue("tableName").jsonPrimitive.content
                fun sql(element: kotlinx.serialization.json.JsonElement) =
                    element.jsonObject.getValue("createSql").jsonPrimitive.content.replace(TABLE_NAME_PLACEHOLDER, table)
                db.execSQL(sql(entity))
                entity.jsonObject["indices"]?.jsonArray?.forEach { db.execSQL(sql(it)) }
            }
            schema.getValue("setupQueries").jsonArray.forEach { db.execSQL(it.jsonPrimitive.content) }
            fill(db)
            db.version = version
        } finally {
            db.close()
        }
    }

    @Test
    fun version1UpgradesKeepingTheOutboxAndTheSyncedData() = runBlocking<Unit> {
        val params = """{"clientRequestId":"c1","threadId":"thr_1","input":[{"type":"text","text":"日本語"}]}"""
        createExported(1) { db ->
            db.execSQL("INSERT INTO meta (`key`, value) VALUES ('epoch', 'e1')")
            db.execSQL("INSERT INTO cursors (stream, seq) VALUES ('$WORKSPACE_STREAM', 42)")
            db.execSQL(
                "INSERT INTO outbox (client_request_id, method, params, created_at, failures, last_error, next_attempt_at) VALUES (?, ?, ?, ?, ?, ?, ?)",
                arrayOf<Any?>("c1", "turn/start", params, 5L, 2, "draining", 77L),
            )
            db.execSQL(
                "INSERT INTO outbox (client_request_id, method, params, created_at, failures, last_error, next_attempt_at) VALUES (?, ?, ?, ?, ?, ?, ?)",
                arrayOf<Any?>("c2", "thread/update", """{"clientRequestId":"c2"}""", 6L, 0, null, 0L),
            )
        }

        val db = AasDatabase.open(context)
        try {
            val store = RoomSyncStore(db)
            store.transaction { tx ->
                assertEquals("e1", tx.epoch())
                assertEquals(42L, tx.cursor(WORKSPACE_STREAM))
                val outbox = tx.outbox()
                assertEquals(listOf("c1", "c2"), outbox.map { it.clientRequestId }, "order kept")
                assertEquals(
                    OutboxEntry("c1", "turn/start", Json.parseToJsonElement(params).jsonObject, 5, 2, "draining", 77, waitingForHarness = null),
                    outbox[0],
                )
                assertEquals(null, outbox[1].waitingForHarness)
            }
            // The new column works like the others.
            store.transaction { tx ->
                tx.updateOutbox(tx.outbox()[0].copy(lastError = "not logged in", waitingForHarness = "codex"))
                tx.addOutbox(OutboxEntry("c3", "native/import", JsonObject(mapOf("clientRequestId" to JsonPrimitive("c3"))), 7, waitingForHarness = "claude"))
            }
            store.transaction { tx ->
                assertEquals(listOf("codex", null, "claude"), tx.outbox().map { it.waitingForHarness })
                assertEquals("not logged in", tx.outbox()[0].lastError)
            }
        } finally {
            db.close()
        }
    }

    @Test
    fun version2UpgradesWithAnEmptyTableOfBackgroundTasks() = runBlocking<Unit> {
        createExported(2) { db ->
            db.execSQL("INSERT INTO meta (`key`, value) VALUES ('epoch', 'e2')")
            db.execSQL("INSERT INTO threads (id, json) VALUES (?, ?)", arrayOf<Any?>("thr_1", AasJson.encodeToString(Thread.serializer(), Samples.thread("thr_1", head = 4))))
            db.execSQL(
                "INSERT INTO outbox (client_request_id, method, params, created_at, failures, last_error, next_attempt_at, waiting_for_harness) VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
                arrayOf<Any?>("c1", "turn/start", """{"clientRequestId":"c1","threadId":"thr_1"}""", 5L, 0, null, 0L, "codex"),
            )
        }
        val db = AasDatabase.open(context)
        try {
            val store = RoomSyncStore(db)
            store.transaction { tx ->
                assertEquals("e2", tx.epoch())
                assertEquals(4L, tx.thread("thr_1")?.head)
                assertEquals(listOf("codex"), tx.outbox().map { it.waitingForHarness })
                assertEquals(emptyList(), tx.backgroundTasksOf("thr_1"))
            }
            // The new table works like the others.
            val task = Samples.backgroundTask("bgt_1", threadId = "thr_1", title = "npm run dev")
            store.transaction { tx -> tx.upsertBackgroundTask(task) }
            store.transaction { tx -> assertEquals(listOf(task), tx.backgroundTasksOf("thr_1")) }
        } finally {
            db.close()
        }
    }

    @Test
    fun version3UpgradesWithUnchainedRequests() = runBlocking<Unit> {
        val params = """{"clientRequestId":"c1","threadId":"thr_1","input":[{"type":"text","text":"日本語"}]}"""
        createExported(3) { db ->
            db.execSQL("INSERT INTO meta (`key`, value) VALUES ('epoch', 'e3')")
            db.execSQL(
                "INSERT INTO outbox (client_request_id, method, params, created_at, failures, last_error, next_attempt_at, waiting_for_harness) VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
                arrayOf<Any?>("c1", "turn/start", params, 5L, 1, "draining", 9L, "codex"),
            )
        }
        val db = AasDatabase.open(context)
        try {
            val store = RoomSyncStore(db)
            store.transaction { tx ->
                assertEquals("e3", tx.epoch())
                assertEquals(
                    listOf(OutboxEntry("c1", "turn/start", Json.parseToJsonElement(params).jsonObject, 5, 1, "draining", 9, waitingForHarness = "codex", after = null)),
                    tx.outbox(),
                )
            }
            // The new column works like the others.
            store.transaction { tx ->
                tx.addOutbox(OutboxEntry("c2", "thread/update", JsonObject(mapOf("clientRequestId" to JsonPrimitive("c2"))), 7, after = "c1"))
                tx.updateOutbox(tx.outbox()[0].copy(failures = 2))
            }
            store.transaction { tx -> assertEquals(listOf(null, "c1"), tx.outbox().map { it.after }) }
        } finally {
            db.close()
        }
    }

    /**
     * The database an older build left (schema 4, the models of the app before
     * `StoredModels.VERSION` was recorded): its harness rows lack `features` (that build did not
     * know them) and it records no model version. The store reports none, so the next connection
     * reads everything again (docs/android.md 6.1, 15.2) although the epoch and the cursor match:
     * the harness's features are back, the request the older build queued is still sent, the
     * unread mark stays, and the version is recorded for the next start.
     */
    @Test
    fun dataOfABuildBeforeModelVersionsIsReadAgainOnTheNextConnection() = runBlocking<Unit> {
        val server = FakeServer()
        val claude = Samples.harness("claude")
        val features = HarnessFeatures(planMode = PlanModeFeature(implementPrompt = "Implement the plan."), rename = true)
        val thread = Samples.thread("thr_1", head = 7, harnessId = "claude")
        val turnStart = """{"clientRequestId":"c1","threadId":"thr_1","input":[{"type":"text","text":"日本語"}]}"""
        createExported(4) { db ->
            db.execSQL("INSERT INTO meta (`key`, value) VALUES ('epoch', ?)", arrayOf<Any?>(server.epoch))
            db.execSQL("INSERT INTO cursors (stream, seq) VALUES ('$WORKSPACE_STREAM', 5)")
            // What that build wrote for a harness: without the `features` it did not know.
            db.execSQL("INSERT INTO harnesses (id, position, json) VALUES ('claude', 0, ?)", arrayOf<Any?>(AasJson.encodeToString(Harness.serializer(), claude)))
            db.execSQL("INSERT INTO threads (id, json) VALUES ('thr_1', ?)", arrayOf<Any?>(AasJson.encodeToString(Thread.serializer(), thread)))
            db.execSQL("INSERT INTO view_states (thread_id, last_viewed_head, marked_unread) VALUES ('thr_1', 3, 0)")
            db.execSQL(
                "INSERT INTO outbox (client_request_id, method, params, created_at, failures, last_error, next_attempt_at, waiting_for_harness, after_request_id) " +
                    "VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
                arrayOf<Any?>("c1", "turn/start", turnStart, 5L, 0, null, 0L, null, null),
            )
        }
        server.snapshot = WorkspaceSnapshotResult(listOf(claude.copy(features = features)), emptyList(), listOf(thread), emptyList(), emptyList(), 9)
        server.reportedHeads[WORKSPACE_STREAM] = 9
        server.onRequest = { _, msg ->
            if (msg.method == Methods.TurnStart.name) {
                AasJson.encodeToJsonElement(TurnStartResult.serializer(), TurnStartResult(Disposition.Started, turnId = "trn_1"))
            } else {
                JsonObject(emptyMap())
            }
        }
        val db = AasDatabase.open(context)
        val scope = CoroutineScope(SupervisorJob() + Dispatchers.Default)
        try {
            val store = RoomSyncStore(db)
            assertNull(store.transaction { it.modelVersion() }, "that build recorded no model version")
            val engine = SyncEngine(store, OkHttpClient(), scope, ClientInfo("test", "0", "jvm"), FAST_SYNC)
            engine.start()
            eventually(what = "the stored workspace") { engine.workspace.value.harnesses.singleOrNull() }
            assertEquals(HarnessFeatures(), engine.workspace.value.harnesses.single().features, "what that build dropped")
            engine.setCredentials(Credentials(server.wsUrl, "tok"))
            eventually(what = "online") { engine.status.value.isOnline.takeIf { it } }

            assertEquals(1, server.requestsFor(Methods.WorkspaceSnapshot.name).size, "read again though the epoch and the cursor match")
            assertEquals(features, engine.workspace.value.harnesses.single().features)
            assertEquals(true, engine.workspace.value.threads.single().unread, "the unread mark stays")
            eventually(what = "the older build's request sent") { server.requestsFor(Methods.TurnStart.name).singleOrNull() }
            eventually(what = "outbox empty") { engine.outbox.value.takeIf { it.isEmpty() } }
            store.transaction { tx ->
                assertEquals(StoredModels.VERSION, tx.modelVersion())
                assertEquals(features, tx.harnesses().single().features)
                assertEquals(9L, tx.cursor(WORKSPACE_STREAM))
            }
            engine.stop().join()
        } finally {
            scope.cancel()
            server.close()
            db.close()
        }
    }

    private companion object {
        /** Room's placeholder for the table name in the exported `createSql`. */
        const val TABLE_NAME_PLACEHOLDER = "\${TABLE_NAME}"
    }
}
