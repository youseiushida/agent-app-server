package dev.aas.android.e2e

import androidx.test.ext.junit.runners.AndroidJUnit4
import androidx.test.uiautomator.By
import org.junit.Assert.assertEquals
import org.junit.Test
import org.junit.runner.RunWith

/**
 * The harnesses' own features driven like a user against the real daemon and the fake harness
 * (docs/android.md 31): forks from any turn (「ここから分岐」 and 「このプロンプトを編集」 in the turn's
 * menu) and the app's `/plan` with the proposed plan implemented in the same thread and in a new
 * one.
 */
@RunWith(AndroidJUnit4::class)
class HarnessFeaturesE2eTest : E2eTest() {
    /**
     * Three turns; 「ここから分岐」 on the second opens a new thread with the first two turns that
     * goes on by itself; 「このプロンプトを編集」 on the second opens a new thread with only the
     * first turn and the second turn's prompt in its composer.
     */
    @Test
    fun forksBranchAtATurnOrEditItsPrompt() {
        flows.pairFresh(AppFlows.unique("e2e-fork"))
        flows.newProjectAndThread(AppFlows.unique("e2e-fork"), "@text first answer")
        app.waitFor(app.inApp(By.text("first answer")), "the first turn's answer", Waits.TURN_MS)
        flows.send(SECOND)
        app.waitFor(app.inApp(By.text("second answer")), "the second turn's answer", Waits.TURN_MS)
        flows.send("@text third answer")
        app.waitFor(app.inApp(By.text("third answer")), "the third turn's answer", Waits.TURN_MS)
        // The turn's end is recorded (its anchor with it) before the menu offers a fork there.
        app.waitFor(app.inApp(By.desc(app.text("turn_menu", 2))), "the second turn's menu")
        control.screenshot("thread-turn-menu")

        // ここから分岐: the fork holds the first two turns.
        openTurnMenu(2)
        app.tap(app.inApp(By.text(app.text("fork_here"))), "ここから分岐")
        app.waitUntil("the fork to open") { app.find(app.inApp(By.text("third answer"))) == null && app.find(app.inApp(By.text("second answer"))) != null }
        control.screenshot("thread-forked-at-turn")
        flows.send("@text after the second")
        app.waitFor(app.inApp(By.text("after the second")), "the fork's own turn", Waits.TURN_MS)
        app.device.pressBack()
        app.waitFor(app.inApp(By.text("third answer")), "the source thread, unchanged")

        // このプロンプトを編集: the fork holds the first turn; the second turn's prompt waits in its composer.
        openTurnMenu(2)
        app.tap(app.inApp(By.text(app.text("fork_edit_prompt"))), "このプロンプトを編集")
        app.waitUntil("the fork with the prompt to edit") { app.find(flows.composer)?.text == SECOND }
        assertEquals(null, app.find(app.inApp(By.text("second answer"))))
        app.waitFor(app.inApp(By.text("first answer")), "the first turn in the fork")
        control.screenshot("thread-edit-prompt")
        app.setText(flows.composer, "@text an edited second", "the composer")
        app.tap(app.inApp(By.desc(app.text("composer_send"))), "送信")
        app.waitFor(app.inApp(By.text("an edited second")), "the edited turn's answer", Waits.TURN_MS)
    }

    /**
     * `/plan <request>`: plan mode (the composer's プラン chip) and a proposed plan with
     * 「新しいスレッドで実装」 (a new thread starting with the harness's preamble and the plan) and
     * 「実装する」 (plan mode off, then the harness's own text).
     */
    @Test
    fun planProposesAPlanThatIsImplementedHereOrInANewThread() {
        flows.pairFresh(AppFlows.unique("e2e-plan"))
        flows.newProjectAndThread(AppFlows.unique("e2e-plan"), "@text ready")
        app.waitFor(app.inApp(By.text("ready")), "the first turn's answer", Waits.TURN_MS)

        app.setText(flows.composer, "/pl", "the composer")
        app.waitFor(app.inApp(By.text("/plan")), "/plan in the palette")
        app.setText(flows.composer, "/plan $REQUEST", "the composer")
        app.tap(app.inApp(By.desc(app.text("composer_send"))), "送信")
        app.waitFor(app.inApp(By.text(app.text("plan_chip"))), "the plan-mode chip")
        app.waitFor(app.inApp(By.text(app.text("proposed_plan_title"))), "the proposed plan", Waits.TURN_MS)
        app.waitFor(app.inApp(By.textContains("Look into: $REQUEST")), "the plan's first step")
        app.waitFor(app.inApp(app.button(app.text("proposed_plan_implement"))), "実装する")
        control.screenshot("thread-proposed-plan")

        // 新しいスレッドで実装: a new thread with the preamble and the plan as its first message.
        app.tap(app.inApp(app.button(app.text("proposed_plan_new_thread"))), "新しいスレッドで実装")
        app.waitFor(app.inApp(By.textStartsWith(PREAMBLE_START)), "the new thread's first message", Waits.TURN_MS)
        control.screenshot("thread-plan-new-thread")
        app.device.pressBack()

        // 実装する: plan mode off, and the harness's own text. Tapped once: a second tap would ask
        // for a second implementation while the first is on its way.
        app.waitFor(app.inApp(By.text(app.text("proposed_plan_title"))), "the source thread's plan")
        app.tap(app.inApp(app.button(app.text("proposed_plan_implement"))), "実装する")
        app.waitFor(app.inApp(By.text("echo: Implement the plan.")), "the implementation's turn", Waits.TURN_MS)
        app.waitGone(app.inApp(By.text(app.text("plan_chip"))), "the plan-mode chip")
    }

    private fun openTurnMenu(turnNumber: Int) {
        val menu = app.inApp(By.desc(app.text("turn_menu", turnNumber)))
        app.tapUntil(menu, app.inApp(By.text(app.text("fork_here"))), "the menu of turn $turnNumber")
    }

    private companion object {
        const val SECOND = "@text second answer"
        const val REQUEST = "Add a login page"

        /** The start of the fake harness's preamble for a new thread that implements a plan. */
        const val PREAMBLE_START = "Implement the plan below"
    }
}
