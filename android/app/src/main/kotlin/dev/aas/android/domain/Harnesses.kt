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

/**
 * The harnesses that can list and import their own sessions (`native/list`, `native/import`,
 * capability `nativeSessions`), for 「PC のセッションを取り込む」 and `/resume`.
 *
 * The capabilities of an unavailable harness are unknown (its probe failed, protocol.md: the
 * server answers `native/list` for it with `harnessUnavailable`), so such a harness is neither
 * counted as importable nor dropped when it was asked for: its listing then shows why it cannot
 * be used, with 再確認, in place.
 *
 * `unable` (in [choices] and [preselect]) are the harnesses the server answered
 * `capabilityUnsupported` for on this screen: they cannot list their sessions, even while the
 * workspace still shows them unavailable (the server probes such a harness before it answers,
 * and the `harness/updated` it publishes can arrive after the answer). The screen never picks
 * one of them again by itself, so each refusal moves it on to another harness and it never lists
 * back and forth. While the workspace shows one of them importable (a newer CLI can have the
 * capability) it is still offered, and the user can pick it.
 */
object NativeSessionHarnesses {
    /** Harnesses known to list their sessions: available, with the capability. In the server's order. */
    fun importable(harnesses: List<Harness>): List<Harness> = harnesses.filter(::canList)

    /** Some harness can list its sessions: the import is offered (the thread list's menu, `/resume`). */
    fun canImport(harnesses: List<Harness>): Boolean = harnesses.any(::canList)

    /**
     * The harnesses the import screen offers, in the server's order: the importable ones, the
     * [selected] one whatever its state (the screen shows its listing or why that failed, so its
     * chip stays next to the others), and [requested] (the harness of the thread `/resume` came
     * from) while it is unavailable and not in [unable] (its capability is unknown; listing it
     * shows why it cannot be used).
     */
    fun choices(harnesses: List<Harness>, selected: String?, requested: String?, unable: Set<String> = emptySet()): List<Harness> =
        harnesses.filter { canList(it) || it.id == selected || (it.id == requested && mayList(it, unable)) }

    /**
     * The harness whose sessions the import screen lists first: [requested] (the harness of the
     * thread `/resume` came from), else the project's default harness ([projectDefault]), else the
     * first importable one; `null` when none can import. [requested] and [projectDefault] count
     * when they are listed and not known to lack the capability (importable, or unavailable).
     * Harnesses in [unable] are never picked.
     */
    fun preselect(harnesses: List<Harness>, requested: String?, projectDefault: String?, unable: Set<String> = emptySet()): String? {
        val pickable = harnesses.filterNot { it.id in unable }
        fun candidate(id: String?): String? = pickable.firstOrNull { it.id == id }?.takeIf { canList(it) || !it.available }?.id
        return candidate(requested) ?: candidate(projectDefault) ?: importable(pickable).firstOrNull()?.id
    }

    private fun canList(harness: Harness): Boolean = harness.available && harness.capabilities.nativeSessions

    /** Unavailable, so its capabilities are unknown, and the server has not said it cannot list. */
    private fun mayList(harness: Harness, unable: Set<String>): Boolean = !harness.available && harness.id !in unable
}
