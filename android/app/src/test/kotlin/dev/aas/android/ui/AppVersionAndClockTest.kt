package dev.aas.android.ui

import android.app.Application
import android.os.Looper
import android.os.SystemClock
import androidx.compose.ui.test.hasText
import androidx.compose.ui.test.junit4.v2.createEmptyComposeRule
import androidx.compose.ui.test.onNodeWithText
import androidx.compose.ui.test.performClick
import androidx.compose.ui.test.performScrollTo
import androidx.test.core.app.ActivityScenario
import androidx.test.core.app.ApplicationProvider
import androidx.test.ext.junit.runners.AndroidJUnit4
import dev.aas.android.AndroidClock
import dev.aas.android.AppVersion
import dev.aas.android.MainActivity
import dev.aas.android.appContainer
import dev.aas.android.protocol.WORKSPACE_STREAM
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
import org.robolectric.shadows.ShadowSystemClock
import java.io.File
import java.time.Duration
import java.util.concurrent.TimeUnit
import kotlin.test.assertEquals
import kotlin.test.assertSame
import kotlin.test.assertTrue

/**
 * The app's clock for the engine (it must count deep sleep) and its version (from the git commit
 * it was built from, shown in 設定 → 診断).
 */
@RunWith(AndroidJUnit4::class)
@Config(application = TestAasApplication::class, qualifiers = "w411dp-h891dp-xxhdpi")
class AppVersionAndClockTest {
    @get:Rule
    val compose = createEmptyComposeRule()

    private val app get() = ApplicationProvider.getApplicationContext<Application>()

    /**
     * The engine measures a connection's silence with elapsedRealtime, which keeps counting while
     * the phone sleeps (System.nanoTime stops there, and the app showed a dead socket as
     * connected after waking).
     */
    @Test
    fun theEnginesClockIsElapsedRealtime() {
        val clock = app.appContainer.clock
        assertSame(AndroidClock, clock)
        val before = clock.monotonicMs()
        assertEquals(SystemClock.elapsedRealtime(), before)
        ShadowSystemClock.advanceBy(Duration.ofMinutes(10))
        assertEquals(before + Duration.ofMinutes(10).toMillis(), clock.monotonicMs())
    }

    @Test
    fun theVersionNamesTheCommitItWasBuiltFrom() {
        val version = AppVersion.Current
        assertTrue(Regex("""0\.1\.0\+([0-9a-f]{7,40}(\.dirty)?|nogit)""").matches(version.name), version.name)
        assertTrue(version.code >= 1, "$version")
        assertEquals("debug", version.buildType)
        val hash = git("rev-parse", "--short=7", "HEAD") ?: return
        assertTrue(version.name.startsWith("0.1.0+$hash"), "$version is not HEAD's ($hash)")
        val count = git("rev-list", "--count", "HEAD")?.toInt()
        val shallow = git("rev-parse", "--is-shallow-repository") == "true"
        assertEquals(if (shallow || count == null) 1 else count, version.code, "versionCode is the commit count")
    }

    @Test
    fun diagnosticsShowTheVersion() {
        runBlocking {
            val container = app.appContainer
            container.credentialStore.save(PairingInfo("ws://127.0.0.1:9/v1/ws", "home pc", "dev_1", "Pixel", 1), "token")
            container.pairingState.first { it is PairingState.Paired }
            container.syncStore.transaction { tx ->
                tx.setEpoch("e1")
                tx.setCursor(WORKSPACE_STREAM, 1)
                tx.upsertProject(Samples.project("prj_1", name = "agent-app-server"))
            }
            container.engine.start()
            eventually(what = "the workspace") { container.engine.workspace.value.takeIf { it.synced } }
        }
        val version = AppVersion.Current
        ActivityScenario.launch(MainActivity::class.java).use {
            waitFor("agent-app-server")
            compose.onNodeWithText("設定").performClick()
            waitFor("診断を開く")
            compose.onNodeWithText("診断を開く").performScrollTo().performClick()
            waitFor("${version.name}（versionCode ${version.code}、debug ビルド）")
        }
    }

    private fun waitFor(text: String) {
        compose.waitUntil(WAIT_MS) {
            shadowOf(Looper.getMainLooper()).idle()
            compose.waitForIdle()
            compose.onAllNodes(hasText(text)).fetchSemanticsNodes().isNotEmpty()
        }
    }

    /**
     * git's answer in this repository, or null without git (the build's fallback is checked
     * above). Without optional locks, like the build's own calls: it never holds `.git/index.lock`
     * while the user's git commands run.
     */
    private fun git(vararg args: String): String? = try {
        val process = ProcessBuilder(listOf("git", "--no-optional-locks") + args).directory(File(".").absoluteFile).redirectErrorStream(true).start()
        val out = process.inputStream.bufferedReader().readText().trim()
        if (process.waitFor(GIT_TIMEOUT_S, TimeUnit.SECONDS) && process.exitValue() == 0) out else null
    } catch (e: java.io.IOException) {
        null
    }

    private companion object {
        const val WAIT_MS = 10_000L

        /** A local git query answers in milliseconds. */
        const val GIT_TIMEOUT_S = 30L
    }
}
