package dev.aas.android.notify

import android.Manifest
import android.app.Application
import android.app.NotificationManager
import androidx.test.core.app.ApplicationProvider
import androidx.test.ext.junit.runners.AndroidJUnit4
import android.os.Looper
import dev.aas.android.appContainer
import dev.aas.android.protocol.ErrorKind
import dev.aas.android.protocol.InteractionStatus
import dev.aas.android.protocol.Methods
import dev.aas.android.protocol.TurnStatus
import dev.aas.android.security.PairingInfo
import dev.aas.android.security.PairingState
import dev.aas.android.service.ConnectionService
import dev.aas.android.settings.TurnNotificationMode
import dev.aas.android.sync.FakeServer
import dev.aas.android.sync.OutboxEntry
import dev.aas.android.sync.OutboxResult
import dev.aas.android.sync.Samples
import dev.aas.android.sync.SyncSignal
import dev.aas.android.sync.eventually
import dev.aas.android.ui.TestAasApplication
import kotlinx.coroutines.flow.first
import kotlinx.coroutines.runBlocking
import kotlinx.coroutines.withTimeout
import kotlinx.serialization.json.JsonObject
import kotlinx.serialization.json.JsonPrimitive
import org.junit.Before
import org.junit.Test
import org.junit.runner.RunWith
import org.robolectric.Shadows.shadowOf
import org.robolectric.annotation.Config
import kotlin.test.assertEquals
import kotlin.test.assertNotNull
import kotlin.test.assertNull
import kotlin.test.assertTrue

/** Notifications posted for signals, with their actions, channels and removal. */
@RunWith(AndroidJUnit4::class)
@Config(application = TestAasApplication::class)
class NotifierTest {
    private val app = ApplicationProvider.getApplicationContext<Application>()
    private val container get() = app.appContainer
    private val manager get() = app.getSystemService(NotificationManager::class.java)

    /** Paired (nothing is posted for the engine's signals without a pairing). */
    @Before
    fun setUp() = runBlocking<Unit> {
        shadowOf(app).grantPermissions(Manifest.permission.POST_NOTIFICATIONS)
        // Bounded: a pairing that never becomes readable fails the test instead of hanging the run.
        withTimeout(SETUP_TIMEOUT_MS) {
            container.credentialStore.save(PairingInfo("ws://127.0.0.1:9/v1/ws", "home pc", "dev_1", "Pixel", 1), "token")
            container.pairingState.first { it is PairingState.Paired }
        }
    }

    private fun refused(crid: String, threadId: String) = OutboxResult.Failed(
        OutboxEntry(crid, Methods.ThreadArchive.name, JsonObject(mapOf("clientRequestId" to JsonPrimitive(crid), "threadId" to JsonPrimitive(threadId))), 1),
        FakeServer.rpcError(ErrorKind.InvalidState, "refused $crid"),
    )

    private fun active(tag: String) = manager.activeNotifications.firstOrNull { it.tag == tag }

    @Test
    fun anApprovalHasAllowOnceAndDenyAndGoesAwayWhenClosed() = runBlocking<Unit> {
        val interaction = Samples.approval("int_1", threadId = "thr_1")
        container.notifier.onSignal(SyncSignal.InteractionPending(interaction, Samples.thread("thr_1", title = "Fix the build")))

        val posted = assertNotNull(active(Notifier.interactionTag("int_1")))
        assertEquals(NotificationChannels.APPROVALS, posted.notification.channelId)
        val actions = posted.notification.actions.map { it.title.toString() }
        assertEquals(listOf("拒否", "許可（一度だけ）"), actions)
        assertTrue(posted.notification.extras.getString("android.title")!!.contains("Fix the build"))

        container.notifier.onSignal(SyncSignal.InteractionClosed("int_1", "thr_1", InteractionStatus.Resolved))
        assertNull(active(Notifier.interactionTag("int_1")))
    }

