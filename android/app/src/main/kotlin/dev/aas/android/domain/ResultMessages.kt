package dev.aas.android.domain

import dev.aas.android.R
import dev.aas.android.protocol.Methods
import dev.aas.android.protocol.NativeRenameStatus
import dev.aas.android.protocol.RpcException
import dev.aas.android.protocol.ThreadUpdateResult
import dev.aas.android.sync.OutboxClearedException
import dev.aas.android.sync.OutboxDiscard
import dev.aas.android.sync.OutboxResult
import dev.aas.android.sync.PendingMutation
import dev.aas.android.ui.common.UiText
import kotlinx.serialization.json.JsonObject
import kotlinx.serialization.json.JsonPrimitive
import kotlinx.serialization.json.booleanOrNull
import kotlinx.serialization.json.contentOrNull

/**
 * What the app tells the user about the final outcome of a queued request (snackbar in the
 * foreground; Notifier covers the background):
 *
 * * a definitive error: "<what> に失敗しました: <server message>" (the harness's own words after
 *   a lead-in for `adapterError`; what to use instead for a typed session-switching command,
 *   [ErrorTexts]);
 * * an answer to an interaction that was already answered elsewhere: say so, since the user's
 *   choice was not the one applied;
 * * a request of a chain dropped because an earlier one failed: nothing (the earlier one's
 *   failure is the message);
 * * everything else: nothing (the synced state shows the effect).
 */
object ResultMessages {
    /** [ownDeviceId]: this device's id (`SyncStatus.deviceId`), to tell "answered elsewhere" apart. */
    fun describe(result: OutboxResult, ownDeviceId: String?): UiText? = when (result) {
        is OutboxResult.Failed -> ErrorTexts.requestFailed(UiText.of(RequestLabels.of(result.entry.method)), result.error)
        is OutboxResult.Succeeded -> if (result.entry.method == Methods.InteractionRespond.name && answeredElsewhere(result, ownDeviceId)) {
            UiText.of(R.string.answer_already_resolved)
        } else {
            null
        }
        is OutboxResult.Discarded -> null
        // The consequence of another request's failure, which that request's own result reports.
        is OutboxResult.Dropped -> null
    }

    /**
     * What a rename did to the native session (`nativeRename` of the `thread/update` answer, on
     * harnesses with the feature `rename`); `null` when the harness has no such name.
     */
    fun nativeRename(result: ThreadUpdateResult): UiText? {
        val rename = result.nativeRename ?: return null
        return when (rename.status) {
            NativeRenameStatus.Applied -> UiText.of(R.string.rename_native_applied)
            NativeRenameStatus.Pending -> UiText.of(R.string.rename_native_pending)
            NativeRenameStatus.Failed -> UiText.of(R.string.rename_native_failed, rename.message ?: "")
            // A status of a newer daemon: nothing to say about it.
            NativeRenameStatus.Unknown -> null
        }
    }

    /**
     * Waits for a rename's answer and says what happened to the native session ([nativeRename]).
     * A definitive refusal is `null` here: the shell reports it like any refused request; so is a
     * request dropped from the outbox (unpaired meanwhile).
     */
    suspend fun awaitNativeRename(pending: PendingMutation<ThreadUpdateResult>): UiText? = try {
        nativeRename(pending.await())
    } catch (e: RpcException) {
        null
    } catch (e: OutboxClearedException) {
        null
    }

    /** What discarding an outbox entry did. */
    fun discarded(outcome: OutboxDiscard): UiText = when (outcome) {
        OutboxDiscard.Discarded -> UiText.of(R.string.outbox_discarded)
        OutboxDiscard.InFlight -> UiText.of(R.string.outbox_discard_in_flight)
        OutboxDiscard.NotFound -> UiText.of(R.string.outbox_discard_gone)
    }

    /**
     * `alreadyResolved` and resolved by someone else (another device or the system), read from
     * the `InteractionRespondResult` without decoding the whole interaction. A second tap on this
     * device's own answer is not worth a message.
     */
    private fun answeredElsewhere(result: OutboxResult.Succeeded, ownDeviceId: String?): Boolean {
        val body = result.result as? JsonObject ?: return false
        if ((body[ALREADY_RESOLVED] as? JsonPrimitive)?.booleanOrNull != true) return false
        val resolvedBy = ((body[INTERACTION] as? JsonObject)?.get(RESOLVED_BY) as? JsonPrimitive)?.contentOrNull
        return resolvedBy == null || resolvedBy != ownDeviceId
    }

    private const val ALREADY_RESOLVED = "alreadyResolved"
    private const val INTERACTION = "interaction"
    private const val RESOLVED_BY = "resolvedBy"
}
