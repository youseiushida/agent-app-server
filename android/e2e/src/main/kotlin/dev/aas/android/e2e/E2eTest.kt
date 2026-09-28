package dev.aas.android.e2e

import org.junit.After
import org.junit.Before

/**
 * Base of the tests against the real daemon (android/scripts/run-device-tests.ps1). Each test
 * starts from a cleared app (as freshly installed) and a reset daemon (`reset`: a new database,
 * so no device, project, thread or pending approval of an earlier test is left to notify or to
 * show up in lists), and pairs the app as the daemon's first device. The project folders of
 * earlier tests stay on the PC, so tests still use unique names.
 */
abstract class E2eTest {
    protected lateinit var server: ServerArgs
    protected lateinit var app: AppDriver
    protected lateinit var control: HostControl
    protected lateinit var flows: AppFlows

    @Before
    fun startFromAFreshApp() {
        server = ServerArgs.require()
        app = AppDriver()
        control = HostControl(server.controlPort, server.screenshots)
        flows = AppFlows(app, control, server)
        // A test that failed half way may have left the proxy misbehaving, or the phone without
        // a network (airplane mode).
        control.run("chaos pass", "reset")
        app.setAirplaneMode(false)
        app.goHome()
        app.resetApp()
    }

    @After
    fun leaveTheServerUsable() {
        if (::control.isInitialized) control.chaos("pass")
        if (::app.isInitialized) {
            app.setAirplaneMode(false)
            app.goHome()
        }
    }
}
