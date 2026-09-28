package dev.aas.android.e2e

import android.os.SystemClock
import androidx.test.ext.junit.runners.AndroidJUnit4
import androidx.test.uiautomator.By
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNotEquals
import org.junit.Assert.assertTrue
import org.junit.Test
import org.junit.runner.RunWith

/**
 * The connection under stress, against the real daemon: dropped and silent (blackholed)
 * connections while output streams, a daemon restart in the middle of a turn, and the app's
 * process killed in the background.
 */
@RunWith(AndroidJUnit4::class)
class ReliabilityE2eTest : E2eTest() {
    @Test
    fun aDroppedConnectionReconnectsAndLosesNoOutput() {
        flows.pairFresh(AppFlows.unique("e2e-drop"))
        flows.newProjectAndThread(AppFlows.unique("e2e-drop"), "@stream $STREAM_COUNT $STREAM_INTERVAL_MS")
        flows.waitForStreamStart()
        // Every connection is cut; the link then stays slow, so the strip's reconnecting state
        // lasts long enough to be seen (a plain drop reconnects within the backoff's first step).
        control.chaos("drop", "delay $SLOW_LINK_MS")
        val title = flows.waitDisconnected(Waits.CONNECT_MS)
        assertTrue("the strip says it is reconnecting: $title", title in flows.reconnectingTitles)
        assertCutMidStream()
        control.screenshot("connection-reconnecting")
        control.chaos("pass")
        flows.waitConnected()
        // Nothing lost, nothing twice: exactly tok0 … tokN-1.
        flows.waitForStream(STREAM_COUNT)
    }

    @Test
    fun aBlackholedConnectionIsDetectedReconnectsAndLosesNoOutput() {
        flows.pairFresh(AppFlows.unique("e2e-blackhole"))
        flows.newProjectAndThread(AppFlows.unique("e2e-blackhole"), "@stream $STREAM_COUNT $STREAM_INTERVAL_MS")
        flows.waitForStreamStart()
        // Nothing arrives and nothing closes: the watchdog (the server's clientTimeoutMs) notices.
        control.chaos("blackhole")
        val title = flows.waitDisconnected(Waits.CONNECT_MS)
        assertTrue("the strip says it is reconnecting: $title", title in flows.reconnectingTitles)
        assertCutMidStream()
        // The next attempt runs into the blackhole too.
        SystemClock.sleep(BLACKHOLE_HOLD_MS)
        assertNotEquals(flows.connectedTitle, flows.connectionTitle())
        control.chaos("pass")
        flows.waitConnected()
        flows.waitForStream(STREAM_COUNT)
    }

    @Test
    fun aDaemonRestartMidTurnEndsTheTurnAndTheAppResumes() {
        flows.pairFresh(AppFlows.unique("e2e-restart"))
        flows.newProjectAndThread(AppFlows.unique("e2e-restart"), "@stream $STREAM_COUNT $STREAM_INTERVAL_MS")
        flows.waitForStreamStart()
        val ready = control.restart()
        assertEquals("the proxy keeps its address", server.wsUrl, ready.getString("wsUrl"))
        // The daemon ended the running turn when it stopped: the turn reads as stopped, with part
        // of the stream.
        app.waitFor(app.inApp(By.text(app.textPattern("turn_stopped_after"))), "the interrupted turn", Waits.TURN_MS)
        assertCutMidStream()
        // The app reconnects to the restarted daemon (same epoch: it resubscribes) and goes on.
        flows.waitConnected()
        flows.send("after the restart")
        app.waitFor(app.inApp(By.text("echo: after the restart")), "the next turn's answer", Waits.TURN_MS)
    }

