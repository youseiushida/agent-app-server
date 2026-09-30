package dev.aas.android.ui

import android.app.Application
import android.os.Looper
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.verticalScroll
import androidx.compose.runtime.Composable
import androidx.compose.runtime.CompositionLocalProvider
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.setValue
import androidx.compose.ui.Modifier
import androidx.compose.ui.test.assertIsDisplayed
import androidx.compose.ui.test.assertIsNotEnabled
import androidx.compose.ui.test.assertTextEquals
import androidx.compose.ui.test.hasClickAction
import androidx.compose.ui.test.hasContentDescription
import androidx.compose.ui.test.hasTestTag
import androidx.compose.ui.test.hasText
import androidx.compose.ui.test.junit4.v2.createComposeRule
import androidx.compose.ui.test.junit4.v2.createEmptyComposeRule
import androidx.compose.ui.test.onNodeWithContentDescription
import androidx.compose.ui.test.onNodeWithTag
import androidx.compose.ui.test.onNodeWithText
import androidx.compose.ui.test.performClick
import androidx.compose.ui.test.performScrollToNode
import androidx.test.core.app.ActivityScenario
import androidx.test.core.app.ApplicationProvider
import androidx.test.ext.junit.runners.AndroidJUnit4
import dev.aas.android.MainActivity
import dev.aas.android.appContainer
import dev.aas.android.protocol.AasJson
import dev.aas.android.protocol.BackgroundEndReason
import dev.aas.android.protocol.BackgroundTaskKind
import dev.aas.android.protocol.BackgroundTaskStatus
import dev.aas.android.protocol.BackgroundTaskStopParams
import dev.aas.android.protocol.Item
import dev.aas.android.protocol.Methods
import dev.aas.android.protocol.ThreadBackground
import dev.aas.android.protocol.ThreadStatus
import dev.aas.android.protocol.TurnStatus
import dev.aas.android.protocol.WORKSPACE_STREAM
import dev.aas.android.security.PairingInfo
import dev.aas.android.security.PairingState
import dev.aas.android.sync.ItemPosition
import dev.aas.android.sync.Samples
import dev.aas.android.sync.StoredItem
import dev.aas.android.sync.eventually
import dev.aas.android.testing.Fixtures
import dev.aas.android.testing.TestEngine
import dev.aas.android.ui.common.LocalAppContainer
import dev.aas.android.ui.components.ConfirmDialog
import dev.aas.android.ui.interaction.InteractionCard
import dev.aas.android.ui.theme.AasTheme
import dev.aas.android.ui.thread.BackgroundChip
import dev.aas.android.ui.thread.BackgroundStop
import dev.aas.android.ui.thread.BackgroundTags
import dev.aas.android.ui.thread.BackgroundTaskCard
import dev.aas.android.ui.thread.ItemActions
import dev.aas.android.ui.thread.ItemView
import dev.aas.android.ui.thread.RunningBackgroundNote
import dev.aas.android.ui.thread.THREAD_LIST_TAG
import dev.aas.android.ui.thread.TurnStartRow
import kotlinx.coroutines.flow.first
import kotlinx.coroutines.runBlocking
import org.junit.Rule
import org.junit.Test
import org.junit.runner.RunWith
import org.robolectric.Shadows.shadowOf
import org.robolectric.annotation.Config
import kotlin.test.assertEquals

/**
 * The views of background work (Robolectric + Compose), from the golden `thread/read` fixture:
 * a task's card with its progress, result and 停止, the chip of the item that launched it, the
 * divider of a turn the agent started when background work finished, an approval of a background
 * task and the note of the stop dialogs.
 */
@RunWith(AndroidJUnit4::class)
@Config(application = TestAasApplication::class, qualifiers = "w411dp-h891dp-xxhdpi")
class BackgroundViewsTest {
    @get:Rule
    val compose = createComposeRule()

    private val read = Fixtures.threadRead
    private val agent = read.backgroundTasks.single { it.status == BackgroundTaskStatus.Running && it.kind == BackgroundTaskKind.Agent }
    private val build = read.backgroundTasks.single { it.status == BackgroundTaskStatus.Completed }
    private val workflow = read.backgroundTasks.single { it.status == BackgroundTaskStatus.Stopped }

