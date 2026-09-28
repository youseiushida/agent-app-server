package dev.aas.android.e2e

import android.widget.EditText
import androidx.test.ext.junit.runners.AndroidJUnit4
import androidx.test.uiautomator.By
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Test
import org.junit.runner.RunWith
import java.util.regex.Pattern

/**
 * Pairing with the real daemon: the manual entry, the notification permission through the real
 * system dialog, the token kept in the Keystore across app restarts, unpairing and pairing again.
 */
@RunWith(AndroidJUnit4::class)
class PairingE2eTest : E2eTest() {
    @Test
    fun pairsByManualEntryWithThePairingCode() {
        app.launch()
        app.waitFor(app.inApp(By.text(app.text("pairing_intro_title"))), "the pairing screen", Waits.APP_START_MS)
        control.screenshot("pairing-intro")
        app.tap(app.inApp(app.button(app.text("pairing_manual"))), "手で入力")
        app.waitFor(app.inApp(By.text(app.text("pairing_manual_body"))), "the manual entry")
        val fields = app.findAll(app.inApp(By.clazz(EditText::class.java.name))).sortedBy { it.visibleBounds.top }
        assertEquals("the URL and the code", 2, fields.size)
        fields[0].text = server.wsUrl
        fields[1].text = control.pairingCode()
        control.screenshot("pairing-manual")
        app.tap(app.inApp(app.button(app.text("next"))), "次へ")

        app.waitFor(app.inApp(By.text(app.text("pairing_confirm_title"))), "the confirmation")
        app.waitFor(app.inApp(By.text(server.wsUrl)), "the URL on the confirmation")
        control.screenshot("pairing-confirm")
        app.tap(app.inApp(app.button(app.text("pairing_pair"))), "ペアリング")

        app.waitFor(app.inApp(By.text(app.text("setup_title"))), "the setup screen")
        control.screenshot("pairing-setup")
        flows.completeSetup()
        flows.waitConnected()
        // The daemon's own name replaced the (absent) name of a manual entry.
        flows.openSettings()
        app.waitFor(app.inApp(By.text("aas-test-server")), "the server's name in the settings")
        control.screenshot("settings")
    }

    @Test
    fun theNotificationPermissionCanBeDeniedInTheSystemDialog() {
        app.resetNotificationPermission()
        flows.pairWithLink(AppFlows.unique("e2e-deny"))
        app.waitFor(app.inApp(By.text(app.text("setup_title"))), "the setup screen")
        app.tap(app.inApp(app.button(app.text("setup_notifications_allow"))), "通知を許可")
        app.waitFor(By.res(Pattern.compile(".*:id/permission_deny_button")), "the system permission dialog")
        app.tapUntilGone(By.res(Pattern.compile(".*:id/permission_deny_button")), "the system dialog's Don't allow")
        assertFalse("POST_NOTIFICATIONS after denying", app.notificationsGranted())
        // The setup still offers the permission; the settings say notifications are off.
        app.waitFor(app.inApp(app.button(app.text("setup_notifications_allow"))), "通知を許可 after denying")
        flows.completeSetup()
        flows.openSettings()
        app.scrollTo(app.inApp(By.text(app.text("settings_notifications_blocked"))), "通知がオフになっています")
    }

    @Test
    fun theNotificationPermissionCanBeGrantedInTheSystemDialog() {
        app.resetNotificationPermission()
        flows.pairWithLink(AppFlows.unique("e2e-grant"))
        app.waitFor(app.inApp(By.text(app.text("setup_title"))), "the setup screen")
        app.tap(app.inApp(app.button(app.text("setup_notifications_allow"))), "通知を許可")
        app.waitFor(By.res(Pattern.compile(".*:id/permission_allow_button")), "the system permission dialog")
        control.screenshot("permission-dialog")
        app.tapUntilGone(By.res(Pattern.compile(".*:id/permission_allow_button")), "the system dialog's Allow")
        app.waitFor(app.inApp(By.text(app.text("setup_notifications_done"))), "通知は許可されています")
        assertTrue("POST_NOTIFICATIONS after allowing", app.notificationsGranted())
        flows.completeSetup()
        flows.waitConnected()
    }

    @Test
    fun theTokenSurvivesAnAppRestart() {
        val name = AppFlows.unique("e2e-restart")
        flows.pairFresh(name)
        // The process ends (the Keystore-encrypted token stays in the app's files)...
        app.forceStop()
        app.waitUntil("the app process to end") { app.pid() == null }
        app.launch()
        // ...and the next start decrypts it and is accepted by the daemon: no pairing screen.
        flows.waitForProjects(Waits.APP_START_MS)
        flows.waitConnected()
        // The daemon was reset for this test: its own test client and this device.
        flows.openDevices(thisDevice = name)
    }

    @Test
    fun unpairingRevokesTheDeviceAndPairingAgainWorks() {
        val first = AppFlows.unique("e2e-unpair")
        flows.pairFresh(first)
        flows.openSettings()
        app.scrollTo(app.inApp(app.button(app.text("settings_unpair"))), "ペアリングを解除").click()
        app.tapNear(
            app.inApp(By.text(app.text("unpair_confirm_title"))),
            app.button(app.text("settings_unpair")),
            "ペアリングを解除 in the confirmation",
        )
        // Revoked on the daemon (the app was connected), local data gone: the pairing screen.
        app.waitFor(app.inApp(By.text(app.text("pairing_intro_title"))), "the pairing screen after unpairing")

        val second = AppFlows.unique("e2e-repair")
        flows.pairWithLink(second)
        // A new pairing after an unpairing may skip the setup (already done on this install).
        app.waitUntil("the setup or the projects after pairing again") {
            app.find(app.inApp(By.text(app.text("setup_title")))) != null || app.find(app.inApp(By.desc(app.text("project_new")))) != null
        }
        if (app.find(app.inApp(By.text(app.text("setup_title")))) != null) flows.completeSetup()
        flows.waitConnected()
        flows.openDevices(thisDevice = second)
        assertTrue("the unpaired device is revoked (not listed)", app.find(app.inApp(By.textContains(first))) == null)
    }
}