    /**
     * "Connected elsewhere" in the background: a notification that says notifications stopped,
     * with 再接続 starting the connection service as a foreground service (allowed from a
     * notification action) with the reconnect action.
     */
    @Test
    fun connectedElsewhereOffersAReconnectThatStartsTheServiceFromTheBackground() {
        container.notifier.showConnectedElsewhere("home pc")
        val posted = assertNotNull(active(Notifier.TAG_CONNECTED_ELSEWHERE))
        assertEquals(NotificationChannels.CONNECTION_ALERTS, posted.notification.channelId)
        assertEquals("別の場所で接続中です", posted.notification.extras.getString("android.title"))
        val action = posted.notification.actions.single()
        assertEquals("再接続", action.title.toString())
        val pending = shadowOf(action.actionIntent)
        assertTrue(pending.isForegroundService, "a foreground-service start, exempt from the background restriction")
        assertEquals(ConnectionService.ACTION_RECONNECT, pending.savedIntent.action)
        assertEquals(ConnectionService::class.java.name, pending.savedIntent.component?.className)
        // Tapping the notification opens the app (which reconnects when it comes to the foreground).
        assertTrue(shadowOf(posted.notification.contentIntent).isActivityIntent)

        container.notifier.cancelConnectedElsewhere()
        assertNull(active(Notifier.TAG_CONNECTED_ELSEWHERE))
    }

    @Test
    fun theConnectionNotificationReconnectsThroughAForegroundServiceStartToo() {
        val presentation = dev.aas.android.service.ConnectionPresentation(dev.aas.android.service.ConnectionSummary.ConnectedElsewhere, 0, null, null)
        val notification = container.notifier.connectionNotification(presentation, "home pc")
        val pending = shadowOf(notification.actions.single().actionIntent)
        assertTrue(pending.isForegroundService)
        assertEquals(ConnectionService.ACTION_RECONNECT, pending.savedIntent.action)
    }

    @Test
    fun staleInteractionNotificationsAreReconciledAway() = runBlocking<Unit> {
        container.notifier.onSignal(SyncSignal.InteractionPending(Samples.approval("int_a"), null))
        container.notifier.onSignal(SyncSignal.InteractionPending(Samples.approval("int_b"), null))
        container.notifier.reconcileInteractions(setOf("int_b"))
        assertNull(active(Notifier.interactionTag("int_a")))
        assertNotNull(active(Notifier.interactionTag("int_b")))
    }

    @Test
    fun turnNotificationsFollowTheSettingAndWhatIsOnScreen() = runBlocking<Unit> {
        val thread = Samples.thread("thr_2", title = "Refactor")
        val finished = SyncSignal.TurnFinished(thread, Samples.turnSummary("trn_1", 0, TurnStatus.Completed))

        container.settings.setTurnNotifications(TurnNotificationMode.WhenNotViewing)
        container.visibility.setAppInForeground(true)
        container.visibility.threadShown("thr_2")
        container.notifier.onSignal(finished)
        assertNull(active(Notifier.turnTag("thr_2")), "the thread is on screen")

        container.visibility.threadHidden("thr_2")
        container.notifier.onSignal(finished)
        val posted = assertNotNull(active(Notifier.turnTag("thr_2")))
        assertEquals(NotificationChannels.TURNS, posted.notification.channelId)

        manager.cancelAll()
        container.settings.setTurnNotifications(TurnNotificationMode.Never)
        container.notifier.onSignal(finished)
        assertNull(active(Notifier.turnTag("thr_2")))

        // A failed turn is an error, reported whatever the turn setting says.
        container.notifier.onSignal(SyncSignal.TurnFinished(thread, Samples.turnSummary("trn_2", 1, TurnStatus.Failed)))
        assertEquals(NotificationChannels.ERRORS, assertNotNull(active(Notifier.errorTag("thr_2"))).notification.channelId)
    }

