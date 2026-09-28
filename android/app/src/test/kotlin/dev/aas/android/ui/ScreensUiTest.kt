package dev.aas.android.ui

import android.app.Application
import android.content.Intent
import android.os.Looper
import androidx.compose.ui.test.assertIsDisplayed
import androidx.compose.ui.test.hasClickAction
import androidx.compose.ui.test.hasContentDescription
import androidx.compose.ui.test.hasTestTag
import androidx.compose.ui.test.hasText
import androidx.compose.ui.test.junit4.v2.createEmptyComposeRule
import androidx.compose.ui.test.longClick
import androidx.compose.ui.test.onNodeWithTag
import androidx.compose.ui.test.onNodeWithText
import androidx.compose.ui.test.performClick
import androidx.compose.ui.test.performTextInput
import androidx.compose.ui.test.performTouchInput
import androidx.test.core.app.ActivityScenario
import androidx.test.core.app.ApplicationProvider
import androidx.test.ext.junit.runners.AndroidJUnit4
import android.os.Bundle
import dev.aas.android.MainActivity
import dev.aas.android.notify.Notifier
import dev.aas.android.appContainer
import dev.aas.android.protocol.AasJson
import dev.aas.android.protocol.InteractionRequest
import dev.aas.android.protocol.Methods
import dev.aas.android.protocol.ThreadUpdateParams
import dev.aas.android.protocol.TurnStartParams
import dev.aas.android.protocol.TurnStatus
import dev.aas.android.protocol.WORKSPACE_STREAM
import dev.aas.android.security.PairingInfo
import dev.aas.android.security.PairingState
import dev.aas.android.sync.ItemPosition
import dev.aas.android.sync.Samples
import dev.aas.android.sync.StoredItem
import dev.aas.android.sync.ThreadViewState
import dev.aas.android.sync.eventually
import dev.aas.android.testing.Fixtures
import dev.aas.android.testing.TestEngine
import dev.aas.android.ui.composer.ComposerTags
import dev.aas.android.ui.navigation.DeepLinks
import dev.aas.android.ui.projects.threadRowTag
import kotlinx.coroutines.flow.first
import kotlinx.coroutines.runBlocking
import org.junit.Rule
import org.junit.Test
import org.junit.runner.RunWith
import org.robolectric.Robolectric
import org.robolectric.Shadows.shadowOf
import org.robolectric.annotation.Config
import kotlin.test.assertEquals

/**
 * The screens in the real activity and navigation graph with the Room store (Robolectric +
 * Compose), paired with a daemon that cannot be reached: everything stored is browsable offline,
 * and what the user does waits in the outbox.
 */
@RunWith(AndroidJUnit4::class)
@Config(application = TestAasApplication::class, qualifiers = "w411dp-h891dp-xxhdpi")
class ScreensUiTest {
    @get:Rule
    val compose = createEmptyComposeRule()

    private val app get() = ApplicationProvider.getApplicationContext<Application>()
    private val read = Fixtures.threadRead
    private val project = Samples.project(read.thread.projectId, name = "agent-app-server")

    /** A paired device with the fixture thread and two more threads of its project, stored locally. */
    private fun seed() = runBlocking {
        val container = app.appContainer
        container.credentialStore.save(PairingInfo("ws://127.0.0.1:9/v1/ws", "home pc", "dev_1", "Pixel", 1), "token")
        container.pairingState.first { it is PairingState.Paired }
        // The fixture's turn is finished (its summary still says running): an idle thread.
        val thread = read.thread.copy(lastTurn = read.thread.lastTurn!!.copy(status = TurnStatus.Completed), status = dev.aas.android.protocol.ThreadStatus.Idle, head = 5)
        container.syncStore.transaction { tx ->
            tx.setEpoch("e1")
            tx.setCursor(WORKSPACE_STREAM, 1)
            tx.replaceHarnesses(listOf(TestEngine.fakeHarness().copy(id = "claude", displayName = "Claude")))
            tx.upsertProject(project)
            tx.upsertThread(thread)
            tx.upsertThread(Samples.thread("thr_old", title = "Older thread", lastActivityAt = 1_000, projectId = project.id).copy(harnessId = "claude"))
            tx.upsertThread(Samples.thread("thr_new", title = "Newest thread", lastActivityAt = 9_999_999_999_999, projectId = project.id, head = 3).copy(harnessId = "claude"))
            // The newest thread was read up to its head; the older one never.
            tx.setViewState("thr_new", ThreadViewState(lastViewedHead = 3))
            read.turns.forEach { tx.upsertTurn(it) }
            read.items.forEachIndexed { i, item -> tx.upsertItem(StoredItem(item, ItemPosition(0, i.toLong()))) }
            read.interactions.forEach { tx.upsertInteraction(it) }
            tx.replaceQueued(thread.id, read.queued)
        }
        container.engine.start()
        eventually(what = "the workspace") { container.engine.workspace.value.takeIf { it.synced && it.threads.size == 3 } }
    }