    /**
     * The sticky connection service, restarted by the system after the process was killed,
     * reconnects without any activity and brings what happens after the kill.
     *
     * The order is made by explicit steps, not by timing: the message that asks for the approval
     * is sent while the phone has no network (airplane mode: the app does not even try), so it
     * waits in the outbox, which survives the kill. Only after the kill, the restart and the
     * network's return can the restarted process send it; the daemon then starts the turn and
     * asks for the approval. So the approval notification in the shade can only come from the
     * restarted process, and it can only come if that process reconnected (and flushed the
     * outbox) with no activity.
     */
    @Test
    fun theConnectionServiceComesBackAfterTheProcessIsKilled() {
        app.grantNotifications()
        flows.pairFresh(AppFlows.unique("e2e-kill"))
        flows.newProjectAndThread(AppFlows.unique("e2e-kill"), FIRST_PROMPT)
        app.waitFor(app.inApp(By.text("echo: $FIRST_PROMPT")), "the first turn's answer", Waits.TURN_MS)
        val approvalTitle = app.text("notify_approval_title", FIRST_PROMPT)
        try {
            app.setAirplaneMode(true)
            app.waitUntil("the strip to say the phone is offline", Waits.CONNECT_MS) { flows.connectionTitle() == app.text("connection_phone_offline") }
            flows.send(APPROVAL_PROMPT)
            app.goHome()
            val before = app.pid() ?: throw AssertionError("the app does not run")

            // `am kill` only kills processes that are safe to kill: the connection service is a
            // foreground service, so it keeps the process.
            app.shell("am kill ${app.appPackage}")
            SystemClock.sleep(AM_KILL_SETTLE_MS)
            assertEquals("the foreground service keeps the process from am kill", before, app.pid())

            // The low-memory killer's way: SIGKILL (as root; the emulator's userdebug image has su).
            app.shell("su 0 kill -9 $before")
            var restarted: Int? = null
            app.waitUntil("the system to restart the sticky connection service", Waits.SERVICE_RESTART_MS) {
                restarted = app.pid()
                restarted.let { it != null && it != before }
            }
            val after = restarted ?: throw AssertionError("the app's process went away again")
            app.waitUntil("the restarted connection service in the foreground", Waits.SERVICE_RESTART_MS) {
                // The class keeps its name in every build (a manifest component); the package differs (staging).
                app.shell("dumpsys activity services ${app.appPackage}/$CONNECTION_SERVICE").contains("isForeground=true")
            }
            assertEquals("the restarted process runs no activity", emptyList<String>(), app.activitiesIn(after))
            // Still no network: the message waits, and nothing asked for an approval yet.
            assertFalse("no approval before the restarted process sent the message", flows.notificationShown(approvalTitle))
            flows.closeNotificationShade()

            // The network comes back: without any activity, the restarted service reconnects,
            // sends the waiting message, and gets the approval the turn asks for.
            app.setAirplaneMode(false)
            flows.tapNotificationAction(title = approvalTitle, action = app.text("action_allow_once"))
            app.goHome()
            assertEquals("the same process answered", after, app.pid())
            assertEquals("no activity was started to get here", emptyList<String>(), app.activitiesIn(after))
        } finally {
            app.setAirplaneMode(false)
        }
        app.launch()
        app.waitFor(app.inApp(By.text("resumed after the kill")), "the rest of the turn", Waits.TURN_MS)
        flows.waitConnected()
    }

    /**
     * The connection was cut (or the daemon stopped) while the turn still streamed: the message
     * as the app has it then lacks its last token. Without this, a stream that had already ended
     * before the cut would pass the test without ever resuming mid-stream.
     */
    private fun assertCutMidStream() {
        val partial = app.waitFor(app.inApp(By.textStartsWith("tok0 ")), "the partial stream").text
        assertFalse("the stream was still running when the connection was cut: $partial", partial.endsWith("tok${STREAM_COUNT - 1}"))
    }

    private companion object {
        const val CONNECTION_SERVICE = "dev.aas.android.service.ConnectionService"

        /**
         * `@stream`: 200 deltas, 100 ms apart (20 s): still running after the new-thread screen
         * gave way to the thread (about 5 s on the debug build, more on a busy emulator) and the
         * control channel's round trip, so the connection is cut in the middle (checked).
         */
        const val STREAM_COUNT = 200
        const val STREAM_INTERVAL_MS = 100

        /** The chaos proxy's delay per chunk after a drop: each reconnect round trip takes seconds. */
        const val SLOW_LINK_MS = 1_000

        /** How long the blackhole stays after the app noticed it: longer than the server's clientTimeoutMs (1.5 s). */
        const val BLACKHOLE_HOLD_MS = 3_000L

        /** Time for a process that `am kill` ended to be gone (it kills at once; this covers the exit). */
        const val AM_KILL_SETTLE_MS = 1_000L

        /** The kill test's first message: it creates the thread (and names it) before the network goes. */
        const val FIRST_PROMPT = "before the kill"

        /** The kill test's message sent without a network: its turn asks for an approval, then says it went on. */
        const val APPROVAL_PROMPT = "@approve echo after-the-kill\n@text resumed after the kill"
    }
}
