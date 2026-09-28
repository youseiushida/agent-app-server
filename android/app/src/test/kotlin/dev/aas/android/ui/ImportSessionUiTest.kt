package dev.aas.android.ui

import android.app.Application
import android.os.Looper
import androidx.compose.runtime.CompositionLocalProvider
import androidx.compose.ui.test.hasTestTag
import androidx.compose.ui.test.hasText
import androidx.compose.ui.test.junit4.v2.createComposeRule
import androidx.compose.ui.test.onNodeWithText
import androidx.compose.ui.test.performClick
import androidx.navigation.compose.rememberNavController
import androidx.test.core.app.ApplicationProvider
import androidx.test.ext.junit.runners.AndroidJUnit4
import dev.aas.android.AppPolicy
import dev.aas.android.appContainer
import dev.aas.android.data.HarnessRepository
import dev.aas.android.data.ProjectRepository
import dev.aas.android.data.WorkspaceRepository
import dev.aas.android.protocol.AasJson
import dev.aas.android.protocol.ErrorKind
import dev.aas.android.protocol.Event
import dev.aas.android.protocol.HarnessCapabilities
import dev.aas.android.protocol.Methods
import dev.aas.android.protocol.NativeListResult
import dev.aas.android.protocol.NativeSession
import dev.aas.android.protocol.RpcError
import dev.aas.android.protocol.WORKSPACE_STREAM
import dev.aas.android.sync.Samples
import dev.aas.android.testing.TestEngine
import dev.aas.android.ui.common.LocalAppContainer
import dev.aas.android.ui.common.UserMessages
import dev.aas.android.ui.navigation.AppNavigator
import dev.aas.android.ui.projects.ImportSessionScreen
import dev.aas.android.ui.projects.ImportSessionViewModel
import dev.aas.android.ui.projects.nativeSessionRowTag
import dev.aas.android.ui.theme.AasTheme
import kotlinx.coroutines.runBlocking
import kotlinx.serialization.json.JsonObject
import kotlinx.serialization.json.JsonPrimitive
import kotlinx.serialization.json.buildJsonObject
import kotlinx.serialization.json.jsonPrimitive
import org.junit.After
import org.junit.Rule
import org.junit.Test
import org.junit.runner.RunWith
import org.robolectric.Shadows.shadowOf
import org.robolectric.annotation.Config
import kotlin.test.assertEquals

/**
 * 「PC のセッションを取り込む」 as the user sees it (Robolectric + Compose) against a scripted
 * daemon whose Codex lists a session several times (one entry per rollout of a thread resumed in
 * Codex desktop): the screen shows each session once instead of crashing on the repeated key,
 * and a harness whose listing fails shows why in place while the others stay selectable. A
 * harness that cannot list its sessions at all says so without a retry that cannot succeed.
 */
@RunWith(AndroidJUnit4::class)
@Config(application = TestAasApplication::class, qualifiers = "w411dp-h891dp-xxhdpi")
class ImportSessionUiTest {
    @get:Rule
    val compose = createComposeRule()

    private val env = TestEngine()
    private val canList = HarnessCapabilities(nativeSessions = true)

