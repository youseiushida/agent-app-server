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

    /**
     * A running shell's output (`backgroundTask/outputDelta`, docs/android.md 30): its card shows
     * the newest lines as they arrive; 出力の全文を表示 opens the output at its end and follows it;
     * scrolling back stops following and 最新の出力へ returns to the end. Once stopped, the whole
     * output the harness reported (with its last line) replaces the streamed one.
     */
    @Test
    fun aRunningShellsOutputIsShownAsItArrivesAndInFull() {
        flows.pairFresh(AppFlows.unique("e2e-output"))
        flows.newProjectAndThread(AppFlows.unique("e2e-output"), OUTPUT_PROMPT)

        app.waitFor(app.inApp(By.text(app.textPattern("turn_worked"))), "the end of the turn", Waits.TURN_MS)
        app.waitFor(app.inApp(By.text(app.text("bg_output_live"))), "the card's running output")
        // The card's output grows while the shell prints.
        val first = newestLine()
        app.waitUntil("newer output in the card", Waits.TURN_MS) { newestLine() > first }
        control.screenshot("thread-background-output")

        // The full output opens at its end and follows it: a line printed later comes into view.
        app.tap(app.inApp(By.text(app.text("bg_show_output"))), "出力の全文を表示")
        app.waitFor(app.inApp(By.text(app.text("output_live_note"))), "the live output")
        app.waitUntil("the newest lines on screen", Waits.TURN_MS) { newestLine() > first }
        val opened = newestLine()
        app.waitUntil("a later line followed", Waits.TURN_MS) { newestLine() >= opened + FOLLOW_LINES }
        control.screenshot("output-live")

        // Scrolling back stops following; 最新の出力へ returns to the end and follows again.
        val display = app.device.displayHeight
        app.device.swipe(app.device.displayWidth / 2, display * 3 / 10, app.device.displayWidth / 2, display * 8 / 10, SWIPE_STEPS)
        app.waitFor(app.inApp(By.desc(app.text("output_follow_latest"))), "最新の出力へ")
        control.screenshot("output-live-scrolled-back")
        app.tap(app.inApp(By.desc(app.text("output_follow_latest"))), "最新の出力へ")
        app.waitGone(app.inApp(By.desc(app.text("output_follow_latest"))), "最新の出力へ")
        val back = newestLine()
        app.waitUntil("a later line followed again", Waits.TURN_MS) { newestLine() >= back + FOLLOW_LINES }

        // Stopped: the whole output the harness reported replaces the streamed copy.
        app.device.pressBack()
        app.tap(app.inApp(By.desc(app.text("bg_stop_description", OUTPUT_TITLE))), "停止")
        app.waitFor(app.inApp(By.text(app.text("bg_stop_title"))), "the stop confirmation")
        app.tapNear(app.inApp(By.text(app.text("bg_stop_title"))), app.button(app.text("bg_stop")), "the dialog's 停止")
        app.waitFor(app.inApp(By.text(app.text("bg_section_ended", 1))), "one ended task", Waits.TURN_MS)
        app.tap(app.inApp(By.text(app.text("bg_section_title"))), "the section")
        app.tap(app.inApp(By.text(app.text("bg_ended_title", 1))), "終了した作業 (1)")
        app.waitFor(app.inApp(By.textContains("ran $OUTPUT_TITLE")), "the whole output's last line")
        app.waitGone(app.inApp(By.text(app.text("bg_output_live"))), "the running output's label")
        control.screenshot("thread-background-output-ended")
    }

    /**
     * The highest line number on screen (`web line <n>`, in the card or the output screen), 0
     * while none shows. Read from one dump of the screen: it changes many times a second.
     */
    private fun newestLine(): Int = app.screenTexts(app.appPackage).maxOfOrNull { lineNumbers(it).maxOrNull() ?: 0 } ?: 0


    private fun lineNumbers(text: String?): List<Int> =
        Regex("${Regex.escape(LINE_PREFIX)}(\\d+)").findAll(text.orEmpty()).map { it.groupValues[1].toInt() }.toList()

    private companion object {
        const val TITLE = "npm run dev"

        /** A shell that runs until it is stopped (`ms=0`), like a dev server. */
        const val PROMPT = "@bg dev kind=shell ms=0 $TITLE"

        const val OUTPUT_TITLE = "npm run web"

        /**
         * A shell that prints a line every 50 ms (the fake agent's pace for `ms=0`) until it is
         * stopped: 4000 lines keep it printing for over three minutes, while the test looks at them
         * (and stay below the daemon's 64 KiB inline limit).
         */
        const val OUTPUT_PROMPT = "@bg web kind=shell ms=0 output=4000 $OUTPUT_TITLE"

        /** Lines the shell prints in two seconds: how far past the newest line the view must follow. */
        const val FOLLOW_LINES = 40

        /** How the fake agent's output lines start (`<key> line <n>`). */
        const val LINE_PREFIX = "web line "

        /** Steps of the swipe that scrolls the output back (5 ms each: a quick flick of the thumb). */
        const val SWIPE_STEPS = 20
    }
}
