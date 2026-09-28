package dev.aas.android.domain

import dev.aas.android.protocol.Harness
import dev.aas.android.sync.OutboxEntry

/**
 * What the app says about a harness (settings, the new-thread sheet): the server's explicit
 * `available` / `unavailableReason`, or "確認中" while this app's own `harness/refresh` runs.
 * The reason is the probe's text, shown as it is (never interpreted).
 */
sealed interface HarnessState {
    data object Available : HarnessState

    data class Unavailable(val reason: String?) : HarnessState

    /** This app asked the server to probe the harness again and waits for the answer. */
    data object Probing : HarnessState

    companion object {
        fun of(harness: Harness, probing: Set<String>): HarnessState = when {
            harness.id in probing -> Probing
            harness.available -> Available
            else -> Unavailable(harness.unavailableReason)
        }
    }
}

/**
 * A request the server refused with `harnessUnavailable` and that waits for its harness
 * ([OutboxEntry.waitingForHarness]): it is sent when `harness/updated` reports the harness
 * available, and the user can refresh the harness or discard the request meanwhile.
 */
data class HarnessWait(
    val clientRequestId: String,
    val method: String,
    val harnessId: String,
    /** The harness's display name when the workspace knows it, else its id. */
    val harnessName: String,
    /** Why it cannot be used: the harness's current reason, else the one the refusal carried. */
    val reason: String?,
) {
    companion object {
        fun of(entry: OutboxEntry, harnesses: List<Harness>): HarnessWait? {
            val id = entry.waitingForHarness ?: return null
            val harness = harnesses.firstOrNull { it.id == id }
            val reason = harness?.takeIf { !it.available }?.unavailableReason ?: entry.lastError
            return HarnessWait(entry.clientRequestId, entry.method, id, harness?.displayName ?: id, reason)
        }

        /** The waiting requests among [outbox], oldest first. */
        fun all(outbox: List<OutboxEntry>, harnesses: List<Harness>): List<HarnessWait> = outbox.mapNotNull { of(it, harnesses) }
    }
}
