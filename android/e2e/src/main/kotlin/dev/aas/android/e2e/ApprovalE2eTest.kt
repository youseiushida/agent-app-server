package dev.aas.android.e2e

import androidx.test.ext.junit.runners.AndroidJUnit4
import androidx.test.uiautomator.By
import org.junit.Test
import org.junit.runner.RunWith

/**
 * Approvals from the real daemon: the notification's 許可 in the system's notification shade, and
 * the 要対応 tab. Both are checked through the daemon's answer: the command runs and the turn ends
 * with the text after the approval.
 */
@RunWith(AndroidJUnit4::class)
class ApprovalE2eTest : E2eTest() {
    @Test
    fun anApprovalNotificationIsAllowedFromTheShade() {
        app.grantNotifications()
        flows.pairFresh(AppFlows.unique("e2e-shade"))
        val prompt = "@approve echo from-the-shade\n@text approved from the shade"
        flows.newProjectAndThread(AppFlows.unique("e2e-shade"), prompt)
        app.waitFor(app.inApp(By.text(APPROVAL_TITLE)), "the approval card", Waits.TURN_MS)

        // In the background, the notification's button answers.
        app.goHome()
        flows.tapNotificationAction(
            title = app.text("notify_approval_title", prompt.lines().first()),
            action = app.text("action_allow_once"),
            shot = "notification-approval",
        )
        app.goHome()

        // The daemon ran the command and finished the turn; the app shows its record.
        app.launch()
        app.waitFor(app.inApp(By.text("approved from the shade")), "the text after the approval", Waits.TURN_MS)
        app.waitFor(app.inApp(By.text(app.text("record_approval", APPROVAL_TITLE, app.text("approval_allow_once")))), "the approval's record")
    }

    @Test
    fun theInboxTabShowsAndAnswersAPendingApproval() {
        flows.pairFresh(AppFlows.unique("e2e-inbox"))
        val prompt = "@approve echo from-the-inbox\n@text approved from the inbox"
        flows.newProjectAndThread(AppFlows.unique("e2e-inbox"), prompt)
        app.waitFor(app.inApp(By.text(APPROVAL_TITLE)), "the approval card", Waits.TURN_MS)
        app.waitFor(app.inApp(By.text(app.text("banner_approval"))), "the approval banner above the composer")
        control.screenshot("approval-card")

        // Back to the tabs: the 要対応 tab carries the count.
        app.device.pressBack()
        app.waitFor(app.inApp(By.desc(app.text("thread_new"))), "the thread list")
        app.device.pressBack()
        flows.waitForProjects()
        flows.openInbox()
        app.waitFor(app.inApp(By.text("$ echo from-the-inbox")), "the approval in the inbox")
        control.screenshot("inbox")

        // Answering in the inbox: the approval leaves 回答待ち and the turn finishes.
        app.tapNear(app.inApp(By.text("$ echo from-the-inbox")), app.button(app.text("approval_allow_once")), "許可 on the inbox card")
        app.waitGone(app.inApp(By.text("$ echo from-the-inbox")), "the answered approval", Waits.TURN_MS)
        app.tap(app.inApp(By.text(prompt.lines().first())), "the thread in the inbox")
        app.waitFor(app.inApp(By.text("approved from the inbox")), "the text after the approval", Waits.TURN_MS)
    }

    private companion object {
        /** The fake agent's approval request for a command. */
        const val APPROVAL_TITLE = "Run command?"
    }
}
