package dev.aas.android.e2e

import android.annotation.SuppressLint
import android.app.Instrumentation
import android.content.Context
import android.content.Intent
import android.content.pm.PackageManager
import android.content.res.Resources
import android.net.Uri
import android.os.Build
import android.os.ParcelFileDescriptor
import android.os.SystemClock
import android.util.Log
import androidx.test.platform.app.InstrumentationRegistry
import androidx.test.uiautomator.By
import androidx.test.uiautomator.BySelector
import androidx.test.uiautomator.Configurator
import androidx.test.uiautomator.StaleObjectException
import androidx.test.uiautomator.UiDevice
import androidx.test.uiautomator.UiObject2
import java.io.ByteArrayOutputStream
import java.util.regex.Pattern

/** How long the device tests wait (test policy values, docs/android.md 23.2). */
object Waits {
    /** A screen or element after an action in a running app. */
    const val SCREEN_MS = 30_000L

    /**
     * The first screen after the app's process starts (every test starts from a cleared,
     * stopped app). The debug build is neither optimised nor compiled ahead of time and is
     * verified as it runs: on the emulator its first activity took 12–24 s to show (measured),
     * more while the PC was busy; the R8 build takes 3–10 s.
     */
    const val APP_START_MS = 60_000L

    /** Connecting or reconnecting to the test server (adb reverse, initialize, first sync). */
    const val CONNECT_MS = 45_000L

    /** A turn of the fake agent, including the streaming scenarios (up to about 20 s of deltas). */
    const val TURN_MS = 90_000L

    /** The system restarting the sticky connection service after its process died (restart delay and a cold start). */
    const val SERVICE_RESTART_MS = 90_000L

    /** Between two looks at the screen. */
    const val POLL_MS = 100L
}

/**
 * The app under test, driven from the test's own process (the test APK instruments itself): UI
 * Automator for the app and the system UI, intents to open it, and shell commands (as the shell
 * user) to reset, stop and kill it. Works the same on the debug and the R8-processed build: it
 * uses no class of the app, only its package name and its string resources (read by name, so the
 * tests follow the app's wording).
 */
class AppDriver(val instrumentation: Instrumentation = InstrumentationRegistry.getInstrumentation()) {
    val device: UiDevice = UiDevice.getInstance(instrumentation)
    val context: Context = instrumentation.context

    /** The app: the build type's target (dev.aas.android or dev.aas.android.staging), or the `appPackage` argument. */
    val appPackage: String = InstrumentationRegistry.getArguments().getString("appPackage") ?: BuildConfig.APP_PACKAGE

    private val appResources: Resources = context.packageManager.getResourcesForApplication(appPackage)

    init {
        // The app animates (progress indicators, the working clock): UI Automator would wait for an
        // idle screen before every lookup. The tests poll instead.
        Configurator.getInstance().waitForIdleTimeout = 0
        Configurator.getInstance().waitForSelectorTimeout = 0
    }

    /** The app's string [name], formatted with [args] (the same words the user sees). */
    fun text(name: String, vararg args: Any): String {
        // By name: the test APK is built separately from the app (and from its R8 output).
        @SuppressLint("DiscouragedApi")
        val id = appResources.getIdentifier(name, "string", appPackage)
        check(id != 0) { "$appPackage has no string resource $name" }
        return appResources.getString(id, *args)
    }

    /** A pattern for the app's string [name] whose first argument (`%1$s`) is anything. */
    fun textPattern(name: String): Pattern {
        val marker = "\u0000"
        val parts = text(name, marker).split(marker)
        return Pattern.compile(parts.joinToString(".*") { Pattern.quote(it) }, Pattern.DOTALL)
    }

    /** A pattern for the app's string [name] formatted with [leading] and then any count (the last argument, `%n$d`). */
    fun textPatternWithCount(name: String, vararg leading: Any): Pattern {
        val parts = text(name, *leading, COUNT_MARKER).split(COUNT_MARKER.toString())
        return Pattern.compile(parts.joinToString("\\d+") { Pattern.quote(it) }, Pattern.DOTALL)
    }

    // --- Shell -----------------------------------------------------------------------------------

    /** Runs [command] as the shell user (no shell syntax: arguments are split on spaces) and returns its output. */
    fun shell(command: String): String {
        val pfd = instrumentation.uiAutomation.executeShellCommand(command)
        return ParcelFileDescriptor.AutoCloseInputStream(pfd).use { it.readBytes().toString(Charsets.UTF_8) }
    }

