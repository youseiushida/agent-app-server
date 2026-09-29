package dev.aas.android.e2e

import androidx.test.ext.junit.runners.AndroidJUnit4
import androidx.test.uiautomator.By
import org.junit.Test
import org.junit.runner.RunWith

/**
 * The thread composer against the real daemon and the fake harness, as a user drives it: a draft
 * sends, a running turn turns the empty composer's button into 停止, a draft typed while the turn
 * runs is queued (the long press is the other delivery), `/` opens the palette above the input,
 * and 停止 ends the turn. The screenshots `composer-idle`, `composer-running` and
 * `composer-palette-open` are the composer's three looks (docs/android.md 25章).
 */
@RunWith(AndroidJUnit4::class)
class ComposerE2eTest : E2eTest() {
    @Test
    fun theComposerSendsQueuesStopsAndOpensThePalette() {
        flows.pairFresh(AppFlows.unique("e2e-composer"))
        flows.newProjectAndThread(AppFlows.unique("e2e-composer"), "@text ready")
        app.waitFor(app.inApp(By.text("ready")), "the first turn's answer", Waits.TURN_MS)

        // Idle with a draft: the button sends it.
        app.setText(flows.composer, DRAFT, "the composer")
        app.waitFor(app.inApp(By.desc(app.text("composer_send")).enabled(true)), "送信 (enabled)")
        control.screenshot("composer-idle")
        app.tap(app.inApp(By.desc(app.text("composer_send"))), "送信")
        app.waitFor(app.inApp(By.text("echo: $DRAFT")), "the answer to the draft", Waits.TURN_MS)

        // A running turn: the empty composer's button stops it.
        flows.send(LONG_TURN)
        app.waitFor(app.inApp(By.desc(app.text("composer_stop"))), "停止 while the turn runs", Waits.TURN_MS)
        control.screenshot("composer-running")

        // A draft while it runs goes to the daemon's queue (the settings' default).
        app.setText(flows.composer, QUEUED, "the composer")
        app.tap(app.inApp(By.desc(app.text("composer_queue"))), "キューに追加")
        app.waitFor(app.inApp(By.text(QUEUED)), "the queued message")

        // `/` opens the palette right above the input, the daemon's commands first.
        app.setText(flows.composer, "/", "the composer")
        app.waitFor(app.inApp(By.text("/model")), "the daemon's /model in the palette")
        control.screenshot("composer-palette-open")

        // Back to a composer without content (a blank draft sends nothing): 停止 ends the turn (the
        // queue pauses after an interrupted turn). An empty field would report its placeholder.
        app.setText(flows.composer, " ", "the composer")
        app.tap(app.inApp(By.desc(app.text("composer_stop"))), "停止")
        app.waitGone(app.inApp(By.desc(app.text("composer_stop"))), "停止 once the turn ended", Waits.TURN_MS)
    }

    private companion object {
        const val DRAFT = "Summarize the change"
        const val QUEUED = "Then run the tests"

        /** A turn that runs until it is interrupted (`@sleep` reacts to the interrupt). */
        const val LONG_TURN = "@sleep 120000"
    }
}
