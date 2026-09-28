package dev.aas.android.e2e

import androidx.test.ext.junit.runners.AndroidJUnit4
import androidx.test.uiautomator.By
import org.junit.Test
import org.junit.runner.RunWith

/**
 * `/resume` against the real daemon: a conversation started on the PC (a session of the fake
 * CLI, recorded by the test server's `native-session` in a folder under the root) is imported
 * from a thread of the same project and continues on the phone; chosen again, it opens the same
 * thread instead of importing it twice.
 */
@RunWith(AndroidJUnit4::class)
class ResumeE2eTest : E2eTest() {
    @Test
    fun resumeImportsASessionOfThePcAndOpensItsThread() {
        flows.pairFresh(AppFlows.unique("e2e-resume"))
        val folder = AppFlows.unique("e2e-resume")
        // The PC's session: `\n` separates the prompt's lines on the control channel (its first
        // line is the session's title; `@text` is the fake agent's answer).
        control.run("native-session $folder $SESSION_TITLE\\n@text $SESSION_ANSWER")

        flows.openFolderAsProject(folder)
        flows.send(FIRST)
        app.waitFor(app.inApp(By.text("echo: $FIRST")), "the first thread's answer", Waits.TURN_MS)

        // /resume: the project's sessions of the thread's harness (the fake CLI).
        flows.resumeFromThePalette()
        app.waitFor(app.inApp(By.text(SESSION_TITLE)), "the PC's session in the list")
        control.screenshot("import-session")
        app.tap(app.inApp(By.text(SESSION_TITLE)), "the PC's session")

        // The imported thread opens with the PC's history and continues from the phone.
        app.waitFor(app.inApp(By.text(SESSION_ANSWER)), "the PC session's answer in the imported thread", Waits.TURN_MS)
        control.screenshot("thread-resumed")
        flows.send(FOLLOW_UP)
        app.waitFor(app.inApp(By.text("echo: $FOLLOW_UP")), "the answer in the resumed session", Waits.TURN_MS)

        // The imported thread replaced the import screen: back returns to the first thread.
        app.tap(app.inApp(By.desc(app.text("back"))), "戻る")
        app.waitFor(app.inApp(By.text("echo: $FIRST")), "the thread /resume came from")

        // Chosen again, the session (imported now) opens the same thread.
        flows.resumeFromThePalette()
        app.waitFor(app.inApp(By.text(app.text("import_done"))), "取り込み済み on the session")
        app.tap(app.inApp(By.text(SESSION_TITLE)), "the imported session")
        app.waitFor(app.inApp(By.text("echo: $FOLLOW_UP")), "the same thread, with the message sent from the phone")
    }

    private companion object {
        const val SESSION_TITLE = "Resume me on the phone"
        const val SESSION_ANSWER = "Started on the PC."
        const val FIRST = "hello from the phone"
        const val FOLLOW_UP = "continue from the phone"
    }
}
