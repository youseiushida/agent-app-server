package dev.aas.android.data.db

import android.content.Context
import androidx.room.Database
import androidx.room.Room
import androidx.room.RoomDatabase
import androidx.room.migration.Migration
import androidx.sqlite.db.SupportSQLiteDatabase

/**
 * The app's only database: the durable [dev.aas.android.sync.SyncStore] (synced workspace and
 * thread content, cursors, epoch, per-device unread state and the outbox).
 *
 * ## Schema versions and migrations
 * * [VERSION] is bumped for every schema change, and the schema of every version is exported to
 *   `android/app/schemas/` (checked in; it is the baseline migrations are written against).
 * * Every upgrade has an explicit [Migration] in [Migrations.ALL]. There is deliberately no
 *   destructive fallback on upgrade: the outbox holds requests the user made that the server may
 *   not have received yet, and the unread state exists only on this device, so dropping the
 *   tables would lose user intent. (The synced tables alone could be refetched from the server.)
 * * A downgrade (only possible with `adb install -r -d` of an older debug build) drops and
 *   recreates the tables: an older build cannot read a newer schema, and the synced data is
 *   fetched again on the next connection. The outbox is lost in that case; release builds never
 *   downgrade.
 */
@Database(
    entities = [
        MetaEntity::class,
        CursorEntity::class,
        HarnessEntity::class,
        ProjectEntity::class,
        ThreadEntity::class,
        OperationEntity::class,
        TurnEntity::class,
        ItemEntity::class,
        InteractionEntity::class,
        QueuedEntity::class,
        ThreadMetaEntity::class,
        ViewStateEntity::class,
        OutboxEntity::class,
    ],
    version = AasDatabase.VERSION,
    exportSchema = true,
)
abstract class AasDatabase : RoomDatabase() {
    abstract fun syncDao(): SyncDao

    companion object {
        /** Current schema version (see the class documentation for the migration rules). */
        const val VERSION = 2

        /** File name in the app's database directory (`%LOCALAPPDATA%`-like private storage). */
        const val FILE_NAME = "aas-sync.db"

        fun open(context: Context): AasDatabase =
            Room.databaseBuilder(context.applicationContext, AasDatabase::class.java, FILE_NAME)
                .configure()
                .build()

        /** A database that lives only in memory (tests). */
        fun inMemory(context: Context): AasDatabase =
            Room.inMemoryDatabaseBuilder(context.applicationContext, AasDatabase::class.java)
                .configure()
                .build()

        private fun Builder<AasDatabase>.configure(): Builder<AasDatabase> =
            // WAL: readers (the UI's store reads) never block the writer applying a batch.
            setJournalMode(JournalMode.WRITE_AHEAD_LOGGING)
                .addMigrations(*Migrations.ALL)
                .fallbackToDestructiveMigrationOnDowngrade(dropAllTables = true)
    }
}

/** Upgrade steps between schema versions, in order. Version 1 is the first schema. */
object Migrations {
    /**
     * 1 → 2: `outbox.waiting_for_harness`, the harness a request waits for after the server
     * answered `harnessUnavailable` (docs/android.md 6.3). Existing requests wait for nothing.
     */
    val MIGRATION_1_2: Migration = object : Migration(1, 2) {
        override fun migrate(db: SupportSQLiteDatabase) {
            db.execSQL("ALTER TABLE `outbox` ADD COLUMN `waiting_for_harness` TEXT")
        }
    }

    val ALL: Array<Migration> = arrayOf(MIGRATION_1_2)
}
