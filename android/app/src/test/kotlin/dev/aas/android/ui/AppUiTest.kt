package dev.aas.android.ui

import android.app.Application
import android.os.Looper
import androidx.compose.ui.test.assertIsDisplayed
import androidx.compose.ui.test.hasText
import androidx.compose.ui.test.isEnabled
import androidx.compose.ui.test.junit4.v2.createEmptyComposeRule
import androidx.compose.ui.test.onNodeWithText
import androidx.compose.ui.test.performClick
import androidx.compose.ui.test.performTextInput
import androidx.test.core.app.ActivityScenario
import androidx.test.core.app.ApplicationProvider
import androidx.test.ext.junit.runners.AndroidJUnit4
import dev.aas.android.AasApplication
import dev.aas.android.AppContainer
import dev.aas.android.MainActivity
import dev.aas.android.appContainer
import dev.aas.android.protocol.AasJson
import dev.aas.android.protocol.InteractionResolution
import dev.aas.android.protocol.InteractionRespondParams
import dev.aas.android.protocol.Methods
import dev.aas.android.protocol.WORKSPACE_STREAM
import dev.aas.android.security.FakeKeyProvider
import dev.aas.android.security.PairingInfo
import dev.aas.android.security.PairingState
import dev.aas.android.sync.Samples
import dev.aas.android.sync.eventually
import kotlinx.coroutines.flow.first
import kotlinx.coroutines.runBlocking
import org.junit.Rule
import org.junit.Test
import org.junit.runner.RunWith
import org.robolectric.Shadows.shadowOf
import org.robolectric.annotation.Config
import kotlin.test.assertEquals

/** The app with a software key instead of the Android Keystore (absent under Robolectric). */
class TestAasApplication : AasApplication() {
    override fun createContainer(): AppContainer = AppContainer(this, keyProviderFactory = { FakeKeyProvider() })
}

/**
 * The real activity, navigation graph and screens (Robolectric + Compose test): the unpaired
 * start, the shell with its tabs, and answering an approval from the 要対応 tab into the outbox.
 */
@RunWith(AndroidJUnit4::class)
// A phone-sized screen (Robolectric's default is 320×470 dp, where the card lies below the fold).
@Config(application = TestAasApplication::class, qualifiers = "w411dp-h891dp-xxhdpi")
class AppUiTest {
    @get:Rule
    val compose = createEmptyComposeRule()

    private val app get() = ApplicationProvider.getApplicationContext<Application>()

    @Test
    fun unpairedTheAppStartsWithPairingAndValidatesManualEntry() {
        ActivityScenario.launch(MainActivity::class.java).use {
            compose.onNodeWithText("PC とつなぎましょう").assertIsDisplayed()
            compose.onNodeWithText("手で入力").performClick()
            compose.onNodeWithText("サーバの URL").performTextInput("https://pc.example/v1/ws")
            compose.onNodeWithText("ペアリングコード").performTextInput("ABCD-1234")
            compose.onNodeWithText("次へ").performClick()
            compose.onNodeWithText("URL が不正です", substring = true).assertIsDisplayed()
        }
    }

    @Test
    fun pairedTheShellShowsTabsAndAnApprovalIsAnsweredFromTheInbox() = runBlocking<Unit> {
        val container = app.appContainer
        // A pairing whose server is not reachable: the app works from its stored data.
        container.credentialStore.save(PairingInfo("ws://127.0.0.1:9/v1/ws", "home pc", "dev_1", "Pixel", 1), "token")
        container.pairingState.first { it is PairingState.Paired }
        container.syncStore.transaction { tx ->
            tx.setEpoch("e1")
            tx.setCursor(WORKSPACE_STREAM, 1)
            tx.upsertProject(Samples.project("prj_1", name = "agent-app-server"))
            tx.upsertThread(Samples.thread("thr_1", title = "Fix the reconnect race"))
            tx.upsertInteraction(Samples.approval("int_1", threadId = "thr_1"))
        }
        container.engine.start()
        eventually(what = "the workspace") { container.engine.workspace.value.takeIf { it.synced } }

        ActivityScenario.launch(MainActivity::class.java).use {
            compose.onNodeWithText("agent-app-server").assertIsDisplayed()
            compose.onNodeWithText("要対応").performClick()
            compose.onNodeWithText("Run command?").assertIsDisplayed()
            compose.onNodeWithText("Fix the reconnect race").assertIsDisplayed()
            // The buttons arm after AppPolicy.interactionArmDelayMs.
            compose.waitUntil(ARM_TIMEOUT_MS) { compose.onAllNodes(hasText("許可") and isEnabled()).fetchSemanticsNodes().isNotEmpty() }
            compose.onNode(hasText("許可") and isEnabled()).performClick()

            // The view model resumes on the main thread after the Room commit: run its looper
            // while waiting (Robolectric's looper is paused; waitUntil alone only moves the
            // Compose clock).
            compose.waitUntil(ARM_TIMEOUT_MS) {
                shadowOf(Looper.getMainLooper()).idle()
                container.engine.outbox.value.isNotEmpty()
            }
            val entry = container.engine.outbox.value.single()
            assertEquals(Methods.InteractionRespond.name, entry.method)
            val params = AasJson.decodeFromJsonElement(InteractionRespondParams.serializer(), entry.params)
            assertEquals(InteractionResolution.Approval("allow"), params.resolution)
            compose.waitUntil(ARM_TIMEOUT_MS) { compose.onAllNodes(hasText("回答は送信待ちです", substring = true)).fetchSemanticsNodes().isNotEmpty() }

            compose.onNodeWithText("設定").performClick()
            compose.onNodeWithText("home pc").assertIsDisplayed()
        }
    }

    private companion object {
        const val ARM_TIMEOUT_MS = 5_000L
    }
}