    private fun content(body: @Composable () -> Unit) {
        val container = ApplicationProvider.getApplicationContext<Application>().appContainer
        compose.setContent {
            CompositionLocalProvider(LocalAppContainer provides container) {
                AasTheme(dynamicColor = false) { Column(Modifier.verticalScroll(rememberScrollState())) { body() } }
            }
        }
    }

    @Test
    fun aRunningAgentShowsItsProgressAndStops() {
        val stops = mutableListOf<String>()
        content { BackgroundTaskCard(agent, depth = 0, parentTitle = null, stop = BackgroundStop.Available, onStop = { stops += agent.id }, onOpenOutput = {}) }
        compose.onNodeWithText("Review the reconnect logic").assertIsDisplayed()
        compose.onNode(hasText("エージェント · 実行中 · ", substring = true)).assertIsDisplayed()
        compose.onNodeWithText("最後のツール: Read · ツール 7 回 · 18.4k トークン").assertIsDisplayed()
        compose.onNodeWithContentDescription("「Review the reconnect logic」を停止").performClick()
        assertEquals(listOf(agent.id), stops)
    }

    @Test
    fun aStopWaitingForTheHarnessSaysStoppingAndAnUnconfirmedOneCanBeRepeated() {
        var stop by mutableStateOf(BackgroundStop.Requested)
        content { BackgroundTaskCard(agent.copy(stopRequestedAt = 5), 0, null, stop, onStop = {}, onOpenOutput = {}) }
        compose.onNodeWithText("停止中…").assertIsDisplayed()
        compose.onNodeWithTag(BackgroundTags.stop(agent.id)).assertIsNotEnabled()
        stop = BackgroundStop.Queued
        compose.onNodeWithText("停止の送信待ち").assertIsDisplayed()
        stop = BackgroundStop.Unconfirmed
        compose.onNodeWithText("エージェントが停止を確認していません。もう一度止めることができます").assertIsDisplayed()
        compose.onNodeWithContentDescription("「Review the reconnect logic」を停止").assertIsDisplayed()
    }

    @Test
    fun endedTasksShowTheirResultAndWorkflowAgents() {
        val opened = mutableListOf<String>()
        content {
            BackgroundTaskCard(build, 0, null, BackgroundStop.None, onStop = {}, onOpenOutput = { opened += build.id })
            BackgroundTaskCard(workflow, 0, "Review the reconnect logic", BackgroundStop.None, onStop = {}, onOpenOutput = {})
        }
        compose.onNodeWithText("npm run build").assertIsDisplayed()
        compose.onNode(hasText("シェル · 完了 · ", substring = true)).assertIsDisplayed()
        compose.onNodeWithText("終了コード 0").assertIsDisplayed()
        // Only the beginning is inline (the whole output is a blob): it reads from its start.
        compose.onNodeWithTag(BackgroundTags.output(build.id)).assertTextEquals("[4/4] bundling\nbuilt in 41.2s")
        compose.onNodeWithText("出力が長いため、最初の部分を表示しています").assertIsDisplayed()
        compose.onNode(hasText("出力のファイルが大きいため、最初の ", substring = true) and hasText("は読んでいません", substring = true)).assertIsDisplayed()
        compose.onNodeWithTag(BackgroundTags.showOutput(build.id)).performClick()
        assertEquals(listOf(build.id), opened)
        compose.onNodeWithText("review-and-fix").assertIsDisplayed()
        compose.onNodeWithText("「Review the reconnect logic」から起動").assertIsDisplayed()
        compose.onNodeWithText("Stopped by request").assertIsDisplayed()
        compose.onNodeWithText("Review, then fix what the review finds").assertIsDisplayed()
        compose.onNodeWithText("analyze: review · 完了 · general-purpose · haiku · 15.8k トークン").assertIsDisplayed()
        compose.onNodeWithText("apply: fix · 実行中").assertIsDisplayed()
        compose.onNodeWithText("ツール 12 回 · 31.5k トークン").assertIsDisplayed()
        // No 停止 on ended tasks.
        assertEquals(0, compose.onAllNodes(hasTestTag(BackgroundTags.stop(build.id))).fetchSemanticsNodes().size)
    }

