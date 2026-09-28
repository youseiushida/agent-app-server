package dev.aas.android.ui

import android.app.Application
import android.content.Intent
import android.os.Looper
import androidx.compose.ui.test.hasTestTag
import androidx.compose.ui.test.hasText
import androidx.compose.ui.test.junit4.v2.createEmptyComposeRule
import androidx.compose.ui.test.onNodeWithTag
import androidx.compose.ui.test.onNodeWithText
import androidx.compose.ui.test.performClick
import androidx.compose.ui.test.performTextInput
import androidx.test.core.app.ActivityScenario
import androidx.test.core.app.ApplicationProvider
import androidx.test.ext.junit.runners.AndroidJUnit4
import dev.aas.android.MainActivity
import dev.aas.android.appContainer
import dev.aas.android.protocol.AasJson
import dev.aas.android.protocol.Command
import dev.aas.android.protocol.CommandAction
import dev.aas.android.protocol.CommandListResult
import dev.aas.android.protocol.CommandSource
import dev.aas.android.protocol.HarnessCapabilities
import dev.aas.android.protocol.Methods
import dev.aas.android.protocol.NativeImportParams
import dev.aas.android.protocol.NativeListResult
import dev.aas.android.protocol.NativeSession
import dev.aas.android.protocol.ProjectDefaults
import dev.aas.android.protocol.RpcMessage
import dev.aas.android.protocol.ThreadReadResult
import dev.aas.android.protocol.ThreadResult
import dev.aas.android.protocol.WorkspaceSnapshotResult
import dev.aas.android.security.PairingInfo
import dev.aas.android.security.PairingState
import dev.aas.android.sync.FakeServer
import dev.aas.android.sync.Samples
import dev.aas.android.sync.eventually
import dev.aas.android.ui.composer.ComposerTags
import dev.aas.android.ui.navigation.DeepLinks
import dev.aas.android.ui.projects.nativeSessionRowTag
import kotlinx.coroutines.flow.first
import kotlinx.coroutines.runBlocking
import kotlinx.serialization.json.JsonElement
import kotlinx.serialization.json.JsonObject
import kotlinx.serialization.json.jsonPrimitive
import org.junit.After
import org.junit.Rule
import org.junit.Test
import org.junit.runner.RunWith
import org.robolectric.Shadows.shadowOf
import org.robolectric.annotation.Config
import kotlin.test.assertEquals
import kotlin.test.assertTrue

/**
 * `/resume` in the real activity and navigation graph, with the app connected to a scripted
 * daemon (Robolectric + Compose): the palette entry opens 「PC のセッションを取り込む」 for the
 * thread's project with the thread's harness listed first (not the project's default), a session
 * is imported (`native/import`) and its thread replaces the import screen; one imported before
 * opens its thread without importing again; the thread's own session only closes the screen.
 */
@RunWith(AndroidJUnit4::class)
@Config(application = TestAasApplication::class, qualifiers = "w411dp-h891dp-xxhdpi")
class ResumeNavigationTest {
    @get:Rule
    val compose = createEmptyComposeRule()

    private val app get() = ApplicationProvider.getApplicationContext<Application>()
    private val server = FakeServer()
    private val canList = HarnessCapabilities(nativeSessions = true)
    private val project = Samples.project("prj_1", name = "agent-app-server").copy(defaults = ProjectDefaults(harnessId = "codex"))
    private val thread = Samples.thread("thr_1", title = "Current work", harnessId = "claude")
    private val imported = Samples.thread("thr_imp", title = "Fix the build (from the PC)", harnessId = "claude")

    @After
    fun tearDown() {
        server.close()
    }

    private fun read(t: dev.aas.android.protocol.Thread) = ThreadReadResult(t, emptyList(), emptyList(), emptyList(), emptyList(), 0, false)

    private fun json(value: NativeListResult): JsonElement = AasJson.encodeToJsonElement(NativeListResult.serializer(), value)

    private fun harnessOf(msg: RpcMessage) = (msg.params as JsonObject)["harnessId"]!!.jsonPrimitive.content

    /** The daemon: Codex (the project's default) and Claude (the thread's), both listing sessions; [claudeSessions] for Claude. */
    private fun connect(claudeSessions: List<NativeSession>) = runBlocking {
        server.snapshot = WorkspaceSnapshotResult(
            harnesses = listOf(
                Samples.harness("codex", capabilities = canList).copy(displayName = "Codex"),
                Samples.harness("claude", capabilities = canList).copy(displayName = "Claude"),
            ),
            projects = listOf(project),
            threads = listOf(thread),
            pendingInteractions = emptyList(),
            operations = emptyList(),
            head = 0,
        )
        server.threadReads[thread.id] = read(thread)
        server.threadReads[imported.id] = read(imported)
        server.onRequest = { _, msg ->
            when (msg.method) {
                Methods.NativeList.name -> json(NativeListResult(if (harnessOf(msg) == "claude") claudeSessions else emptyList()))
                Methods.NativeImport.name -> AasJson.encodeToJsonElement(ThreadResult.serializer(), ThreadResult(imported))
                // A harness's own /resume (an older daemon passing it through): never offered.
                Methods.CommandList.name -> AasJson.encodeToJsonElement(
                    CommandListResult.serializer(),
                    CommandListResult(listOf(Command("resume", "Resume a conversation", CommandSource.Harness, action = CommandAction.InsertText("/resume ")))),
                )
                else -> JsonObject(emptyMap())
            }
        }
        val container = app.appContainer
        container.credentialStore.save(PairingInfo(server.wsUrl, "home pc", "dev_1", "Pixel", 1), "tok")
        container.pairingState.first { it is PairingState.Paired }
        container.engine.start()
        eventually(what = "the synced workspace") { container.engine.workspace.value.takeIf { it.synced && it.threads.isNotEmpty() && it.harnesses.size == 2 } }
    }

