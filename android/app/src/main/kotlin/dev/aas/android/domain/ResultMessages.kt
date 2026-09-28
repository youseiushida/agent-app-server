package dev.aas.android.domain

import dev.aas.android.R
import dev.aas.android.protocol.Methods
import dev.aas.android.sync.OutboxDiscard
import dev.aas.android.sync.OutboxResult
import dev.aas.android.ui.common.UiText
import kotlinx.serialization.json.JsonObject
import kotlinx.serialization.json.JsonPrimitive
import kotlinx.serialization.json.booleanOrNull
import kotlinx.serialization.json.contentOrNull

/**
 * What the app tells the user about the final outcome of a queued request (snackbar in the
 * foreground; Notifier covers the background):
 *
 * * a definitive error: "<what> に失敗しました: <server message>";
 * * an answer to an interaction that was already answered elsewhere: say so, since the user's
 *   choice was not the one applied;
 * * everything else: nothing (the synced state shows the effect).
 */
object ResultMessages {
    /** [ownDeviceId]: this device's id (`SyncStatus.deviceId`), to tell "answered elsewhere" apart. */
    fun describe(result: OutboxResult, ownDeviceId: String?): UiText? = when (result) {
        is OutboxResult.Failed -> UiText.of(R.string.request_failed, UiText.of(RequestLabels.of(result.entry.method)), result.error.message)
        is OutboxResult.Succeeded -> if (result.entry.method == Methods.InteractionRespond.name && answeredElsewhere(result, ownDeviceId)) {
            UiText.of(R.string.answer_already_resolved)
        } else {
            null
        }
        is OutboxResult.Discarded -> null
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