    @After
    fun tearDown() {
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

    @Test
    fun aSessionListedSeveralTimesIsShownOnceAndAFailingHarnessDoesNotBlockSwitching() {
        env.serve(
            null,
            harnesses = listOf(
                Samples.harness("codex", capabilities = canList).copy(displayName = "Codex"),
                Samples.harness("claude", capabilities = canList).copy(displayName = "Claude"),
            ),
            projects = listOf(Samples.project("prj_1")),
        )
        env.answers[Methods.NativeList.name] = { msg ->
            when ((msg.params as JsonObject)["harnessId"]!!.jsonPrimitive.content) {
                "codex" -> AasJson.encodeToJsonElement(
                    NativeListResult.serializer(),
                    NativeListResult(
                        listOf(
                            NativeSession("019a", title = "Fix the build", updatedAt = 100),
                            NativeSession("019b", title = "Plan the release", updatedAt = 200),
                            NativeSession("019a", title = "Fix the build", updatedAt = 300),
                            NativeSession("019a", title = "Fix the build", updatedAt = 50),
                        ),
                    ),
                )
                else -> RpcError(
                    ErrorKind.HarnessUnavailable.code,
                    "harness claude is unavailable",
                    buildJsonObject {
                        put("kind", JsonPrimitive("harnessUnavailable"))
                        put("harnessId", JsonPrimitive("claude"))
                        put("reason", JsonPrimitive("claude is not logged in"))
                    },
                )
            }
        }
        runBlocking { env.connect() }
        // The thread `/resume` came from is Claude's: its failure is shown first.
        show(requested = "claude")

        waitFor("「Claude」を使えません: claude is not logged in")
        compose.onNodeWithText("再確認").assertExists()
        compose.onNodeWithText("Codex").performClick()
        waitFor("Fix the build")
        idle()
        assertEquals(1, compose.onAllNodes(hasTestTag(nativeSessionRowTag("019a"))).fetchSemanticsNodes().size)
        assertEquals(1, compose.onAllNodes(hasText("Fix the build")).fetchSemanticsNodes().size)
        assertEquals(1, compose.onAllNodes(hasTestTag(nativeSessionRowTag("019b"))).fetchSemanticsNodes().size)
        assertEquals(listOf("claude", "codex"), env.requests(Methods.NativeList.name).map { (it.params as JsonObject)["harnessId"]!!.jsonPrimitive.content })

        // Back to Claude: its failure again, in place.
        compose.onNodeWithText("Claude").performClick()
        waitFor("「Claude」を使えません: claude is not logged in")
    }

    /**
     * `/resume` in the thread of an ACP agent (Devin) that was unavailable, while Codex is not
     * logged in: the server probes Devin and answers `capabilityUnsupported`. The screen says Devin
     * cannot list its sessions, without 再試行 (it used to show the server's English error with a
     * 再試行 that failed every time), and lists Codex once `harness/updated` makes it usable.
     */
    @Test
    fun aHarnessThatCannotListSaysSoWithoutRetryAndOneThatBecomesUsableIsListed() {
        val codex = Samples.harness("codex", capabilities = canList).copy(displayName = "Codex")
        env.serve(
            null,
            harnesses = listOf(
                codex.copy(available = false, unavailableReason = "codex is not logged in", capabilities = HarnessCapabilities()),
                Samples.harness("devin", available = false, reason = "devin not found").copy(displayName = "Devin"),
            ),
            projects = listOf(Samples.project("prj_1")),
        )
        env.answers[Methods.NativeList.name] = { msg ->
            when ((msg.params as JsonObject)["harnessId"]!!.jsonPrimitive.content) {
                "devin" -> RpcError(
                    ErrorKind.CapabilityUnsupported.code,
                    "this harness cannot list its sessions",
                    buildJsonObject {
                        put("kind", JsonPrimitive("capabilityUnsupported"))
                        put("capability", JsonPrimitive("nativeSessions"))
                    },
                )
                else -> AasJson.encodeToJsonElement(NativeListResult.serializer(), NativeListResult(listOf(NativeSession("019a", title = "Fix the build"))))
            }
        }
        runBlocking { env.connect() }
        show(requested = "devin")

        waitFor("「Devin」は PC のセッションの一覧に対応していません")
        assertEquals(0, compose.onAllNodes(hasText("再試行")).fetchSemanticsNodes().size, "no retry: the answer would be the same")
        assertEquals(0, compose.onAllNodes(hasText("再確認")).fetchSemanticsNodes().size)

        // The user logs in to Codex on the PC; the server's probe publishes it.
        env.server.append(WORKSPACE_STREAM, Event.HarnessUpdated(codex))
        env.server.lastConnection.pushNew(WORKSPACE_STREAM)
        waitFor("Fix the build")
        assertEquals(0, compose.onAllNodes(hasText("Devin")).fetchSemanticsNodes().size, "Devin is not offered: it cannot list")
        assertEquals(listOf("devin", "codex"), env.requests(Methods.NativeList.name).map { (it.params as JsonObject)["harnessId"]!!.jsonPrimitive.content })
    }

    /** The import screen for `/resume` from a thread of [requested], against the scripted daemon. */
    private fun show(requested: String) {
        val container = ApplicationProvider.getApplicationContext<Application>().appContainer
        val vm = ImportSessionViewModel(
            "prj_1", requested, ProjectRepository(env.engine, env.reads, env.lists), WorkspaceRepository(env.engine), UserMessages(),
            env.engine.outbox, HarnessRepository(env.engine), AppPolicy(),
        )
        compose.setContent {
            CompositionLocalProvider(LocalAppContainer provides container) {
                AasTheme(dynamicColor = false) {
                    ImportSessionScreen(vm, AppNavigator(rememberNavController()))
                }
            }
        }
    }

    private companion object {
        const val WAIT_MS = 10_000L
    }
}