    /**
     * A dev server's output while it runs (`BackgroundTask.output`, streamed by the harness): its
     * newest lines, marked as running, growing with each `backgroundTask/outputDelta`; at the
     * daemon's limit it says the rest comes at the end. 出力の全文を表示 opens the live view.
     */
    @Test
    fun aRunningShellShowsItsNewestOutputAsItGrows() {
        val devServer = read.backgroundTasks.single { it.status == BackgroundTaskStatus.Running && it.kind == BackgroundTaskKind.Shell }
        var task by mutableStateOf(devServer.copy(output = "> vite\n", outputTruncated = false))
        val opened = mutableListOf<String>()
        content { BackgroundTaskCard(task, 0, null, BackgroundStop.Available, onStop = {}, onOpenOutput = { opened += task.id }) }
        compose.onNodeWithText("出力（実行中）").assertIsDisplayed()
        compose.onNodeWithTag(BackgroundTags.output(task.id)).assertTextEquals("> vite")
        // More output: the card follows its end (display.taskOutputLines lines).
        task = task.copy(output = task.output + (1..8).joinToString("") { "hmr update $it\r\n" })
        compose.onNodeWithTag(BackgroundTags.output(task.id)).assertTextEquals((3..8).joinToString("\n") { "hmr update $it" })
        compose.onNodeWithText("表示できる出力の上限に達しました。続きは作業が終わったときに表示します").assertDoesNotExist()
        // The fixture's copy: at the limit.
        task = devServer
        compose.onNodeWithTag(BackgroundTags.output(task.id)).assertTextEquals("> vite\n\n  VITE ready in 412 ms\n  Local: http://localhost:5173/")
        compose.onNodeWithText("表示できる出力の上限に達しました。続きは作業が終わったときに表示します").assertIsDisplayed()
        compose.onNodeWithTag(BackgroundTags.showOutput(task.id)).performClick()
        assertEquals(listOf(devServer.id), opened)
        // Ended with the whole output reported: that replaces the streamed copy.
        task = devServer.copy(
            status = BackgroundTaskStatus.Stopped, endedAt = devServer.startedAt + 1, output = null, outputTruncated = false,
            result = dev.aas.android.protocol.BackgroundResult(output = "> vite\nbye\n"),
        )
        compose.onNodeWithText("出力（実行中）").assertDoesNotExist()
        compose.onNodeWithTag(BackgroundTags.output(task.id)).assertTextEquals("> vite\nbye")
    }

    @Test
    fun aLostTaskSaysSoAndWhy() {
        val lost = agent.copy(status = BackgroundTaskStatus.Lost, endedAt = agent.startedAt + 65_000, endReason = BackgroundEndReason.ProcessExited)
        content { BackgroundTaskCard(lost, 0, null, BackgroundStop.of(lost, harnessCanStop = true, queued = false), onStop = {}, onOpenOutput = {}) }
        compose.onNodeWithText("エージェント · 失われました · 1 分 5 秒").assertIsDisplayed()
        compose.onNodeWithText("エージェントのプロセスが終了したため、結果は分かりません").assertIsDisplayed()
    }

    @Test
    fun aBackgroundedItemCarriesItsTasksChipThatOpensIt() {
        val item = read.items.filterIsInstance<Item.ToolCall>().single { it.backgroundTaskId != null }
        val opened = mutableListOf<String>()
        content { ItemView(item, ItemActions.None.copy(onOpenBackgroundTask = { opened += it }), backgroundTask = agent) }
        compose.onNode(hasText("バックグラウンドで実行中 · ", substring = true)).assertIsDisplayed().performClick()
        assertEquals(listOf(agent.id), opened)
    }

    @Test
    fun theChipFollowsTheTaskAndWithoutItOnlySaysTheWorkWentOn() {
        var task by androidx.compose.runtime.mutableStateOf<dev.aas.android.protocol.BackgroundTask?>(null)
        content { BackgroundChip(agent.id, task, onOpen = {}) }
        compose.onNodeWithText("バックグラウンドで続行").assertIsDisplayed()
        task = agent.copy(status = BackgroundTaskStatus.Failed, endedAt = agent.startedAt + 1)
        compose.onNodeWithText("バックグラウンド: 失敗").assertIsDisplayed()
    }