    /** Clears the app's data (a fresh install: no pairing, no Keystore key, no permissions) and stops it. */
    fun resetApp() {
        val out = shell("pm clear $appPackage").trim()
        check(out == "Success") { "pm clear $appPackage: $out" }
    }

    fun forceStop() {
        shell("am force-stop $appPackage")
    }

    /** The app's process id, or `null` when it does not run. */
    fun pid(): Int? = shell("pidof $appPackage").trim().takeIf { it.isNotEmpty() }?.split(Regex("\\s+"))?.first()?.toInt()

    /**
     * The activity records (the system's `dumpsys activity activities`) that run in the app's
     * process [pid]. Each record names its process as `app=ProcessRecord{<hash> <pid>:<process>/<uid>}`
     * (the app's process is named after its package); records whose process died say `app=null`.
     * Empty: no activity of the app was started in that process.
     */
    fun activitiesIn(pid: Int): List<String> {
        val marker = " $pid:$appPackage/"
        return shell("dumpsys activity activities").lines().filter { it.contains("ProcessRecord{") && it.contains(marker) }.map { it.trim() }
    }

    /**
     * Turns airplane mode on or off (the shell's `cmd connectivity`). With it on the device has no
     * network: the app's connection stays down by its own rule (no default network, no attempts;
     * docs/android.md 11.3), while the test's own channels (adb, adb reverse) keep working. Waits
     * until the setting reads back.
     */
    fun setAirplaneMode(on: Boolean) {
        shell("cmd connectivity airplane-mode ${if (on) "enable" else "disable"}")
        waitUntil("airplane mode ${if (on) "on" else "off"}") { shell("settings get global airplane_mode_on").trim() == if (on) "1" else "0" }
    }

    fun grantNotifications() {
        shell("pm grant $appPackage $POST_NOTIFICATIONS")
    }

    /** Not granted and not decided by the user: the next request shows the system dialog. */
    fun resetNotificationPermission() {
        shell("pm revoke $appPackage $POST_NOTIFICATIONS")
        shell("pm clear-permission-flags $appPackage $POST_NOTIFICATIONS user-set user-fixed")
    }

    fun notificationsGranted(): Boolean = context.packageManager.checkPermission(POST_NOTIFICATIONS, appPackage) == PackageManager.PERMISSION_GRANTED

    // --- Opening the app -----------------------------------------------------------------------

    /**
     * Starts the app from its launcher entry (as the home screen does) and waits until it is in
     * front. A start that a late Home ends up covering (the system handles Home asynchronously
     * and, when busy, after a start that followed it) is repeated: starting the app's existing
     * task again only brings it to the front.
     */
    fun launch() {
        var intent: Intent? = null
        // Right after `pm clear` the package manager may briefly resolve nothing for the package.
        waitUntil("the launcher entry of $appPackage") {
            intent = context.packageManager.getLaunchIntentForPackage(appPackage)
            intent != null
        }
        val launcherIntent = intent ?: throw AssertionError("$appPackage has no launcher entry")
        var startedAt: Long? = null
        waitUntil("$appPackage in front after starting it", Waits.APP_START_MS) {
            if (foregroundPackage() == appPackage) return@waitUntil true
            val now = SystemClock.uptimeMillis()
            if (startedAt.let { it == null || now - it >= LAUNCH_SETTLE_MS }) {
                start(launcherIntent)
                startedAt = now
            }
            false
        }
    }

    /**
     * Presses Home and waits until the home screen is in front. Home is handled asynchronously:
     * without the wait, an app started right after it can be covered by it.
     */
    fun goHome() {
        device.pressHome()
        val launcher = device.launcherPackageName
        waitUntil("the home screen ($launcher) in front") { foregroundPackage() == launcher }
    }

    /** The package of the window in front, as it is now. */
    fun foregroundPackage(): String? {
        refresh()
        return device.currentPackageName
    }

    fun openLink(uri: Uri) = start(Intent(Intent.ACTION_VIEW, uri))

    private fun start(intent: Intent) {
        context.startActivity(intent.setPackage(appPackage).addFlags(Intent.FLAG_ACTIVITY_NEW_TASK))
    }

    // --- Looking at the screen -------------------------------------------------------------------

    /** [selector] within the app's windows. */
    fun inApp(selector: BySelector): BySelector = selector.pkg(appPackage)

