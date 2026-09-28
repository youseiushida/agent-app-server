package dev.aas.android.data

import android.content.Context
import androidx.test.core.app.ApplicationProvider
import androidx.test.ext.junit.runners.AndroidJUnit4
import dev.aas.android.data.db.AasDatabase
import dev.aas.android.data.db.RoomSyncStore
import dev.aas.android.protocol.WORKSPACE_STREAM
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

    private companion object {
        /** Room's placeholder for the table name in the exported `createSql`. */
        const val TABLE_NAME_PLACEHOLDER = "\${TABLE_NAME}"
    }
}
