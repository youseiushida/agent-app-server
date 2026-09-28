package dev.aas.android.notify

import android.content.BroadcastReceiver
import android.content.Context
import android.content.Intent
import android.net.Uri
import dev.aas.android.appContainer
import dev.aas.android.diagnostics.ConnectionLog
import dev.aas.android.protocol.InteractionId
import dev.aas.android.protocol.InteractionResolution
import dev.aas.android.protocol.InteractionRespondParams
import dev.aas.android.protocol.Methods
import dev.aas.android.protocol.ThreadId
import dev.aas.android.security.PairingState
import dev.aas.android.security.hasPairing
import dev.aas.android.service.ServiceStarter
import dev.aas.android.service.StartReason
import dev.aas.android.sync.SyncEngine
import kotlinx.coroutines.CancellationException
import kotlinx.coroutines.TimeoutCancellationException
import kotlinx.coroutines.launch
import kotlinx.coroutines.withTimeout

/** A notification action arrived while this device has no pairing: its answer was not queued. */
class NotPairedException : Exception("this device is not paired: the answer was not queued")

/**
 * Answers an approval from a notification action: commits `interaction/respond` to the durable
 * outbox (so the answer is sent as soon as there is a connection, even after the process dies)
 * and makes sure the connection service runs. Starting the foreground service from here is
 * allowed on Android 12+: a notification action is one of the background-start exemptions.
 *
 * Without a pairing ([pairing]) nothing is queued: the notification is from before unpairing,
 * and an answer queued now would go to whatever server the device pairs with next.
 */
class InteractionResponder(
    private val engine: SyncEngine,
    private val starter: ServiceStarter,
    private val pairing: suspend () -> PairingState,
) {
    /**
     * Returns the `clientRequestId` of the queued request.
     *
     * @throws NotPairedException no pairing is stored.
     */
    suspend fun respond(interactionId: InteractionId, optionId: String): String {
        if (!pairing().hasPairing) throw NotPairedException()
        val crid = engine.enqueue(Methods.InteractionRespond) { crid ->
            InteractionRespondParams(crid, interactionId, InteractionResolution.Approval(optionId))
        }
        starter.requestStart(StartReason.NotificationAction)
        return crid
    }
}

/** Receives the 許可（一度だけ）/ 拒否 actions of approval notifications (not exported). */
class InteractionActionReceiver : BroadcastReceiver() {
    override fun onReceive(context: Context, intent: Intent) {
        if (intent.action != ACTION_RESPOND) return
        val container = context.appContainer
        val interactionId = intent.getStringExtra(EXTRA_INTERACTION_ID)
        val optionId = intent.getStringExtra(EXTRA_OPTION_ID)
        val threadId: ThreadId? = intent.getStringExtra(EXTRA_THREAD_ID)
        if (interactionId == null || optionId == null) {
            container.connectionLog.warn(ConnectionLog.SOURCE_NOTIFY, "notification action without interaction or option: $intent")
            return
        }
        val pending = goAsync()
        container.applicationScope.launch {
            try {
                withTimeout(container.policy.notificationActionTimeoutMs) {
                    container.interactionResponder.respond(interactionId, optionId)
                }
                container.notifier.markSending(interactionId)
                container.connectionLog.info(ConnectionLog.SOURCE_NOTIFY, "answer to $interactionId queued from a notification")
            } catch (e: NotPairedException) {
                container.connectionLog.warn(ConnectionLog.SOURCE_NOTIFY, "answer to $interactionId not queued: not paired", e)
                container.notifier.showActionNotPaired(interactionId)
            } catch (e: TimeoutCancellationException) {
                container.connectionLog.warn(ConnectionLog.SOURCE_NOTIFY, "queuing the answer to $interactionId timed out", e)
                container.notifier.showActionFailed(interactionId, threadId, e.message ?: "timeout")
            } catch (e: CancellationException) {
                throw e
            } catch (e: Exception) {
                container.connectionLog.warn(ConnectionLog.SOURCE_NOTIFY, "queuing the answer to $interactionId failed", e)
                container.notifier.showActionFailed(interactionId, threadId, e.message ?: e.javaClass.simpleName)
            } finally {
                pending.finish()
            }
        }
    }

    companion object {
        const val ACTION_RESPOND = "dev.aas.android.action.RESPOND_INTERACTION"
        const val EXTRA_INTERACTION_ID = "interactionId"
        const val EXTRA_THREAD_ID = "threadId"
        const val EXTRA_OPTION_ID = "optionId"

        /** The explicit intent of one action; the data URI keeps the PendingIntents distinct. */
        fun intent(context: Context, interactionId: InteractionId, threadId: ThreadId, optionId: String): Intent =
            Intent(context, InteractionActionReceiver::class.java)
                .setAction(ACTION_RESPOND)
                .setData(Uri.Builder().scheme("aas-action").authority("respond").appendPath(interactionId).appendPath(optionId).build())
                .putExtra(EXTRA_INTERACTION_ID, interactionId)
                .putExtra(EXTRA_THREAD_ID, threadId)
                .putExtra(EXTRA_OPTION_ID, optionId)
    }
}
