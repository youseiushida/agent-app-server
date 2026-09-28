package dev.aas.android.ui

import android.app.Application
import androidx.compose.runtime.Composable
import androidx.compose.runtime.CompositionLocalProvider
import androidx.compose.ui.test.assertIsDisplayed
import androidx.compose.ui.test.hasContentDescription
import androidx.compose.ui.test.hasText
import androidx.compose.ui.test.junit4.v2.createComposeRule
import androidx.compose.ui.test.onAllNodesWithText
import androidx.compose.ui.test.onNodeWithText
import androidx.compose.ui.test.performClick
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.verticalScroll
import androidx.compose.ui.Modifier
import androidx.test.core.app.ApplicationProvider
import androidx.test.ext.junit.runners.AndroidJUnit4
import dev.aas.android.appContainer
import dev.aas.android.domain.diff.UnifiedDiff
import dev.aas.android.protocol.Item
import dev.aas.android.protocol.ThreadDiffResult
import dev.aas.android.protocol.DiffFile
import dev.aas.android.protocol.DiffSummary
import dev.aas.android.protocol.FileChangeKind
import dev.aas.android.testing.Fixtures
import dev.aas.android.testing.item
import dev.aas.android.ui.common.LocalAppContainer
import dev.aas.android.ui.components.MarkdownText
import dev.aas.android.ui.diff.DiffViewModel
import dev.aas.android.ui.diff.LoadedDiffView
import dev.aas.android.ui.theme.AasTheme
import dev.aas.android.ui.thread.ItemActions
import dev.aas.android.ui.thread.ItemView
import dev.aas.android.DiffViewPolicy
import kotlinx.serialization.json.JsonPrimitive
import kotlinx.serialization.json.buildJsonObject
import kotlinx.serialization.json.put
import org.junit.Rule
import org.junit.Test
import org.junit.runner.RunWith
import org.robolectric.annotation.Config
import kotlin.test.assertEquals

/**
 * Every item kind of the protocol, rendered from the golden `thread/read` fixture (Robolectric +
 * Compose): what is visible at once, what opens on tap.
 */
@RunWith(AndroidJUnit4::class)
@Config(application = TestAasApplication::class, qualifiers = "w411dp-h891dp-xxhdpi")
class ItemRenderingTest {
    @get:Rule
    val compose = createComposeRule()

    private val read = Fixtures.threadRead

    private fun show(item: Item, actions: ItemActions = ItemActions.None) = content { ItemView(item, actions) }

    /** [body] in the app's theme; [scroll]: in a scrolling column (not for content that is a lazy list itself). */
    private fun content(scroll: Boolean = true, body: @Composable () -> Unit) {
        val container = ApplicationProvider.getApplicationContext<Application>().appContainer
        compose.setContent {
            CompositionLocalProvider(LocalAppContainer provides container) {
                AasTheme(dynamicColor = false) {
                    if (scroll) Column(Modifier.verticalScroll(rememberScrollState())) { body() } else body()
                }
            }
        }
    }

    @Test
    fun theUsersMessageWithMentionsAndImages() {
        val item = read.item<Item.UserMessage>()
        val opened = mutableListOf<String>()
        show(item, ItemActions.None.copy(onOpenImage = { opened += it }))
        compose.onNodeWithText(item.text).assertIsDisplayed()
        compose.onNodeWithText("@crates/aas-server/tests/reconnect.rs").assertIsDisplayed()
        // The image loads through the blob cache; without a pairing it shows the retry icon.
        compose.waitUntil(WAIT_MS) { compose.onAllNodes(hasContentDescription("画像をもう一度読み込む", substring = true)).fetchSemanticsNodes().isNotEmpty() }
    }

    @Test
    fun reasoningIsFoldedUntilOpened() {
        val item = read.item<Item.Reasoning>()
        show(item)
        compose.onNodeWithText("思考時間", substring = true).assertIsDisplayed()
        assertEquals(0, compose.onAllNodesWithText(item.text).fetchSemanticsNodes().size)
        compose.onNodeWithText("思考時間", substring = true).performClick()
        compose.onNodeWithText(item.text).assertIsDisplayed()
    }

    @Test
    fun aCommandCardShowsItsStatusAndOutputWhenOpened() {
        val item = read.item<Item.CommandExecution>()
        show(item)
        compose.onNodeWithText("$ ${item.command}").assertIsDisplayed()
        compose.onNodeWithText("実行済み · 5 秒").assertIsDisplayed()
        compose.onNodeWithText("$ ${item.command}").performClick()
        compose.onNodeWithText("running 3 tests\ntest ok").assertIsDisplayed()
        compose.onNodeWithText("場所: ${item.cwd}").assertIsDisplayed()
    }

    @Test
    fun aRunningCommandStreamsItsLastLinesAndAFailureIsRed() {
        val base = read.item<Item.CommandExecution>()
        val running = base.copy(status = dev.aas.android.protocol.ItemStatus.InProgress, output = (1..10).joinToString("\n") { "line $it" }, exitCode = null, completedAt = null, durationMs = null)
        content {
            ItemView(running, ItemActions.None)
            ItemView(base.copy(id = "failed", exitCode = 101, output = ""), ItemActions.None)
        }
        compose.onNodeWithText("line 7\nline 8\nline 9\nline 10").assertIsDisplayed()
        compose.onNodeWithText("実行中").assertIsDisplayed()
        compose.onNodeWithText("終了コード 101", substring = true).assertIsDisplayed()
    }