    @Test
    fun aTurnStartedWhenBackgroundWorkFinishedSaysSo() {
        val triggered = read.turns.single { it.trigger != null }
        content { TurnStartRow(triggered) }
        compose.onNodeWithText("バックグラウンド作業の完了を受けて · ターン 2 · opus").assertIsDisplayed()
    }

    @Test
    fun anApprovalOfABackgroundTaskNamesIt() {
        val asked = read.interactions.single { it.backgroundTaskId != null }
        content {
            InteractionCard(asked, responsePending = false, onRespond = {}, onOpenQuestion = {}, backgroundTaskTitle = agent.title)
            InteractionCard(asked.copy(id = "int_other"), responsePending = false, onRespond = {}, onOpenQuestion = {})
        }
        compose.onNodeWithText("バックグラウンドの作業「Review the reconnect logic」から").assertIsDisplayed()
        compose.onNodeWithText("バックグラウンドの作業から").assertIsDisplayed()
    }

    @Test
    fun theStopDialogNamesTheBackgroundWorkThatStopsToo() {
        val titles = (1..7).map { "task $it" }
        content {
            ConfirmDialog(title = "t", text = "body", confirm = "ok", onConfirm = {}, onDismiss = {}, extra = { RunningBackgroundNote(8, titles) })
        }
        compose.onNodeWithText("8 件のバックグラウンド作業も止まります").assertIsDisplayed()
        compose.onNodeWithText("・task 5").assertIsDisplayed()
        assertEquals(0, compose.onAllNodes(hasText("・task 6")).fetchSemanticsNodes().size, "at most display.dialogTaskTitles")
        compose.onNodeWithText("ほか 3 件").assertIsDisplayed()
    }
}

/**
 * The バックグラウンド section in the real activity with the Room store, offline: the thread list
 * says バックグラウンドで実行中 (1), the thread shows its tasks, 停止 asks first and then waits in the
 * outbox, the chip of the launching item shows its task, and the stop dialog names the work that
 * stops with the process.
 */
@RunWith(AndroidJUnit4::class)
@Config(application = TestAasApplication::class, qualifiers = "w411dp-h891dp-xxhdpi")
class BackgroundScreenTest {
    @get:Rule
    val compose = createEmptyComposeRule()

    private val app get() = ApplicationProvider.getApplicationContext<Application>()
    private val read = Fixtures.threadRead
    private val agent = read.backgroundTasks.single { it.status == BackgroundTaskStatus.Running && it.kind == BackgroundTaskKind.Agent }

    private fun seed() = runBlocking {
        val container = app.appContainer
        container.credentialStore.save(PairingInfo("ws://127.0.0.1:9/v1/ws", "home pc", "dev_1", "Pixel", 1), "token")
        container.pairingState.first { it is PairingState.Paired }
        val thread = read.thread.copy(
            lastTurn = read.thread.lastTurn!!.copy(id = read.turns.last().id, index = 1, status = TurnStatus.Completed),
            status = ThreadStatus.Ready,
            background = ThreadBackground(running = 2),
            head = 5,
        )
        container.syncStore.transaction { tx ->
            tx.setEpoch("e1")
            tx.setCursor(WORKSPACE_STREAM, 1)
            tx.replaceHarnesses(listOf(TestEngine.fakeHarness(background = true).copy(id = "claude", displayName = "Claude")))
            tx.upsertProject(Samples.project(read.thread.projectId, name = "agent-app-server"))
            tx.upsertThread(thread)
            read.turns.forEach { tx.upsertTurn(it) }
            read.items.forEachIndexed { i, item -> tx.upsertItem(StoredItem(item, ItemPosition(0, i.toLong()))) }
            // No pending approval: the thread's status is its background work.
            read.backgroundTasks.forEach { tx.upsertBackgroundTask(it) }
        }
        container.engine.start()
        eventually(what = "the workspace") { container.engine.workspace.value.takeIf { it.synced && it.threads.size == 1 } }
    }

    private fun idle() {
        shadowOf(Looper.getMainLooper()).idle()
        compose.waitForIdle()
    }

