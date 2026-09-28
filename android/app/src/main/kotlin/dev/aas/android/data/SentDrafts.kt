package dev.aas.android.data

import dev.aas.android.protocol.RpcException
import dev.aas.android.sync.OutboxClearedException
import dev.aas.android.sync.PendingMutation
import kotlinx.coroutines.CancellationException
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.launch

/**
 * Messages sent from a composer, followed until the daemon's final answer.
 *
 * A message leaves its composer as soon as its request is in the outbox (it shows as waiting in
 * the conversation). The daemon may still refuse it definitively afterwards, e.g. `invalidState`
 * for the first message of a fork whose source thread moved on (protocol.md: fork again) or for
 * a thread archived meanwhile, `capabilityUnsupported`, `invalidParams`. The request then leaves
 * the outbox, and the draft (text, mentions, uploaded images) is given back to its composer
 * ([ComposerDrafts.giveBack]): at once while the screen is open, else when it opens next. The
 * shell says why (the failure snackbar, or a notification while the app is in the background).
 *
 * Followed in [scope], the app's (not a screen's): the answer may come long after the screen
 * was left. Like [ComposerDrafts], this lasts as long as the process: a refusal that arrives
 * after the process was restarted gives nothing back (the failure is still reported).
 */
class SentDrafts(private val scope: CoroutineScope, private val drafts: ComposerDrafts) {
    /** Follows [request], sent from the composer of [key] with [draft]. */
    fun follow(key: String, request: PendingMutation<*>, draft: Draft) {
        scope.launch {
            try {
                request.awaitAccepted()
            } catch (e: CancellationException) {
                throw e
            } catch (e: RpcException) {
                drafts.giveBack(key, draft)
            } catch (e: OutboxClearedException) {
                // Taken back by the user (送信を取り消す) or dropped with the pairing: the daemon
                // refused nothing, so nothing is given back.
            }
        }
    }
}
