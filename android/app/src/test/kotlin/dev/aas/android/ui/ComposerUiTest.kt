package dev.aas.android.ui

import android.app.Application
import android.os.Looper
import androidx.compose.runtime.CompositionLocalProvider
import androidx.compose.runtime.collectAsState
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.ui.test.assertIsDisplayed
import androidx.compose.ui.test.assertIsNotEnabled
import androidx.compose.ui.test.hasContentDescription
import androidx.compose.ui.test.hasTestTag
import androidx.compose.ui.test.hasText
import androidx.compose.ui.test.junit4.v2.createComposeRule
import androidx.compose.ui.test.longClick
import androidx.compose.ui.test.onNodeWithTag
import androidx.compose.ui.test.onNodeWithText
import androidx.compose.ui.test.performClick
import androidx.compose.ui.test.performTextClearance
import androidx.compose.ui.test.performTextInput
import androidx.compose.ui.test.performTextReplacement
import androidx.compose.ui.test.performTouchInput
import androidx.test.core.app.ApplicationProvider
import androidx.test.ext.junit.runners.AndroidJUnit4
import dev.aas.android.AppPolicy
import dev.aas.android.appContainer
import dev.aas.android.data.ComposerDrafts
import dev.aas.android.domain.composer.PaletteContext
import dev.aas.android.domain.composer.SendAction
import dev.aas.android.domain.composer.SendBlock
import dev.aas.android.domain.composer.SendState
import dev.aas.android.protocol.CommandListResult
import dev.aas.android.protocol.FsSearchResult
import dev.aas.android.testing.FakeUploader
import dev.aas.android.testing.Fixtures
import dev.aas.android.ui.common.LocalAppContainer
import dev.aas.android.ui.composer.CommandsState
import dev.aas.android.ui.composer.ComposerBar
import dev.aas.android.ui.composer.ComposerController
import dev.aas.android.ui.composer.ComposerTags
import dev.aas.android.ui.composer.PaletteChoice
import dev.aas.android.ui.composer.PromptTemplates
import dev.aas.android.ui.theme.AasTheme
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.SupervisorJob
import kotlinx.coroutines.cancel
import org.junit.After
import org.junit.Rule
import org.junit.Test
import org.junit.runner.RunWith
import org.robolectric.Shadows.shadowOf
import org.robolectric.annotation.Config
import kotlin.test.assertEquals

/**
 * The composer as the user sees it (Robolectric + Compose): the `/` palette from `command/list`
 * plus the app's commands, `@` mentions through `fs/search`, and the send button's states.
 */
@RunWith(AndroidJUnit4::class)
@Config(application = TestAasApplication::class, qualifiers = "w411dp-h891dp-xxhdpi")
class ComposerUiTest {
    @get:Rule
    val compose = createComposeRule()

    private val scope = CoroutineScope(SupervisorJob() + Dispatchers.Main)
    private val searches = mutableListOf<String>()
    private val choices = mutableListOf<PaletteChoice>()
    private val sent = mutableListOf<SendAction>()
    private val send = mutableStateOf(SendState(SendAction.Start, null, SendBlock.Empty))

    private val controller = ComposerController(
        scope = scope,
        uploader = FakeUploader(),
        search = { query ->
            searches += query
            Fixtures.result("fs_search", FsSearchResult.serializer()).results
        },
        policy = AppPolicy(mentionSearchDebounceMs = 0),
        drafts = ComposerDrafts(),
        draftKey = ComposerDrafts.threadKey("thr_1"),
        templates = PromptTemplates("REVIEW TEMPLATE", "INIT TEMPLATE"),
        paletteContext = PaletteContext(inThread = true),
    )

    private fun show() {
        val container = ApplicationProvider.getApplicationContext<Application>().appContainer
        compose.setContent {
            CompositionLocalProvider(LocalAppContainer provides container) {
                AasTheme(dynamicColor = false) {
                    val state by controller.state.collectAsState()
                    val sendState by send
                    ComposerBar(
                        value = controller.textValue,
                        state = state,
                        send = sendState,
                        placeholder = "依頼を入力",
                        imagesAllowed = true,
                        onValueChange = controller::onValueChange,
                        onChoose = { choices += controller.choose(it) },
                        onChooseMention = controller::chooseMention,
                        onPickImages = {},
                        onTakePhoto = null,
                        onRemoveAttachment = controller::removeAttachment,
                        onRetryAttachment = controller::retryAttachment,
                        onSend = { sent += it },
                    )
                }
            }
        }
    }

    private fun idle() {
        shadowOf(Looper.getMainLooper()).idle()
        compose.waitForIdle()
    }

    private val input get() = compose.onNodeWithTag(ComposerTags.INPUT)

    @After
    fun tearDown() = scope.cancel()