    @Test
    fun aThreadShownOrReadInTheAppLosesItsTurnErrorAndRequestNotifications() = runBlocking<Unit> {
        container.settings.setTurnNotifications(TurnNotificationMode.Always)
        val a = Samples.thread("thr_a", title = "A")
        val b = Samples.thread("thr_b", title = "B")
        container.notifier.onSignal(SyncSignal.TurnFinished(a, Samples.turnSummary("trn_1", 0, TurnStatus.Completed)))
        container.notifier.onSignal(SyncSignal.TurnFinished(b, Samples.turnSummary("trn_2", 0, TurnStatus.Failed)))
        // Refused requests of one thread in the background share one notification: the latest.
        container.visibility.setAppInForeground(false)
        container.notifier.onResult(refused("c1", "thr_a"))
        container.notifier.onResult(refused("c2", "thr_a"))
        val requests = manager.activeNotifications.filter { it.id == Notifier.ID_REQUEST }
        assertEquals(listOf(Notifier.requestTag("thr_a")), requests.map { it.tag })
        assertTrue(requests.single().notification.extras.getCharSequence("android.text").toString().contains("refused c2"))
        assertNotNull(active(Notifier.turnTag("thr_a")))

        // Opening the thread in the app.
        container.visibility.setAppInForeground(true)
        container.visibility.threadShown("thr_a")
        assertNull(active(Notifier.turnTag("thr_a")))
        assertNull(active(Notifier.requestTag("thr_a")))
        assertNotNull(active(Notifier.errorTag("thr_b")))

        // Marked read elsewhere in the app (the thread list): only threads that became read.
        container.visibility.threadHidden("thr_a")
        container.notifier.threadsRead(setOf("thr_a"), previous = setOf("thr_a"))
        assertNotNull(active(Notifier.errorTag("thr_b")))
        container.notifier.threadsRead(setOf("thr_a", "thr_b"), previous = setOf("thr_a"))
        assertNull(active(Notifier.errorTag("thr_b")))
    }

    @Test
    fun postedNotificationsStayWithinTheBudgetAndApprovalsAreKept() = runBlocking<Unit> {
        container.settings.setTurnNotifications(TurnNotificationMode.Always)
        container.notifier.onSignal(SyncSignal.InteractionPending(Samples.approval("int_keep", threadId = "thr_0"), null))
        val budget = container.policy.notificationBudget
        repeat(budget + 5) { i ->
            container.notifier.onSignal(SyncSignal.TurnFinished(Samples.thread("thr_$i", title = "T$i"), Samples.turnSummary("trn_$i", 0, TurnStatus.Completed)))
        }
        assertTrue(manager.activeNotifications.size <= budget, "${manager.activeNotifications.size} posted, budget $budget")
        assertNotNull(active(Notifier.interactionTag("int_keep")), "approvals are never trimmed")
        assertNotNull(active(Notifier.turnTag("thr_${budget + 4}")), "the newest is posted")
    }

    @Test
    fun unpairingTakesTheNotificationsAlongAndAStaleButtonQueuesNothing() = runBlocking<Unit> {
        container.notifier.onSignal(SyncSignal.InteractionPending(Samples.approval("int_1", threadId = "thr_1"), Samples.thread("thr_1")))
        assertNotNull(active(Notifier.interactionTag("int_1")))

        container.pairingRepository.unpair()
        assertNull(active(Notifier.interactionTag("int_1")), "no approval with working buttons is left")
        // Nothing is posted for signals any more.
        container.notifier.onSignal(SyncSignal.InteractionPending(Samples.approval("int_2", threadId = "thr_1"), null))
        assertNull(active(Notifier.interactionTag("int_2")))

        // A button of a notification that was still on screen: refused, and said so.
        app.sendBroadcast(InteractionActionReceiver.intent(app, interactionId = "int_1", threadId = "thr_1", optionId = "allow"))
        shadowOf(Looper.getMainLooper()).idle()
        val notice = eventually(what = "the refusal") { active(Notifier.interactionTag("int_1")) }
        assertTrue(notice.notification.extras.getCharSequence("android.text").toString().contains("ペアリングが解除されている"))
        assertEquals(emptyList(), container.syncStore.transaction { it.outbox() })
    }

    @Test
    fun aRemovedThreadTakesItsNotificationsAlong() = runBlocking<Unit> {
        container.notifier.onSignal(SyncSignal.InteractionPending(Samples.approval("int_x", threadId = "thr_9"), null))
        container.notifier.onSignal(SyncSignal.ThreadRemoved("thr_9"))
        assertNull(active(Notifier.interactionTag("int_x")))
    }

    private companion object {
        const val SETUP_TIMEOUT_MS = 10_000L
    }
}
