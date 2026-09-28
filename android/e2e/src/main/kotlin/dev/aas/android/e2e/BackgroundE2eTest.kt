package dev.aas.android.e2e

import androidx.test.ext.junit.runners.AndroidJUnit4
import androidx.test.uiautomator.By
import org.junit.Test
import org.junit.runner.RunWith

/**
 * Background work against the real daemon (the fake agent's `@bg`, docs/adapters/fake.md §5): a
 * shell the agent leaves running outlives its turn. The thread shows it in its バックグラウンド
 * section and on the command that launched it, the thread list says バックグラウンドで実行中 (1),
 * and 停止 (after its confirmation) asks the daemon to stop that one task; the harness reports
 * the stop and the section says so.
 */
@RunWith(AndroidJUnit4::class)
class BackgroundE2eTest : E2eTest() {
    @Test
    fun aBackgroundShellIsShownInTheThreadAndInTheListAndStoppedFromThePhone() {
        flows.pairFresh(AppFlows.unique("e2e-background"))
        flows.newProjectAndThread(AppFlows.unique("e2e-background"), PROMPT)

        // The turn ends; the shell goes on in the background.
        app.waitFor(app.inApp(By.text(app.textPattern("turn_worked"))), "the end of the turn", Waits.TURN_MS)
        app.waitFor(app.inApp(By.text(app.text("bg_section_title"))), "the バックグラウンド section")
        app.waitFor(app.inApp(By.text(app.text("bg_section_running", 1))), "one task running")
        app.waitFor(app.inApp(By.text(TITLE)), "the task's title")
        app.waitFor(app.inApp(By.desc(app.text("bg_stop_description", TITLE))), "its 停止 button")
        // The launching command's chip follows the task.
        app.waitFor(app.inApp(By.text(app.textPattern("bg_chip_running"))), "the command's chip")
        control.screenshot("thread-background")

        // The thread list says the agent works in the background.
        app.device.pressBack()
        app.waitFor(app.inApp(By.text(app.text("activity_background_count", 1))), "バックグラウンドで実行中 (1) in the thread list")
        control.screenshot("thread-list-background")
        app.tapUntil(app.inApp(By.text(PROMPT)), app.inApp(By.text(app.text("bg_section_title"))), "the thread")

        // 停止 asks first, then the daemon asks the harness; the task ends as stopped.
        app.tap(app.inApp(By.desc(app.text("bg_stop_description", TITLE))), "停止")
        app.waitFor(app.inApp(By.text(app.text("bg_stop_title"))), "the stop confirmation")
        control.screenshot("background-stop-dialog")
        app.tapNear(app.inApp(By.text(app.text("bg_stop_title"))), app.button(app.text("bg_stop")), "the dialog's 停止")
        app.waitGone(app.inApp(By.text(app.text("bg_stop_title"))), "the stop confirmation")
        // Nothing runs any more: the section folds to its counts, the ended task inside.
        app.waitFor(app.inApp(By.text(app.text("bg_section_ended", 1))), "one ended task", Waits.TURN_MS)
        app.waitGone(app.inApp(By.desc(app.text("bg_stop_description", TITLE))), "the 停止 button")
        app.waitFor(app.inApp(By.text(app.text("bg_chip_ended", app.text("bg_status_stopped")))), "the command's chip says stopped")
        app.tap(app.inApp(By.text(app.text("bg_section_title"))), "the section")
        app.tap(app.inApp(By.text(app.text("bg_ended_title", 1))), "終了した作業 (1)")
        app.waitFor(
            app.inApp(By.textStartsWith("${app.text("bg_kind_shell")} · ${app.text("bg_status_stopped")}")),
            "the stopped task's kind and status",
        )
        control.screenshot("thread-background-stopped")

        // The thread list no longer says background work runs.
        app.device.pressBack()
        app.waitGone(app.inApp(By.text(app.text("activity_background_count", 1))), "バックグラウンドで実行中 (1)")
    }

    private companion object {
        const val TITLE = "npm run dev"

        /** A shell that runs until it is stopped (`ms=0`), like a dev server. */
        const val PROMPT = "@bg dev kind=shell ms=0 $TITLE"
    }
}
