package dev.aas.android.data.db

import androidx.room.ColumnInfo
import androidx.room.Entity
import androidx.room.Index
import androidx.room.PrimaryKey

/*
 * Tables of the local sync store (docs/android.md, "Room のスキーマ").
 *
 * Protocol objects are kept whole as JSON (`AasJson`) in a `json` column, so fields a newer
 * server adds survive a round trip through the store. Only the columns the store queries or
 * orders by are separate.
 */

/** Scalar sync metadata (`epoch`, `lastSyncAt`); see [MetaKeys]. */
@Entity(tableName = "meta")
data class MetaEntity(
    @PrimaryKey val key: String,
    val value: String,
)

/** Read position (highest applied `seq`) of one stream. */
@Entity(tableName = "cursors")
data class CursorEntity(
    @PrimaryKey val stream: String,
    val seq: Long,
)

/** A harness; [position] keeps the server's order of the list. */
@Entity(tableName = "harnesses")
data class HarnessEntity(
    @PrimaryKey val id: String,
    val position: Int,
    val json: String,
)

@Entity(tableName = "projects")
data class ProjectEntity(
    @PrimaryKey val id: String,
    val json: String,
)

@Entity(tableName = "threads")
data class ThreadEntity(
    @PrimaryKey val id: String,
    val json: String,
)

@Entity(tableName = "operations")
data class OperationEntity(
    @PrimaryKey val id: String,
    val json: String,
)

@Entity(
    tableName = "turns",
    indices = [Index(value = ["thread_id", "turn_index"])],
)
data class TurnEntity(
    @PrimaryKey val id: String,
    @ColumnInfo(name = "thread_id") val threadId: String,
    @ColumnInfo(name = "turn_index") val turnIndex: Int,
    val json: String,
)

/** An item with its sort position (`ItemPosition`: turn index, then `sort_seq`). */
@Entity(
    tableName = "items",
    indices = [Index(value = ["thread_id", "turn_index", "sort_seq"])],
)
data class ItemEntity(
    @PrimaryKey val id: String,
    @ColumnInfo(name = "thread_id") val threadId: String,
    @ColumnInfo(name = "turn_index") val turnIndex: Int,
    @ColumnInfo(name = "sort_seq") val sortSeq: Long,
    val json: String,
)

@Entity(
    tableName = "interactions",
    indices = [Index(value = ["thread_id", "created_at"]), Index(value = ["pending", "created_at"])],
)
data class InteractionEntity(
    @PrimaryKey val id: String,
    @ColumnInfo(name = "thread_id") val threadId: String,
    /** 1 while the interaction's status is `pending`. */
    val pending: Boolean,
    @ColumnInfo(name = "created_at") val createdAt: Long,
    val json: String,
)

/**
 * A background task of a thread (schema version 3). [startedAt] is the start of its current run:
 * the store lists a thread's tasks by it, then by id.
 */
@Entity(
    tableName = "background_tasks",
    indices = [Index(value = ["thread_id", "started_at"])],
)
data class BackgroundTaskEntity(
    @PrimaryKey val id: String,
    @ColumnInfo(name = "thread_id") val threadId: String,
    @ColumnInfo(name = "started_at") val startedAt: Long,
    val json: String,
)

/** One queued input of a thread; [position] is its index in the queue. */
@Entity(tableName = "queued", primaryKeys = ["thread_id", "position"])
data class QueuedEntity(
    @ColumnInfo(name = "thread_id") val threadId: String,
    val position: Int,
    val id: String,
    val json: String,
)

@Entity(tableName = "thread_meta")
data class ThreadMetaEntity(
    @PrimaryKey @ColumnInfo(name = "thread_id") val threadId: String,
    @ColumnInfo(name = "has_more_before") val hasMoreBefore: Boolean,
    @ColumnInfo(name = "commands_version") val commandsVersion: Int,
)

/** Local unread state of a thread (never sent to the server). */
@Entity(tableName = "view_states")
data class ViewStateEntity(
    @PrimaryKey @ColumnInfo(name = "thread_id") val threadId: String,
    @ColumnInfo(name = "last_viewed_head") val lastViewedHead: Long,
    @ColumnInfo(name = "marked_unread") val markedUnread: Boolean,
)

/**
 * A mutating request waiting for its definitive answer. [seq] (auto-increment) is the order the
 * requests were made in; [clientRequestId] is unique.
 */
@Entity(
    tableName = "outbox",
    indices = [Index(value = ["client_request_id"], unique = true)],
)
data class OutboxEntity(
    @PrimaryKey(autoGenerate = true) val seq: Long = 0,
    @ColumnInfo(name = "client_request_id") val clientRequestId: String,
    val method: String,
    /** Complete params as JSON, including `clientRequestId`. */
    val params: String,
    @ColumnInfo(name = "created_at") val createdAt: Long,
    val failures: Int,
    @ColumnInfo(name = "last_error") val lastError: String?,
    @ColumnInfo(name = "next_attempt_at") val nextAttemptAt: Long,
    /** The harness the request waits for after `harnessUnavailable` (schema version 2). */
    @ColumnInfo(name = "waiting_for_harness") val waitingForHarness: String?,
)

/** Keys of the [MetaEntity] table. */
object MetaKeys {
    const val EPOCH = "epoch"
    const val LAST_SYNC_AT = "lastSyncAt"
}
