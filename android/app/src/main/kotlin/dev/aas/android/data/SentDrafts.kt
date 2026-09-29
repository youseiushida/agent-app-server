package dev.aas.android.data

import dev.aas.android.protocol.RpcException
import dev.aas.android.protocol.ThreadCreateResult
import dev.aas.android.sync.OutboxChainBrokenException
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
 * A message chained after another request ([dev.aas.android.sync.SyncEngine.submitChain]) comes
 * back the same way when that request was refused: it was never sent.
 *
 * Followed in [scope], the app's (not a screen's): the answer may come long after the screen
 * was left. Like [ComposerDrafts], this lasts as long as the process: a refusal that arrives
 * after the process was restarted gives nothing back (the failure is still reported).
 */
class SentDrafts(private val scope: CoroutineScope, private val drafts: ComposerDrafts) {
    /** Follows [request], sent from the composer of [key] with [draft]. */
    fun follow(key: String, request: PendingMutation<*>, draft: Draft) {
        scope.launch { giveBackIfRefused(key, request, draft) }
    }

    /**
     * Follows a new thread's creation whose first message is [draft]: sent with it
     * (`thread/create { input }`, [request] `null`), or chained after it ([request], `/plan
     * <request>`, [dev.aas.android.data.ThreadRepository.createInPlanMode]). A refused creation
     * gives the draft back to the new-thread composer of [newThreadKey] (the message never
     * went); a refused request, once the thread exists, to the created thread's composer. A
     * creation taken back (取り消す on the new-thread screen) gives nothing back here: that screen
     * puts its draft back itself.
     */
    fun followCreation(newThreadKey: String, creation: PendingMutation<ThreadCreateResult>, request: PendingMutation<*>?, draft: Draft) {
        scope.launch {
            val threadId = try {
                creation.await().thread.id
            } catch (e: CancellationException) {
                throw e
            } catch (e: RpcException) {
                drafts.giveBack(newThreadKey, draft)
                return@launch
            } catch (e: OutboxClearedException) {
                return@launch
            } catch (e: IllegalArgumentException) {
                // An answer without a readable thread (SerializationException is one): the thread
                // exists, but the requests chained after it were dropped (the engine cannot tell
                // them their thread), so a message chained after it never went.
                if (request != null) drafts.giveBack(newThreadKey, draft)
                return@launch
            }
            if (request != null) giveBackIfRefused(ComposerDrafts.threadKey(threadId), request, draft)
        }
    }

    private suspend fun giveBackIfRefused(key: String, request: PendingMutation<*>, draft: Draft) {
        try {
            request.awaitAccepted()
        } catch (e: CancellationException) {
            throw e
        } catch (e: RpcException) {
            drafts.giveBack(key, draft)
        } catch (e: OutboxChainBrokenException) {
            // A request before it in its chain was refused or taken back (plan mode for
            // `/plan <request>`): the message itself was never sent.
            drafts.giveBack(key, draft)
        } catch (e: OutboxClearedException) {
            // Taken back by the user (送信を取り消す) or dropped with the pairing: the daemon
            // refused nothing, so nothing is given back.
        }
    }
}
