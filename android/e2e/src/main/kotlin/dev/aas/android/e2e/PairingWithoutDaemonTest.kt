package dev.aas.android.e2e

import androidx.test.ext.junit.runners.AndroidJUnit4
import androidx.test.uiautomator.By
import org.junit.Assert.assertEquals
import org.junit.Assert.assertTrue
import org.junit.Before
import org.junit.Test
import org.junit.runner.RunWith

/**
 * The app on its own, without the real daemon (these run in every connected run, also without
 * android/scripts/run-device-tests.ps1): a fresh install, the pairing link, and a pairing with a
 * small HTTP server on the device. On the staging build they reach what R8 can break without a
 * compile error: typed navigation routes and the protocol types (kotlinx.serialization), Room's
 * generated database, DataStore, the Keystore-encrypted token, OkHttp (HTTP and the WebSocket),
 * and starting the connection service.
 */
@RunWith(AndroidJUnit4::class)
class PairingWithoutDaemonTest {
    private lateinit var app: AppDriver

    @Before
    fun startFromAFreshApp() {
        app = AppDriver()
        app.goHome()
        app.resetApp()
    }

    @Test
    fun aFreshInstallStartsOnThePairingScreen() {
        app.launch()
        app.waitFor(app.inApp(By.text(app.text("pairing_intro_title"))), "the pairing screen", Waits.APP_START_MS)
        app.waitFor(app.inApp(app.button(app.text("pairing_manual"))), "manual pairing")
    }

    @Test
    fun aPairingLinkAsksForConfirmation() {
        app.openLink(AppFlows.pairingLink("ws://127.0.0.1:7878/v1/ws", "ABCD-EFGH", "staging-pc"))
        app.waitFor(app.inApp(By.text(app.text("pairing_confirm_title"))), "the confirmation", Waits.APP_START_MS)
        app.waitFor(app.inApp(By.text("staging-pc")), "the server name from the link")
    }

    @Test
    fun pairingWithALocalServerStoresTheTokenAndConnects() {
        LocalServer { request ->
            when (request.path) {
                PAIR_PATH -> LocalServer.Response(
                    LocalServer.HTTP_OK,
                    """{"deviceId":"dev_local","token":"$TOKEN","server":{"name":"local-pc","epoch":"e1"}}""",
                )
                // Not a WebSocket server: the engine gets a refused upgrade and keeps retrying.
                else -> LocalServer.Response(LocalServer.HTTP_SERVICE_UNAVAILABLE, """{"kind":"internal","message":"no WebSocket here"}""")
            }
        }.use { server ->
            app.openLink(AppFlows.pairingLink("ws://${LocalServer.LOOPBACK}:${server.port}/v1/ws", "ABCD-EFGH", "local-pc"))
            app.waitFor(app.inApp(By.text(app.text("pairing_confirm_title"))), "the confirmation", Waits.APP_START_MS)
            app.tap(app.inApp(app.button(app.text("pairing_pair"))), "ペアリング")

            // The protocol's PairRequest, serialized by the app.
            val pair = server.nextRequest(REQUEST_TIMEOUT_MS)
            assertEquals("POST", pair.method)
            assertEquals(PAIR_PATH, pair.path)
            assertTrue(pair.body, pair.body.contains("\"code\":\"ABCD-EFGH\""))
            assertTrue(pair.body, pair.body.contains("\"platform\":\"android\""))

            // The response decoded, the token stored (encrypted) and read back for the
            // connection service's WebSocket upgrade.
            app.waitFor(app.inApp(By.text(app.text("setup_title"))), "the setup screen after the first pairing")
            val upgrade = server.nextRequest(REQUEST_TIMEOUT_MS)
            assertEquals(WS_PATH, upgrade.path)
            assertEquals("Bearer $TOKEN", upgrade.header("Authorization"))
            assertEquals("websocket", upgrade.header("Upgrade")?.lowercase())
        }
    }

    private companion object {
        /** Upper bound for one request to reach the local server (a cold start of the app on an emulator). */
        const val REQUEST_TIMEOUT_MS = 30_000L
        const val PAIR_PATH = "/v1/pair"
        const val WS_PATH = "/v1/ws"
        const val TOKEN = "local-token"
    }
}
