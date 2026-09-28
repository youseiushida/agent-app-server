package dev.aas.android.data.db

import androidx.room.Dao
import androidx.room.Insert
import androidx.room.OnConflictStrategy
import androidx.room.Query
import androidx.room.Upsert

/**
 * Every statement of the sync store. Called only inside `RoomSyncStore.transaction` (Room's
 * `withTransaction`), so each block of the engine is one SQLite transaction.
 */
@Dao
interface SyncDao {
    // ----- meta and cursors ---------------------------------------------------------------------

    @Query("SELECT value FROM meta WHERE `key` = :key")
    suspend fun meta(key: String): String?

    @Upsert
    suspend fun putMeta(entity: MetaEntity)

    @Query("DELETE FROM meta")
    suspend fun clearMeta()

    @Query("SELECT seq FROM cursors WHERE stream = :stream")
    suspend fun cursor(stream: String): Long?

    @Query("SELECT * FROM cursors")
    suspend fun cursors(): List<CursorEntity>

    @Upsert
    suspend fun putCursor(entity: CursorEntity)

    @Query("DELETE FROM cursors WHERE stream = :stream")
    suspend fun deleteCursor(stream: String)

    @Query("DELETE FROM cursors")
    suspend fun clearCursors()

    // ----- workspace ----------------------------------------------------------------------------

    @Query("SELECT * FROM harnesses ORDER BY position, id")
    suspend fun harnesses(): List<HarnessEntity>

    @Query("SELECT position FROM harnesses WHERE id = :id")
    suspend fun harnessPosition(id: String): Int?

    @Query("SELECT MAX(position) FROM harnesses")
    suspend fun maxHarnessPosition(): Int?

    @Upsert
    suspend fun upsertHarness(entity: HarnessEntity)

    @Query("DELETE FROM harnesses")
    suspend fun clearHarnesses()

    @Query("SELECT * FROM projects")
    suspend fun projects(): List<ProjectEntity>

    @Upsert
    suspend fun upsertProject(entity: ProjectEntity)

    @Query("DELETE FROM projects WHERE id = :id")
    suspend fun deleteProject(id: String)

    @Query("DELETE FROM projects")
    suspend fun clearProjects()

    @Query("SELECT * FROM threads")
    suspend fun threads(): List<ThreadEntity>

    @Query("SELECT * FROM threads WHERE id = :id")
    suspend fun thread(id: String): ThreadEntity?

    @Upsert
    suspend fun upsertThread(entity: ThreadEntity)

    @Query("DELETE FROM threads WHERE id = :id")
    suspend fun deleteThread(id: String)

    @Query("DELETE FROM threads")
    suspend fun clearThreads()

    @Query("SELECT * FROM operations")
    suspend fun operations(): List<OperationEntity>

    @Query("SELECT * FROM operations WHERE id = :id")
    suspend fun operation(id: String): OperationEntity?

    @Upsert
    suspend fun upsertOperation(entity: OperationEntity)

    @Query("DELETE FROM operations")
    suspend fun clearOperations()

    @Query("SELECT * FROM view_states")
    suspend fun viewStates(): List<ViewStateEntity>

    @Query("SELECT * FROM view_states WHERE thread_id = :threadId")
    suspend fun viewState(threadId: String): ViewStateEntity?

    @Upsert
    suspend fun putViewState(entity: ViewStateEntity)

    @Query("DELETE FROM view_states WHERE thread_id = :threadId")
    suspend fun deleteViewState(threadId: String)

    @Query("DELETE FROM view_states")
    suspend fun clearViewStates()

    // ----- thread content -----------------------------------------------------------------------

    @Query("SELECT * FROM turns WHERE id = :id")
    suspend fun turn(id: String): TurnEntity?

    @Upsert
    suspend fun upsertTurn(entity: TurnEntity)

    @Query("SELECT * FROM turns WHERE thread_id = :threadId ORDER BY turn_index, id")
    suspend fun turnsOf(threadId: String): List<TurnEntity>

    @Query("DELETE FROM turns WHERE thread_id = :threadId")
    suspend fun deleteTurnsOf(threadId: String)

    @Query("DELETE FROM turns")
    suspend fun clearTurns()