    private fun idle() {
        shadowOf(Looper.getMainLooper()).idle()
        compose.waitForIdle()
    }

    /** Waits (running the main looper, where view models resume) for an outbox entry of [method]. */
    private fun awaitOutbox(method: String): dev.aas.android.sync.OutboxEntry {
        compose.waitUntil(WAIT_MS) {
            idle()
            app.appContainer.engine.outbox.value.any { it.method == method }
        }
        return app.appContainer.engine.outbox.value.first { it.method == method }
    }

    private fun waitFor(text: String, substring: Boolean = false) {
        compose.waitUntil(WAIT_MS) {
            idle()
            compose.onAllNodes(hasText(text, substring = substring)).fetchSemanticsNodes().isNotEmpty()
        }
    }

    @Test
    fun projectsThenThreadsPinnedFirstWithUnreadAndLongPressPin() {
        seed()
        ActivityScenario.launch(MainActivity::class.java).use {
            waitFor("agent-app-server")
            compose.onNodeWithText("agent-app-server").performClick()
            waitFor("Fix the flaky reconnect test")
            // Pinned first, then by activity.
            val tops = listOf(read.thread.id, "thr_new", "thr_old").map { id ->
                compose.onNodeWithTag(threadRowTag(id)).fetchSemanticsNode().boundsInRoot.top
            }
            assertEquals(tops.sorted(), tops)
            // Unread: the older thread was never read; the newest was read up to its head.
            assertEquals(1, compose.onAllNodes(hasTestTag(threadRowTag("thr_old")) and hasContentDescription("未読")).fetchSemanticsNodes().size)
            assertEquals(0, compose.onAllNodes(hasTestTag(threadRowTag("thr_new")) and hasContentDescription("未読")).fetchSemanticsNodes().size)

            compose.onNodeWithTag(threadRowTag("thr_old")).performTouchInput { longClick() }
            idle()
            compose.onNodeWithText("ピン留め").performClick()
            val entry = awaitOutbox(Methods.ThreadUpdate.name)
            val params = AasJson.decodeFromJsonElement(ThreadUpdateParams.serializer(), entry.params)
            assertEquals("thr_old", params.threadId)
            assertEquals(true, params.pinned)
        }
    }

    @Test
    fun theThreadRendersOfflineAndAMessageWaitsInTheOutbox() {
        seed()
        ActivityScenario.launch(MainActivity::class.java).use {
            waitFor("agent-app-server")
            compose.onNodeWithText("agent-app-server").performClick()
            waitFor("Fix the flaky reconnect test")
            compose.onNodeWithText("Fix the flaky reconnect test").performClick()
            // The newest rows are at the bottom: the question asked last and the turn's end.
            waitFor("Which database?")
            // The stored conversation, the queue and the banner of the pending approval.
            compose.onNodeWithText("オフラインです。端末に保存された内容を表示しています。送信した内容は接続したら送られます。").assertIsDisplayed()
            compose.onNodeWithText("送信待ちのメッセージ（1）").assertIsDisplayed()
            compose.onNodeWithText("承認が必要です").assertIsDisplayed()
            // The thread's model is not in the harness's list: its id is shown as it is.
            compose.onNodeWithText("Claude · opus · High").assertIsDisplayed()

            compose.onNodeWithTag(ComposerTags.INPUT).performTextInput("Please also add a test")
            idle()
            compose.onNodeWithTag(ComposerTags.SEND).performClick()
            val entry = awaitOutbox(Methods.TurnStart.name)
            val params = AasJson.decodeFromJsonElement(TurnStartParams.serializer(), entry.params)
            assertEquals(read.thread.id, params.threadId)
            assertEquals(listOf(dev.aas.android.protocol.InputPart.Text("Please also add a test")), params.input)
            // The message shows where it will appear, marked as waiting for the connection.
            waitFor("送信待ち（接続したら送ります）")
            compose.onNodeWithText("Please also add a test").assertIsDisplayed()
        }
    }