    @Test
    fun theSlashPaletteListsCommandsAndInsertsOrRunsThem() {
        controller.setCommands(CommandsState.Loaded(Fixtures.result("command_list", CommandListResult.serializer()).commands))
        show()
        input.performTextInput("/")
        idle()
        compose.onNodeWithTag(ComposerTags.PALETTE).assertIsDisplayed()
        compose.onNodeWithText("/model").assertIsDisplayed()
        compose.onNodeWithText("モデルを選ぶ").assertIsDisplayed()
        compose.onNodeWithText("/fork").assertIsDisplayed()

        // Typing filters; choosing an insert-text command puts it into the message.
        input.performTextInput("comp")
        idle()
        assertEquals(0, compose.onAllNodes(hasText("/model")).fetchSemanticsNodes().size)
        compose.onNodeWithText("[instructions]").assertIsDisplayed()
        compose.onNodeWithText("ハーネス").assertIsDisplayed()
        compose.onNodeWithText("/compact").performClick()
        idle()
        assertEquals("/compact ", controller.textValue.text)
        assertEquals(PaletteChoice.Inserted, choices.last())
        assertEquals(0, compose.onAllNodes(hasTestTag(ComposerTags.PALETTE)).fetchSemanticsNodes().size)

        // A picker command clears its token and asks the screen to open the picker.
        input.performTextClearance()
        input.performTextInput("/mo")
        idle()
        compose.onNodeWithText("/model").performClick()
        idle()
        assertEquals(PaletteChoice.Picker(dev.aas.android.protocol.PickerKind.Model), choices.last())
        assertEquals("", controller.textValue.text)

        // The app's own template commands insert their prompt.
        input.performTextInput("/rev")
        idle()
        compose.onNodeWithText("/review").performClick()
        idle()
        assertEquals("REVIEW TEMPLATE", controller.textValue.text)
    }

    @Test
    fun theDaemonsCommandsAreInViewWhenTheyArriveAfterThePaletteOpened() {
        // The palette opens while command/list is still loading: the app's own commands only.
        controller.setCommands(CommandsState.Loading)
        show()
        input.performTextInput("/")
        idle()
        compose.onNodeWithText("/new").assertIsDisplayed()
        // The daemon's commands come first once they arrive, and the list shows its top: they
        // were scrolled out of view above the previously first row (found on the emulator).
        controller.setCommands(CommandsState.Loaded(Fixtures.result("command_list", CommandListResult.serializer()).commands))
        idle()
        compose.onNodeWithText("/model").assertIsDisplayed()
        compose.onNodeWithText("モデルを選ぶ").assertIsDisplayed()
    }

    @Test
    fun replacingTheWholeTextFiltersThePaletteByTheNewText() {
        controller.setCommands(CommandsState.Loaded(Fixtures.result("command_list", CommandListResult.serializer()).commands))
        show()
        input.performTextInput("/")
        idle()
        compose.onNodeWithText("/model").assertIsDisplayed()
        input.performTextReplacement("/sta")
        idle()
        assertEquals("/sta", controller.textValue.text)
        assertEquals(4, controller.textValue.selection.end)
        compose.onNodeWithText("/status").assertIsDisplayed()
        assertEquals(0, compose.onAllNodes(hasText("/model")).fetchSemanticsNodes().size)
    }

    @Test
    fun offlineThePaletteHasOnlyTheAppsCommandsAndSaysWhy() {
        controller.setCommands(CommandsState.Offline)
        show()
        input.performTextInput("/")
        idle()
        compose.onNodeWithText("オフラインのため、ハーネスのコマンドは表示されません").assertIsDisplayed()
        compose.onNodeWithText("/status").assertIsDisplayed()
    }

    @Test
    fun mentionsSearchTheProjectAndInsertThePath() {
        show()
        input.performTextInput("fix @")
        idle()
        compose.onNodeWithText("入力してファイルを検索").assertIsDisplayed()
        input.performTextInput("rec")
        compose.waitUntil(WAIT_MS) {
            idle()
            compose.onAllNodes(hasText("crates/aas-server/tests/reconnect.rs")).fetchSemanticsNodes().isNotEmpty()
        }
        assertEquals("rec", searches.last())
        compose.onNodeWithText("crates/aas-server/tests/reconnect.rs").performClick()
        idle()
        assertEquals("fix @crates/aas-server/tests/reconnect.rs ", controller.textValue.text)
        assertEquals(setOf("crates/aas-server/tests/reconnect.rs"), controller.currentMentions)
        assertEquals(0, compose.onAllNodes(hasTestTag(ComposerTags.MENTIONS)).fetchSemanticsNodes().size)
    }

    @Test
    fun theSendButtonFollowsTheProtocolStates() {
        show()
        val button = compose.onNodeWithTag(ComposerTags.SEND)
        // Nothing to send.
        button.assertIsNotEnabled()
        send.value = SendState(SendAction.Start, null, null)
        idle()
        compose.onNode(hasTestTag(ComposerTags.SEND) and hasContentDescription("送信")).performClick()
        assertEquals(listOf(SendAction.Start), sent)

        // A running turn and a draft: queue, long press steers.
        send.value = SendState(SendAction.Queue, SendAction.Steer, null)
        idle()
        compose.onNodeWithText("実行中です。送信するとキューに追加（長押しで今すぐ反映）").assertIsDisplayed()
        compose.onNode(hasTestTag(ComposerTags.SEND) and hasContentDescription("キューに追加")).performTouchInput { longClick() }
        assertEquals(SendAction.Steer, sent.last())

        // A running turn and an empty composer: stop.
        send.value = SendState(SendAction.Interrupt, null, null)
        idle()
        compose.onNode(hasTestTag(ComposerTags.SEND) and hasContentDescription("停止")).performClick()
        assertEquals(SendAction.Interrupt, sent.last())
    }

    private companion object {
        const val WAIT_MS = 5_000L
    }
}