    @Query("SELECT * FROM items WHERE id = :id")
    suspend fun item(id: String): ItemEntity?

    @Upsert
    suspend fun upsertItem(entity: ItemEntity)

    @Query("SELECT * FROM items WHERE thread_id = :threadId ORDER BY turn_index, sort_seq, id")
    suspend fun itemsOf(threadId: String): List<ItemEntity>

    @Query("DELETE FROM items WHERE thread_id = :threadId")
    suspend fun deleteItemsOf(threadId: String)

    @Query("DELETE FROM items")
    suspend fun clearItems()

    @Query("SELECT * FROM interactions WHERE id = :id")
    suspend fun interaction(id: String): InteractionEntity?

    @Upsert
    suspend fun upsertInteraction(entity: InteractionEntity)

    @Query("SELECT * FROM interactions WHERE thread_id = :threadId ORDER BY created_at, id")
    suspend fun interactionsOf(threadId: String): List<InteractionEntity>

    @Query("SELECT * FROM interactions WHERE pending = 1 ORDER BY created_at, id")
    suspend fun pendingInteractions(): List<InteractionEntity>

    @Query("DELETE FROM interactions WHERE thread_id = :threadId")
    suspend fun deleteInteractionsOf(threadId: String)

    @Query("DELETE FROM interactions")
    suspend fun clearInteractions()

    @Query("SELECT * FROM background_tasks WHERE id = :id")
    suspend fun backgroundTask(id: String): BackgroundTaskEntity?

    @Upsert
    suspend fun upsertBackgroundTask(entity: BackgroundTaskEntity)

    @Query("SELECT * FROM background_tasks WHERE thread_id = :threadId ORDER BY started_at, id")
    suspend fun backgroundTasksOf(threadId: String): List<BackgroundTaskEntity>

    @Query("DELETE FROM background_tasks WHERE thread_id = :threadId")
    suspend fun deleteBackgroundTasksOf(threadId: String)

    @Query("DELETE FROM background_tasks")
    suspend fun clearBackgroundTasks()

    @Query("SELECT * FROM queued WHERE thread_id = :threadId ORDER BY position")
    suspend fun queuedOf(threadId: String): List<QueuedEntity>

    @Insert(onConflict = OnConflictStrategy.ABORT)
    suspend fun insertQueued(entities: List<QueuedEntity>)

    @Query("DELETE FROM queued WHERE thread_id = :threadId")
    suspend fun deleteQueuedOf(threadId: String)

    @Query("DELETE FROM queued")
    suspend fun clearQueued()

    @Query("SELECT * FROM thread_meta WHERE thread_id = :threadId")
    suspend fun threadMeta(threadId: String): ThreadMetaEntity?

    @Upsert
    suspend fun putThreadMeta(entity: ThreadMetaEntity)

    @Query("DELETE FROM thread_meta WHERE thread_id = :threadId")
    suspend fun deleteThreadMeta(threadId: String)

    @Query("DELETE FROM thread_meta")
    suspend fun clearThreadMeta()

    // ----- outbox -------------------------------------------------------------------------------

    @Query("SELECT * FROM outbox ORDER BY seq")
    suspend fun outbox(): List<OutboxEntity>

    /** Fails (constraint) when the `clientRequestId` already exists: ids are never reused. */
    @Insert(onConflict = OnConflictStrategy.ABORT)
    suspend fun insertOutbox(entity: OutboxEntity)

    @Query(
        """
        UPDATE outbox
        SET method = :method, params = :params, created_at = :createdAt, failures = :failures,
            last_error = :lastError, next_attempt_at = :nextAttemptAt, waiting_for_harness = :waitingForHarness
        WHERE client_request_id = :clientRequestId
        """,
    )
    suspend fun updateOutbox(
        clientRequestId: String,
        method: String,
        params: String,
        createdAt: Long,
        failures: Int,
        lastError: String?,
        nextAttemptAt: Long,
        waitingForHarness: String?,
    ): Int

    @Query("DELETE FROM outbox WHERE client_request_id = :clientRequestId")
    suspend fun deleteOutbox(clientRequestId: String): Int

    @Query("DELETE FROM outbox")
    suspend fun clearOutbox()
}
