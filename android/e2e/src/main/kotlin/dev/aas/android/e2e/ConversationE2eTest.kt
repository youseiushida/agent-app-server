package dev.aas.android.e2e

import androidx.test.ext.junit.runners.AndroidJUnit4
import androidx.test.uiautomator.By
import org.junit.Assert.assertEquals
import org.junit.Test
import org.junit.runner.RunWith

/**
 * The main path against the real daemon and the fake harness: a new project under the root, a
 * thread, streamed output with a command and a file change, the diff, the `/` palette and `@`
 * mentions.
 */
@RunWith(AndroidJUnit4::class)
class ConversationE2eTest : E2eTest() {
    @Test
    fun aNewProjectAndThreadStreamTheAgentsWorkAndItsDiff() {
        flows.pairFresh(AppFlows.unique("e2e-conversation"))
        control.screenshot("projects")
        val project = AppFlows.unique("e2e-project")
        flows.createProject(project, shots = "new-project")
        control.screenshot("new-thread")
        flows.send(SCENARIO)

        // Streamed output, the command and the file change, then the turn's end.
        flows.waitForStream(STREAM_COUNT)
        app.waitFor(app.inApp(By.text(app.textPattern("turn_worked"))), "the end of the turn")
        control.screenshot("thread-streamed")
        // The activity group (reasoning, command, file change) is folded once the turn ended: open it.
        app.tap(app.inApp(By.text(app.text("group_title", 3))), "3 件の操作")
        app.waitFor(app.inApp(By.text("$ $COMMAND")), "the command card")
        // "実行済み · 0 秒": the state, then the duration.
        app.waitFor(app.inApp(By.textStartsWith(app.text("item_command_done"))), "the command's state")
        app.waitFor(app.inApp(By.text(app.text("item_files_changed", 1))), "the file change")
        app.waitFor(app.inApp(By.text(FILE)), "the changed file")
        control.screenshot("thread-command-and-diff")

        // The turn's diff from git on the PC.
        app.tap(app.inApp(app.button(app.text("item_view_turn_diff"))), "ターンの差分を見る")
        app.waitFor(app.inApp(By.text(app.text("diff_title"))), "the diff screen")
        app.waitFor(app.inApp(By.text(FILE_CONTENT)), "the added line in the diff")
        control.screenshot("diff")

        // Back to the project's thread list and the project list.
        app.device.pressBack()
        app.waitFor(app.inApp(By.text(app.text("item_view_turn_diff"))), "the thread again")
        app.device.pressBack()
        app.waitFor(app.inApp(By.desc(app.text("thread_new"))), "the thread list")
        app.waitFor(app.inApp(By.text(SCENARIO.lines().first())), "the thread in its project's list")
        control.screenshot("thread-list")
        app.device.pressBack()
        flows.waitForProjects()
        app.waitFor(app.inApp(By.text(project)), "the project in the list")
    }

    @Test
    fun theSlashPaletteAndMentionsUseTheDaemon() {
        flows.pairFresh(AppFlows.unique("e2e-palette"))
        flows.newProjectAndThread(AppFlows.unique("e2e-palette"), "@write $FILE $FILE_CONTENT\n@text wrote it")
        app.waitFor(app.inApp(By.text("wrote it")), "the first turn's answer", Waits.TURN_MS)

        // `/`: the daemon's commands (command/list) first, in view, then the app's own.
        app.setText(flows.composer, "/", "the composer")
        app.waitFor(app.inApp(By.text("/model")), "the daemon's /model at the top of the palette")
        app.waitFor(app.inApp(By.text(app.text("command_model"))), "its description")
        control.screenshot("composer-palette")
        app.setText(flows.composer, "/sta", "the composer")
        app.tap(app.inApp(By.text("/status")), "/status")
        app.waitFor(app.inApp(By.text(app.text("status_thread_id"))), "the status sheet")
        app.device.pressBack()
        app.waitGone(app.inApp(By.text(app.text("status_thread_id"))), "the status sheet")

        // `@`: fs/search on the PC, the chosen path inserted and sent as a mention.
        app.setText(flows.composer, "see @not", "the composer")
        // The result row in the mention popup (the thread shows notes.txt in its file change too):
        // one node with the file icon's description and the path when Compose merged the row.
        val fileIcon = app.text("mention_file")
        app.tapFirst(
            listOf(
                app.inApp(By.desc(fileIcon).text(FILE)),
                app.inApp(By.clickable(true).hasDescendant(By.desc(fileIcon)).hasDescendant(By.text(FILE))),
            ),
            "the search result $FILE",
        )
        app.waitUntil("the mention in the composer") { app.find(flows.composer)?.text == "see @$FILE " }
        app.tap(app.inApp(By.desc(app.text("composer_send"))), "送信")
        app.waitFor(app.inApp(By.text("echo: see @$FILE")), "the agent's answer", Waits.TURN_MS)
        val chip = app.waitFor(app.inApp(By.text("@$FILE")), "the mention chip on the message")
        assertEquals("@$FILE", chip.text)
    }

    private companion object {
        const val COMMAND = "cargo --version"
        const val FILE = "notes.txt"
        const val FILE_CONTENT = "hello from the agent"
        const val STREAM_COUNT = 60
        val SCENARIO = "@reason Planning the change.\n@exec $COMMAND\n@write $FILE $FILE_CONTENT\n@stream $STREAM_COUNT 30"
    }
}
