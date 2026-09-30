package dev.aas.android.ui

import android.app.Application
import android.os.Looper
import androidx.compose.runtime.CompositionLocalProvider
import androidx.compose.ui.test.assertIsDisplayed
import androidx.compose.ui.test.hasContentDescription
import androidx.compose.ui.test.hasText
import androidx.compose.ui.test.junit4.v2.createComposeRule
import androidx.compose.ui.test.onNodeWithContentDescription
import androidx.compose.ui.test.onNodeWithTag
import androidx.compose.ui.test.onNodeWithText
import androidx.compose.ui.test.performClick
import androidx.compose.ui.test.performScrollToIndex
import androidx.compose.ui.test.performTouchInput
import androidx.compose.ui.test.onRoot
import androidx.compose.ui.test.swipe
import androidx.compose.ui.geometry.Offset
import androidx.navigation.compose.rememberNavController
import androidx.test.core.app.ApplicationProvider
import androidx.test.ext.junit.runners.AndroidJUnit4
import dev.aas.android.AppPolicy
import dev.aas.android.appContainer
import dev.aas.android.data.BlobCache
import dev.aas.android.data.BlobRepository
import dev.aas.android.data.ThreadRepository
import dev.aas.android.protocol.BackgroundTaskKind
import dev.aas.android.protocol.BackgroundTaskStatus
import dev.aas.android.protocol.Event
import dev.aas.android.protocol.threadStream
import dev.aas.android.sync.Samples
import dev.aas.android.testing.Fixtures
import dev.aas.android.testing.TestEngine
import dev.aas.android.ui.common.LocalAppContainer
import dev.aas.android.ui.navigation.AppNavigator
import dev.aas.android.ui.theme.AasTheme
import dev.aas.android.ui.thread.OUTPUT_LINES_TAG
import dev.aas.android.ui.thread.OutputScreen
import dev.aas.android.ui.thread.OutputTarget
import dev.aas.android.ui.thread.OutputViewModel
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.runBlocking
import org.junit.After
import org.junit.Rule
import org.junit.Test
import org.junit.rules.TemporaryFolder
import org.junit.runner.RunWith
import org.robolectric.Shadows.shadowOf
import org.robolectric.annotation.Config

/**
 * The output of a background shell while it runs (docs/android.md 30), as the user sees it
 * (Robolectric + Compose) against a scripted daemon that streams `backgroundTask/outputDelta`:
 * the full view opens at the newest line and follows the output as it arrives; scrolling up stops
 * following (new lines do not pull the view away), and 最新の出力へ follows again.
 */
@RunWith(AndroidJUnit4::class)
@Config(application = TestAasApplication::class, qualifiers = "w411dp-h891dp-xxhdpi")
class OutputViewTest {
    @get:Rule
    val compose = createComposeRule()

    @get:Rule
    val temp = TemporaryFolder()

    private val env = TestEngine()
    private val read = Fixtures.threadRead
    private val stream = threadStream(read.thread.id)
    private val devServer = read.backgroundTasks.single { it.status == BackgroundTaskStatus.Running && it.kind == BackgroundTaskKind.Shell }
    private var seq = read.head
    private val viewModels = mutableListOf<OutputViewModel>()

    @After
    fun tearDown() {
        dev.aas.android.testing.clearViewModels(viewModels)
        env.close()
    }

    private fun idle() {
        shadowOf(Looper.getMainLooper()).idle()
        compose.waitForIdle()
    }

    private fun waitFor(text: String) {
        compose.waitUntil(WAIT_MS) {
            idle()
            compose.onAllNodes(hasText(text)).fetchSemanticsNodes().isNotEmpty()
        }
    }

    /** The daemon streams more of the dev server's output (the thread stream's next event). */
    private fun stream(text: String) {
        seq++
        env.server.append(stream, Event.BackgroundTaskOutputDelta(devServer.id, text), seq = seq)
        env.server.reportedHeads[stream] = seq
        env.server.lastConnection.pushNew(stream)
    }

    @Test
    fun theLiveOutputFollowsItsEndUntilTheUserScrollsAway() {
        val lines = (1..LINES).joinToString("") { "line $it\n" }
        val task = devServer.copy(output = lines, outputTruncated = false)
        env.serve(read.copy(backgroundTasks = listOf(task)), projects = listOf(Samples.project(read.thread.projectId)))
        env.server.reportedHeads[stream] = seq
        runBlocking { env.connect() }
        val container = ApplicationProvider.getApplicationContext<Application>().appContainer
        val blobs = BlobRepository(dev.aas.android.sync.AasHttp(okhttp3.OkHttpClient()), { null }, BlobCache(temp.newFolder("blobs"), 1024 * 1024, Dispatchers.IO)) { _, e ->
            throw AssertionError(e)
        }
        val vm = OutputViewModel(read.thread.id, OutputTarget.OfTask(task.id), ThreadRepository(env.engine, env.reads, env.lists), blobs, AppPolicy())
        viewModels += vm
        compose.setContent {
            CompositionLocalProvider(LocalAppContainer provides container) {
                AasTheme(dynamicColor = false) { OutputScreen(vm, AppNavigator(rememberNavController())) }
            }
        }

        // Opened at the newest line, which is the running output's end.
        waitFor("line $LINES")
        compose.onNodeWithText("実行中の出力です。届くたびに続きが表示されます").assertIsDisplayed()
        compose.onNodeWithText("line $LINES").assertIsDisplayed()
        // More output: followed.
        stream("line ${LINES + 1}\nline ${LINES + 2}\n")
        waitFor("line ${LINES + 2}")
        compose.onNodeWithText("line ${LINES + 2}").assertIsDisplayed()

        // Dragged back (beside the short lines, near the screen's right edge: the list takes the
        // whole width): the user takes over, and more output does not pull the view to the end.
        compose.onRoot().performTouchInput { swipe(Offset(width - EDGE_PX, height * 0.3f), Offset(width - EDGE_PX, height * 0.8f)) }
        idle()
        compose.onNodeWithContentDescription("最新の出力へ").assertIsDisplayed()
        compose.onNodeWithText("line ${LINES + 2}").assertDoesNotExist()
        stream("line ${LINES + 3}\n")
        compose.waitUntil(WAIT_MS) {
            idle()
            vm.state.value.lines?.size == LINES + 3
        }
        idle()
        compose.onNodeWithText("line ${LINES + 3}").assertDoesNotExist()
        compose.onNodeWithContentDescription("最新の出力へ").assertIsDisplayed()

        // Scrolled back without a drag (an accessibility scroll): the same.
        compose.onNodeWithTag(OUTPUT_LINES_TAG).performScrollToIndex(0)
        idle()
        compose.onNodeWithText("line 1").assertIsDisplayed()
        stream("line ${LINES + 4}\n")
        compose.waitUntil(WAIT_MS) {
            idle()
            vm.state.value.lines?.size == LINES + 4
        }
        idle()
        compose.onNodeWithText("line 1").assertIsDisplayed()
        compose.onNodeWithText("line ${LINES + 4}").assertDoesNotExist()

        // 最新の出力へ: back at the end, following again.
        compose.onNode(hasContentDescription("最新の出力へ")).performClick()
        waitFor("line ${LINES + 4}")
        stream("line ${LINES + 5}\n")
        waitFor("line ${LINES + 5}")
        compose.onNodeWithText("line ${LINES + 5}").assertIsDisplayed()
    }

    private companion object {
        const val WAIT_MS = 10_000L

        /** How far from the screen's right edge the drag is made. */
        const val EDGE_PX = 20f

        /** Far more lines than the screen shows. */
        const val LINES = 200
    }
}