    private fun idle() {
        shadowOf(Looper.getMainLooper()).idle()
        compose.waitForIdle()
    }

    private fun waitFor(what: String, condition: () -> Boolean) {
        compose.waitUntil(WAIT_MS) {
            idle()
            condition()
        }
        check(condition()) { what }
    }

    private fun waitForText(text: String) = waitFor(text) { compose.onAllNodes(hasText(text)).fetchSemanticsNodes().isNotEmpty() }

    /** Opens the thread (as a notification would) and runs `/resume` from the palette. */
    private fun resumeFromTheThread() {
        waitForText("Current work")
        compose.onNodeWithTag(ComposerTags.INPUT).performTextInput("/resu")
        waitForText("/resume")
        // The daemon's command list (with the harness's /resume) has arrived.
        waitFor("command/list answered") {
            server.requestsFor(Methods.CommandList.name).isNotEmpty() &&
                compose.onAllNodes(hasText("コマンドを読み込んでいます…")).fetchSemanticsNodes().isEmpty()
        }
        assertEquals(1, compose.onAllNodes(hasText("/resume")).fetchSemanticsNodes().size, "the harness's /resume is not offered next to the app's")
        assertTrue(compose.onAllNodes(hasText("ハーネス")).fetchSemanticsNodes().isEmpty())
        compose.onNodeWithText("/resume").performClick()
        waitForText("PC のセッションを取り込む")
    }

    private fun launchThread() = ActivityScenario.launch<MainActivity>(Intent(app, MainActivity::class.java).setData(DeepLinks.thread(thread.id)))

    @Test
    fun resumeImportsASessionOfTheThreadsHarnessAndOpensItsThread() {
        connect(
            listOf(
                NativeSession("s1", title = "Fix the build", updatedAt = 100),
                NativeSession("s2", title = "Plan the release", updatedAt = 200),
                NativeSession("s1", title = "Fix the build", updatedAt = 300),
            ),
        )
        launchThread().use {
            resumeFromTheThread()
            waitForText("Fix the build")
            assertEquals(listOf("claude"), server.requestsFor(Methods.NativeList.name).map { harnessOf(it.second) }, "the thread's harness, not the project's default")
            assertEquals(1, compose.onAllNodes(hasTestTag(nativeSessionRowTag("s1"))).fetchSemanticsNodes().size)

            compose.onNodeWithText("Fix the build").performClick()
            waitForText("Fix the build (from the PC)")
            val params = AasJson.decodeFromJsonElement(NativeImportParams.serializer(), server.requestsFor(Methods.NativeImport.name).single().second.params!!)
            assertEquals(Triple("prj_1", "claude", "s1"), Triple(params.projectId, params.harnessId, params.nativeSessionId))

            // The imported thread replaced the import screen: back returns to the thread /resume came from.
            it.onActivity { activity -> activity.onBackPressedDispatcher.onBackPressed() }
            waitForText("Current work")
            assertTrue(compose.onAllNodes(hasText("PC のセッションを取り込む")).fetchSemanticsNodes().isEmpty())
        }
    }

    @Test
    fun aSessionImportedBeforeOpensItsThreadWithoutImportingAgain() {
        connect(listOf(NativeSession("s1", title = "Fix the build", updatedAt = 100, importedThreadId = imported.id)))
        launchThread().use {
            resumeFromTheThread()
            waitForText("取り込み済み")
            compose.onNodeWithText("Fix the build").performClick()
            waitForText("Fix the build (from the PC)")
            assertTrue(server.requestsFor(Methods.NativeImport.name).isEmpty())
        }
    }

    @Test
    fun theThreadsOwnSessionOnlyClosesTheImportScreen() {
        connect(listOf(NativeSession("s0", title = "This very thread", updatedAt = 100, importedThreadId = thread.id)))
        launchThread().use {
            resumeFromTheThread()
            waitForText("This very thread")
            compose.onNodeWithText("This very thread").performClick()
            waitForText("Current work")
            waitFor("the import screen closed") { compose.onAllNodes(hasText("PC のセッションを取り込む")).fetchSemanticsNodes().isEmpty() }
            // The thread is on the back stack once: back leaves it for the project list.
            it.onActivity { activity -> activity.onBackPressedDispatcher.onBackPressed() }
            waitForText("agent-app-server")
            assertTrue(compose.onAllNodes(hasText("Current work")).fetchSemanticsNodes().isEmpty())
        }
    }

    private companion object {
        const val WAIT_MS = 15_000L
    }
}