    /**
     * Drops UI Automation's cached accessibility nodes, so the next lookup reads the screen as it
     * is now. The cache is refreshed by the accessibility events an app sends, and Compose sends
     * none here: it sends them only while an accessibility service is listed as enabled, and the
     * test's UI Automation connection is not (AccessibilityManager lists no service to the app).
     * Without this, content that changes inside a screen (the `/` palette as it filters, a
     * thread's status in its header) stays as it was when the cache was last filled.
     */
    fun refresh() {
        val automation = instrumentation.uiAutomation
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.UPSIDE_DOWN_CAKE) {
            automation.clearCache()
        } else {
            // Before Android 14 there is no clearCache(); setting the service info clears the
            // connection's cache as a side effect.
            automation.serviceInfo = automation.serviceInfo
        }
    }

    fun find(selector: BySelector): UiObject2? = try {
        refresh()
        device.findObject(selector)
    } catch (e: StaleObjectException) {
        null // the screen changed while it was read: look again
    }

    /** Every element matching [selector], as the screen is now. */
    fun findAll(selector: BySelector): List<UiObject2> = try {
        refresh()
        device.findObjects(selector)
    } catch (e: StaleObjectException) {
        emptyList() // the screen changed while it was read: look again
    }

    fun waitFor(selector: BySelector, what: String, timeoutMs: Long = Waits.SCREEN_MS): UiObject2 {
        var found: UiObject2? = null
        waitUntil(what, timeoutMs) { find(selector).also { found = it } != null }
        return found ?: throw AssertionError("$what disappeared")
    }

    fun waitGone(selector: BySelector, what: String, timeoutMs: Long = Waits.SCREEN_MS) =
        waitUntil("$what to go away", timeoutMs) { find(selector) == null }

    /**
     * Polls [condition] until it holds. A condition that reads an element the screen replaced
     * meanwhile ([StaleObjectException]) counts as not yet holding.
     */
    fun waitUntil(what: String, timeoutMs: Long = Waits.SCREEN_MS, condition: () -> Boolean) {
        val deadline = SystemClock.uptimeMillis() + timeoutMs
        fun holds(): Boolean = try {
            condition()
        } catch (e: StaleObjectException) {
            false
        }
        while (!holds()) {
            if (SystemClock.uptimeMillis() >= deadline) throw AssertionError("$what: not within $timeoutMs ms. On screen: ${screenSummary()}")
            SystemClock.sleep(Waits.POLL_MS)
        }
    }

    /** Waits for [selector] and clicks it (again when the screen recomposed under the click). */
    fun tap(selector: BySelector, what: String, timeoutMs: Long = Waits.SCREEN_MS) {
        waitUntil("tapping $what", timeoutMs) { find(selector)?.click() != null }
    }

    /** Waits for the first of [selectors] that is on screen and clicks it. */
    fun tapFirst(selectors: List<BySelector>, what: String, timeoutMs: Long = Waits.SCREEN_MS) {
        waitUntil("tapping $what", timeoutMs) { selectors.firstNotNullOfOrNull { find(it) }?.click() != null }
    }

    /** A Compose button (a clickable node) labelled [label]. */
    fun button(label: String): BySelector = By.clickable(true).hasChild(By.text(label))

    /** Replaces the text of the input [selector] (a Compose text field takes the accessibility "set text" action). */
    fun setText(selector: BySelector, value: String, what: String) {
        waitUntil("typing into $what", Waits.SCREEN_MS) {
            val field = find(selector) ?: return@waitUntil false
            field.text = value
            find(selector)?.text == value
        }
    }

    /**
     * Drags the app's scrollable container (settings, long lists) upwards until [selector] is on
     * screen. A slow drag, not UiObject2.scroll: that waits for scroll events Compose does not send
     * and reports the end of the list at once.
     */
    fun scrollTo(selector: BySelector, what: String): UiObject2 {
        repeat(MAX_SCROLLS) {
            find(selector)?.let { return it }
            val container = find(inApp(By.scrollable(true))) ?: throw AssertionError("nothing scrolls while looking for $what. On screen: ${screenSummary()}")
            val bounds = container.visibleBounds
            val before = screenSummary(appPackage)
            device.swipe(bounds.centerX(), bounds.top + bounds.height() * 3 / 4, bounds.centerX(), bounds.top + bounds.height() / 4, SCROLL_STEPS)
            // The accessibility tree follows the scroll a little later: the list is at its end only
            // when nothing on screen changed for a while.
            val deadline = SystemClock.uptimeMillis() + SCROLL_SETTLE_MS
            var moved = false
            while (!moved && SystemClock.uptimeMillis() < deadline) {
                find(selector)?.let { return it }
                moved = screenSummary(appPackage) != before
                if (!moved) SystemClock.sleep(Waits.POLL_MS)
            }
            if (!moved) throw AssertionError("$what is not on the screen at the end of the list. On screen: $before")
        }
        throw AssertionError("$what is not on the screen after $MAX_SCROLLS scrolls. On screen: ${screenSummary()}")
    }

    /**
     * Taps [target] until [until] is on screen: for navigation (tabs, rows), where a tap that
     * arrives while the screen still settles is ignored.
     */
    fun tapUntil(target: BySelector, until: BySelector, what: String, timeoutMs: Long = Waits.SCREEN_MS) {
        var taps = 0
        waitUntil("$what (tapping until the next screen shows)", timeoutMs) {
            if (find(until) != null) return@waitUntil true
            try {
                if (find(target)?.click() != null) taps++
            } catch (e: StaleObjectException) {
                // The screen changed under the tap; look again.
            }
            SystemClock.sleep(TAP_SETTLE_MS)
            find(until) != null
        }
        // Kept in the log: a screen that often needs a second tap is worth a look.
        if (taps > 1) Log.w(TAG, "$what took $taps taps")
    }

    /**
     * Taps [target] until it is gone: for system dialogs, which ignore touches for a moment after
     * they appear (a protection against taps meant for the screen below), so a tap as soon as the
     * button shows can be dropped.
     */
    fun tapUntilGone(target: BySelector, what: String, timeoutMs: Long = Waits.SCREEN_MS) {
        var taps = 0
        waitUntil("$what (tapping until it is gone)", timeoutMs) {
            if (find(target)?.click() == null) return@waitUntil taps > 0
            taps++
            SystemClock.sleep(TAP_SETTLE_MS)
            find(target) == null
        }
        if (taps > 1) Log.w(TAG, "$what took $taps taps")
    }

    /**
     * Taps the element [target] of the group (card, row, dialog) that holds [anchor]: the
     * nearest ancestor of the anchor that contains a [target], once it is enabled. A tap on a
     * disabled element is lost (an approval card's buttons arm only after
     * `AppPolicy.interactionArmDelayMs`; tapped as soon as the card showed, the inbox test's
     * answer was lost and the card stayed).
     */
    fun tapNear(anchor: BySelector, target: BySelector, what: String) {
        waitUntil("tapping $what (enabled)", Waits.SCREEN_MS) {
            try {
                var node: UiObject2? = find(anchor)
                var hit: UiObject2? = null
                while (node != null && hit == null) {
                    hit = node.findObject(target)
                    node = node.parent
                }
                hit?.takeIf { it.isEnabled }?.click() != null
            } catch (e: StaleObjectException) {
                false
            }
        }
    }

    /** The texts and descriptions on screen (of [onlyPackage] when given), for failure messages and change checks. */
    fun screenSummary(onlyPackage: String? = null): String {
        val out = ByteArrayOutputStream()
        return try {
            refresh()
            device.dumpWindowHierarchy(out)
            NODE.findAll(out.toString(Charsets.UTF_8.name()))
                .map { it.value }
                .filter { node -> onlyPackage == null || attribute(node, "package") == onlyPackage }
                .flatMap { node -> listOfNotNull(attribute(node, "text"), attribute(node, "content-desc")) }
                .filter { it.isNotEmpty() }
                .distinct()
                .joinToString(" | ")
                .take(SUMMARY_CHARS)
        } catch (e: Exception) {
            "(the window hierarchy could not be read: $e)"
        }
    }

    private fun attribute(node: String, name: String): String? = Regex("\\s${Regex.escape(name)}=\"([^\"]*)\"").find(node)?.groupValues?.get(1)

    private companion object {
        const val TAG = "AasE2e"

        /** One element of UI Automator's window dump. */
        val NODE = Regex("<node [^>]*>")

        /** A count no string of the app contains otherwise: stands for "any count" in patterns. */
        const val COUNT_MARKER = 987_654_321
        const val POST_NOTIFICATIONS = "android.permission.POST_NOTIFICATIONS"

        /** Scroll drags before giving up (a long settings screen needs a few). */
        const val MAX_SCROLLS = 12

        /** Steps of one drag (about 5 ms each): slow enough not to fling. */
        const val SCROLL_STEPS = 40

        /** How long a drag may take to show in the accessibility tree before the list counts as at its end. */
        const val SCROLL_SETTLE_MS = 3_000L

        /** After a navigation tap, how long to let the next screen appear before tapping again. */
        const val TAP_SETTLE_MS = 700L

        /**
         * How long a started app may take to come to the front before it is started again. Its
         * starting window shows within a second even on a cold start; five seconds only pass
         * when something (a late Home) covered it.
         */
        const val LAUNCH_SETTLE_MS = 5_000L

        /** Bound of the screen summary in a failure message (an assertion message is logged whole). */
        const val SUMMARY_CHARS = 6_000
    }
}