    @Test
    fun fileChangesOpenTheirDiffAndLinkToTheTurnDiff() {
        val item = read.item<Item.FileChangeItem>()
        val opened = mutableListOf<String>()
        show(item, ItemActions.None.copy(onOpenTurnDiff = { opened += it }))
        compose.onNodeWithText("1 個のファイルを変更").assertIsDisplayed()
        compose.onNodeWithText(item.changes.single().path).assertIsDisplayed()
        compose.onNodeWithText(item.changes.single().path).performClick()
        compose.onNodeWithText("let t = 2;").assertIsDisplayed()
        compose.onNodeWithText("ターンの差分を見る").performClick()
        assertEquals(listOf(item.turnId), opened)
    }

    @Test
    fun toolCallsShowInputAndOutputWhenOpened() {
        val item = read.item<Item.ToolCall>()
        show(item)
        compose.onNodeWithText(item.title).assertIsDisplayed()
        compose.onNodeWithText("MCP · docs").assertIsDisplayed()
        compose.onNodeWithText(item.title).performClick()
        compose.onNodeWithText("\"query\": \"tokio watch\"", substring = true).assertIsDisplayed()
        compose.onNodeWithText("3 results").assertIsDisplayed()
    }

    @Test
    fun planAgentMessageNoticeAndUnknownKinds() {
        val unknown = Item.Unknown(
            "imageGeneration",
            buildJsonObject {
                put("id", "itm_x")
                put("threadId", read.thread.id)
                put("turnId", "trn_x")
                put("status", "completed")
                put("startedAt", JsonPrimitive(1))
            },
        )
        content {
            ItemView(read.item<Item.Plan>(), ItemActions.None)
            ItemView(read.item<Item.AgentMessage>(), ItemActions.None)
            ItemView(read.item<Item.Notice>(), ItemActions.None)
            ItemView(unknown, ItemActions.None)
        }
        compose.onNodeWithText("プラン").assertIsDisplayed()
        compose.onNodeWithText("1 / 3 完了").assertIsDisplayed()
        listOf("Reproduce", "Fix", "Verify").forEach { compose.onNodeWithText(it).assertIsDisplayed() }
        compose.onNodeWithText("I found the race: the heartbeat").assertIsDisplayed()
        compose.onNodeWithText("Context compacted").assertIsDisplayed()
        compose.onNodeWithText("compacted").assertIsDisplayed()
        compose.onNodeWithText("このアプリが対応していない項目（imageGeneration）").assertIsDisplayed()
    }

    @Test
    fun markdownRendersCodeBlocksListsAndTables() {
        content {
            MarkdownText("## Plan\n\n1. **first** step\n2. `second`\n\n```kotlin\nval x = 1\n```\n\n| a | b |\n|---|---|\n| 1 | 2 |\n")
        }
        compose.onNodeWithText("Plan").assertIsDisplayed()
        compose.onNodeWithText("first step").assertIsDisplayed()
        compose.onNodeWithText("val x = 1").assertIsDisplayed()
        compose.onNodeWithText("kotlin").assertIsDisplayed()
        listOf("a", "b", "1", "2").forEach { compose.onNode(hasText(it)).assertIsDisplayed() }
    }

    @Test
    fun theDiffViewerDrawsFilesHunksAndLineNumbers() {
        val patch = "diff --git a/src/main.rs b/src/main.rs\n--- a/src/main.rs\n+++ b/src/main.rs\n@@ -1,2 +1,2 @@\n keep\n-old line\n+new line\n" +
            "diff --git a/logo.png b/logo.png\nBinary files a/logo.png and b/logo.png differ\n"
        val result = ThreadDiffResult(
            DiffSummary(2, 1, 1),
            listOf(DiffFile("src/main.rs", FileChangeKind.Update, 1, 1, false), DiffFile("logo.png", FileChangeKind.Update, 0, 0, true)),
            patch = patch,
        )
        val diff = DiffViewModel.build(result, UnifiedDiff.parse(patch), null, DiffViewPolicy())
        val comments = mutableListOf<String>()
        content(scroll = false) { LoadedDiffView(diff, wrap = true, onSelect = {}, onComment = { file, line -> comments += "${file.path}:${line.newNumber}" }) }
        compose.onNodeWithText("2 個のファイル").assertIsDisplayed()
        compose.onNodeWithText("src/main.rs").assertIsDisplayed()
        compose.onNodeWithText("@@ -1,2 +1,2 @@").assertIsDisplayed()
        compose.onNodeWithText("new line").assertIsDisplayed()
        compose.onNodeWithText("old line").assertIsDisplayed()
        compose.onNodeWithText("バイナリファイルのため、差分は表示できません").assertIsDisplayed()
        // Line numbers of both sides.
        assertEquals(2, compose.onAllNodesWithText("2").fetchSemanticsNodes().size)
    }

    private companion object {
        const val WAIT_MS = 5_000L
    }
}