    @Test
    fun aNotificationForAQuestionOpensItsSheet() {
        seed()
        val question = read.interactions.first { it.request is InteractionRequest.Question }
        val intent = Intent(app, MainActivity::class.java).setData(DeepLinks.thread(read.thread.id, question.id))
        ActivityScenario.launch<MainActivity>(intent).use {
            waitFor("Which database should the migration target?")
            compose.onNodeWithText("PostgreSQL").assertIsDisplayed()
            compose.onNodeWithText("Production").assertIsDisplayed()
        }
    }

    @Test
    fun aNotificationTapThatReachesARecreatedActivityBeforeItsFirstFrameIsHandled() {
        seed()
        val question = read.interactions.first { it.request is InteractionRequest.Question }
        // The activity is recreated from its saved state (a configuration change in the
        // background, or the process coming back from recents)...
        val saved = Bundle()
        val first = Robolectric.buildActivity(MainActivity::class.java).setup()
        waitFor("agent-app-server")
        first.saveInstanceState(saved).pause().stop().destroy()
        val second = Robolectric.buildActivity(MainActivity::class.java).create(saved).start()
        // ...and the tap arrives through onNewIntent between onStart and onResume, before
        // anything is composed.
        second.newIntent(Notifier.threadIntent(app, DeepLinks.thread(read.thread.id, question.id)))
        second.postCreate(saved).resume().visible()
        waitFor("Which database should the migration target?")
        compose.onNodeWithText("PostgreSQL").assertIsDisplayed()
        second.pause().stop().destroy()
    }

    @Test
    fun theNewProjectFlowStartsFromTheProjectList() {
        seed()
        ActivityScenario.launch(MainActivity::class.java).use {
            waitFor("agent-app-server")
            compose.onNodeWithText("新しいプロジェクト", useUnmergedTree = true).performClick()
            waitFor("既存のフォルダーを使用")
            compose.onNodeWithText("最初から始める").assertIsDisplayed()
            compose.onNodeWithText("既存のフォルダーを使用").performClick()
            // The daemon is not reachable: the folder list says why and offers a retry.
            waitFor("フォルダーの一覧は、サーバに接続しているときだけ表示できます")
            compose.onNodeWithText("再試行").assertIsDisplayed()
        }
    }

    @Test
    fun theFloatingButtonsAndTheInboxBadgeHaveAccessibleNames() {
        seed()
        ActivityScenario.launch(MainActivity::class.java).use {
            waitFor("agent-app-server")
            // Material 3 clears the semantics of an extended FAB's label and of a labelled tab's
            // icon (with its badge): without their own descriptions, TalkBack and UI Automator
            // found an unnamed button and no count (found on the emulator).
            compose.onNode(hasClickAction() and hasContentDescription("新しいプロジェクト")).assertIsDisplayed()
            compose.onNode(hasClickAction() and hasContentDescription("要対応（", substring = true)).assertIsDisplayed()
            compose.onNodeWithText("agent-app-server").performClick()
            waitFor("Fix the flaky reconnect test")
            compose.onNode(hasClickAction() and hasContentDescription("新しいスレッド")).assertIsDisplayed()
        }
    }

    private companion object {
        const val WAIT_MS = 10_000L
    }
}