    private fun waitFor(text: String, substring: Boolean = false) {
        compose.waitUntil(WAIT_MS) {
            idle()
            compose.onAllNodes(hasText(text, substring = substring)).fetchSemanticsNodes().isNotEmpty()
        }
    }

    @Test
    fun theSectionListsTheTasksAndStopWaitsInTheOutbox() {
        seed()
        ActivityScenario.launch(MainActivity::class.java).use {
            waitFor("agent-app-server")
            compose.onNodeWithText("agent-app-server").performClick()
            waitFor("バックグラウンドで実行中 (2)")
            compose.onNodeWithText("Fix the flaky reconnect test").performClick()
            waitFor("Review the reconnect logic")
            compose.onNodeWithTag(BackgroundTags.HEADER).assertIsDisplayed()
            compose.onNodeWithText("実行中 2 · 終了 2").assertIsDisplayed()
            compose.onNodeWithText("終了した作業 (2)").assertIsDisplayed()
            // Ended tasks open on demand.
            compose.onNodeWithTag(BackgroundTags.ENDED).performClick()
            waitFor("npm run build")

            // 停止 asks first, then the request waits in the outbox and the card says so.
            compose.onNodeWithTag(THREAD_LIST_TAG).performScrollToNode(hasContentDescription("「Review the reconnect logic」を停止"))
            compose.onNodeWithContentDescription("「Review the reconnect logic」を停止").performClick()
            waitFor("バックグラウンドの作業を止めますか？")
            // The dialog's 停止 (not the 停止 of a card).
            val cardStops = read.backgroundTasks.map { !hasTestTag(BackgroundTags.stop(it.id)) }.reduce { a, b -> a and b }
            compose.onNode(hasText("停止") and hasClickAction() and cardStops).performClick()
            compose.waitUntil(WAIT_MS) {
                idle()
                app.appContainer.engine.outbox.value.any { it.method == Methods.BackgroundTaskStop.name }
            }
            val entry = app.appContainer.engine.outbox.value.first { it.method == Methods.BackgroundTaskStop.name }
            val params = AasJson.decodeFromJsonElement(BackgroundTaskStopParams.serializer(), entry.params)
            assertEquals(read.thread.id to agent.id, params.threadId to params.taskId)
            waitFor("停止の送信待ち")
        }
    }

    @Test
    fun theLaunchingItemsChipShowsItsTaskAndTheStopDialogNamesTheWork() {
        seed()
        ActivityScenario.launch(MainActivity::class.java).use {
            waitFor("agent-app-server")
            compose.onNodeWithText("agent-app-server").performClick()
            waitFor("Fix the flaky reconnect test")
            compose.onNodeWithText("Fix the flaky reconnect test").performClick()
            waitFor("Review the reconnect logic")
            // Fold the section, then open the task from the item that launched it.
            compose.onNodeWithTag(BackgroundTags.HEADER).performClick()
            compose.waitUntil(WAIT_MS) {
                idle()
                compose.onAllNodes(hasTestTag(BackgroundTags.task(agent.id))).fetchSemanticsNodes().isEmpty()
            }
            compose.onNodeWithTag(THREAD_LIST_TAG).performScrollToNode(hasTestTag(BackgroundTags.chip(agent.id)))
            compose.onNodeWithTag(BackgroundTags.chip(agent.id)).performClick()
            compose.waitUntil(WAIT_MS) {
                idle()
                compose.onAllNodes(hasTestTag(BackgroundTags.task(agent.id))).fetchSemanticsNodes().isNotEmpty()
            }
            compose.onNodeWithTag(BackgroundTags.task(agent.id)).assertIsDisplayed()

            // The process stop names the background work that stops with it.
            compose.onNodeWithContentDescription("その他の操作").performClick()
            idle()
            compose.onNodeWithText("プロセスを停止").performClick()
            waitFor("2 件のバックグラウンド作業も止まります")
            compose.onNodeWithText("・Review the reconnect logic").assertIsDisplayed()
            compose.onNodeWithText("・npm run dev").assertIsDisplayed()
        }
    }

    private companion object {
        const val WAIT_MS = 10_000L
    }
}
