package dev.aas.android.e2e

import android.net.Uri
import android.os.SystemClock
import android.widget.EditText
import androidx.test.uiautomator.By
import androidx.test.uiautomator.BySelector
import androidx.test.uiautomator.Direction
import java.net.URLEncoder
import java.util.regex.Pattern

/**
 * What a user does in the app, step by step, against the real daemon: pairing, the new-project
 * and new-thread flows, sending messages, the connection strip, the notification shade.
 */
class AppFlows(val app: AppDriver, val control: HostControl, val server: ServerArgs) {
    // --- Pairing --------------------------------------------------------------------------------

    /** Opens the `aas://pair` link with a fresh code (as a camera app would) and pairs as [deviceName]. */
    fun pairWithLink(deviceName: String) {
        val code = control.pairingCode()
        app.openLink(pairingLink(server.wsUrl, code, SERVER_NAME))
        app.waitFor(app.inApp(By.text(app.text("pairing_confirm_title"))), "the pairing confirmation", Waits.APP_START_MS)
        // The device name is the confirmation's only input.
        app.setText(app.inApp(By.clazz(EditText::class.java.name)), deviceName, "the device name")
        app.tap(app.inApp(app.button(app.text("pairing_pair"))), "ペアリング")
    }

    /** After the first pairing: leaves the setup screen as it is (あとで / 完了) and waits for the projects. */
    fun completeSetup() {
        app.waitFor(app.inApp(By.text(app.text("setup_title"))), "the setup screen")
        val finish = Pattern.compile("${Pattern.quote(app.text("setup_later"))}|${Pattern.quote(app.text("done"))}")
        app.tap(app.inApp(By.clickable(true).hasChild(By.text(finish))), "あとで / 完了")
        waitForProjects()
    }

    /** Pairs a fresh app (link, setup) and waits until it is connected. */
    fun pairFresh(deviceName: String) {
        pairWithLink(deviceName)
        completeSetup()
        waitConnected()
    }

    /** The 設定 tab. */
    fun openSettings() {
        app.tapUntil(app.inApp(By.text(app.text("tab_settings"))), app.inApp(By.text(app.text("settings_server"))), "opening 設定")
    }

    /** 設定 → デバイス, until the daemon's device list shows [thisDevice] as this device. */
    fun openDevices(thisDevice: String) {
        openSettings()
        app.scrollTo(app.inApp(By.text(app.text("settings_devices"))), "デバイス")
        // A tap while the settings still settle after the scroll is lost: tap until the list shows.
        app.tapUntil(
            app.inApp(By.text(app.text("settings_devices"))),
            app.inApp(By.text(app.text("devices_this_device", thisDevice))),
            "opening デバイス with this device ($thisDevice) in the daemon's list",
        )
    }

    /** The 要対応 tab (its accessible name carries the badge's count when there is one). */
    fun openInbox() {
        val tab = By.desc(app.textPatternWithCount("tab_badge", app.text("tab_inbox")))
        app.tapUntil(app.inApp(tab), app.inApp(By.text(app.textPatternWithCount("inbox_section_waiting"))), "opening 要対応 with its badge")
    }

    fun waitForProjects(timeoutMs: Long = Waits.SCREEN_MS) {
        app.waitFor(app.inApp(By.desc(app.text("project_new"))), "the projects screen", timeoutMs)
    }

    // --- The connection strip -------------------------------------------------------------------

    /** Every title the connection strip can show (ConnectionTexts.title). */
    private val connectionTitles: Set<String> by lazy {
        listOf(
            "connection_connected", "connection_connecting", "connection_reconnecting", "connection_unreachable",
            "connection_server_restarting", "connection_server_stopped", "connection_protocol_error", "connection_phone_offline",
            "connection_elsewhere", "connection_not_paired", "connection_revoked", "connection_token_rejected",
            "connection_invalid_url", "connection_incompatible", "connection_stopped", "connection_keystore_unavailable",
        ).map { app.text(it) }.toSet()
    }

    /** Titles of an app that lost the connection and is getting it back by itself. */
    val reconnectingTitles: Set<String> by lazy {
        listOf(
            "connection_connecting", "connection_reconnecting", "connection_unreachable",
            "connection_server_restarting", "connection_server_stopped",
        ).map { app.text(it) }.toSet()
    }

    val connectedTitle: String by lazy { app.text("connection_connected") }

    /** The connection strip's title: the topmost connection title on screen (settings repeat it lower down). */
    fun connectionTitle(): String? {
        val pattern = Pattern.compile(connectionTitles.joinToString("|") { Pattern.quote(it) })
        return try {
            app.findAll(app.inApp(By.text(pattern))).minByOrNull { it.visibleBounds.top }?.text
        } catch (e: androidx.test.uiautomator.StaleObjectException) {
            null
        }
    }

    fun waitConnected(timeoutMs: Long = Waits.CONNECT_MS) =
        app.waitUntil("the connection strip to say $connectedTitle", timeoutMs) { connectionTitle() == connectedTitle }

    /** Waits until the strip leaves 接続済み and returns what it says then. */
    fun waitDisconnected(timeoutMs: Long): String {
        var title: String? = null
        app.waitUntil("the connection strip to leave $connectedTitle", timeoutMs) {
            title = connectionTitle()
            title != null && title != connectedTitle
        }
        return title ?: throw AssertionError("the connection strip disappeared")
    }

    // --- Projects and threads -------------------------------------------------------------------

    /**
     * The new-project flow (最初から始める → git init → name → the project root → ここに作成),
     * ending on the new-thread screen of the created project. [shots] names the screenshots.
     */
    fun createProject(name: String, shots: String? = null) {
        app.tap(app.inApp(By.desc(app.text("project_new"))), "新しいプロジェクト")
        app.waitFor(app.inApp(By.text(app.text("newproject_existing"))), "the new-project choices")
        shots?.let { control.screenshot("$it-choose") }
        app.tap(app.inApp(By.text(app.text("newproject_new"))), "最初から始める")
        app.tap(app.inApp(By.text(app.text("newproject_kind_git_init"))), "git init")
        app.setText(app.inApp(By.clazz(EditText::class.java.name)), name, "the folder name")
        shots?.let { control.screenshot("$it-details") }
        app.tap(app.inApp(app.button(app.text("newproject_choose_location"))), "置き場所を選ぶ")
        // The roots list shows each root's path; open the server's root and create the folder there.
        app.tap(app.inApp(By.text(server.root)), "the project root ${server.root}")
        app.waitFor(app.inApp(app.button(app.text("newproject_create_here"))), "ここに作成")
        shots?.let { control.screenshot("$it-location") }
        // Tapping replaces the button with the progress at once: a second tap only happens when
        // the first one was lost.
        app.tapUntil(
            app.inApp(app.button(app.text("newproject_create_here"))),
            app.inApp(By.text(app.text("newthread_title"))),
            "ここに作成",
        )
        app.waitFor(app.inApp(By.text(name)), "the new project's name on the new-thread screen")
    }

    /**
     * The new-project flow for a folder that exists on the PC (既存のフォルダーを使用 → パスを入力
     * → 開く → このフォルダーを開く), ending on the new-thread screen of the project it registers
     * (a folder without threads). [folder] is relative to the server's root; the project is named
     * after it.
     */
    fun openFolderAsProject(folder: String) {
        app.tap(app.inApp(By.desc(app.text("project_new"))), "新しいプロジェクト")
        app.tap(app.inApp(By.text(app.text("newproject_existing"))), "既存のフォルダーを使用")
        app.tap(app.inApp(By.text(app.text("newproject_type_path"))), "パスを入力")
        val separator = if (server.root.endsWith("\\")) "" else "\\"
        app.setText(app.inApp(By.clazz(EditText::class.java.name)), server.root + separator + folder, "the folder's path")
        app.tap(app.inApp(app.button(app.text("newproject_go"))), "開く")
        // Tapping replaces the button with the progress at once: a second tap only happens when
        // the first one was lost.
        app.tapUntil(
            app.inApp(app.button(app.text("newproject_open_this"))),
            app.inApp(By.text(app.text("newthread_title"))),
            "このフォルダーを開く",
        )
        app.waitFor(app.inApp(By.text(folder)), "the project's name on the new-thread screen")
    }

    /**
     * Runs the app's `/resume` from the composer's palette (typed partly, so the palette row is
     * the only "/resume" on screen) and waits for 「PC のセッションを取り込む」.
     */
    fun resumeFromThePalette() {
        app.setText(composer, "/resu", "the composer")
        app.tap(app.inApp(By.text("/resume")), "/resume in the palette")
        app.waitFor(app.inApp(By.text(app.text("import_session"))), "PC のセッションを取り込む")
    }

    /** The composer (the only text field of the thread and new-thread screens). */
    val composer: BySelector get() = app.inApp(By.clazz(EditText::class.java.name))

    /** Types [prompt] into the composer and sends it; waits for its bubble. */
    fun send(prompt: String) {
        app.setText(composer, prompt, "the composer")
        app.tap(app.inApp(By.desc(app.text("composer_send"))), "送信")
        app.waitUntil("the composer to empty after sending") { app.find(composer)?.text != prompt }
        app.waitFor(app.inApp(By.text(prompt).clazz(TEXT_VIEW)), "the sent message")
    }

    /** The whole flow: a new project with a new thread whose first message is [prompt]. */
    fun newProjectAndThread(name: String, prompt: String) {
        createProject(name)
        send(prompt)
    }

    /** The agent message of `@stream <count>`: `tok0 tok1 … tok<count-1>`. */
    fun streamed(count: Int): String = (0 until count).joinToString(" ") { "tok$it" }

    /** Waits for the full agent message of `@stream <count>`, exactly once. */
    fun waitForStream(count: Int, timeoutMs: Long = Waits.TURN_MS) {
        val expected = streamed(count)
        app.waitFor(app.inApp(By.text(expected)), "the whole streamed message (tok0 … tok${count - 1})", timeoutMs)
        val copies = app.findAll(app.inApp(By.textStartsWith("tok0 "))).size
        if (copies != 1) throw AssertionError("the streamed message is shown $copies times")
    }

    /** Waits until the streaming of `@stream` has visibly started. */
    fun waitForStreamStart() {
        app.waitFor(app.inApp(By.textStartsWith("tok0 tok1 tok2")), "the first streamed tokens", Waits.TURN_MS)
    }

    // --- Notifications --------------------------------------------------------------------------

    /**
     * Opens the notification shade and taps [action] on the notification titled [title]
     * (expanding it when its actions are folded away).
     */
    fun tapNotificationAction(title: String, action: String, shot: String? = null) {
        app.device.openNotification()
        val titleSelector = By.text(title)
        app.waitFor(titleSelector, "the notification '$title'", Waits.TURN_MS)
        val actionSelector = By.text(action)
        if (app.find(actionSelector) == null) {
            app.find(titleSelector)?.swipe(Direction.DOWN, EXPAND_SWIPE_PERCENT)
        }
        shot?.let { control.screenshot(it) }
        app.tap(actionSelector, "'$action' on the notification")
    }

    /** Whether a notification titled [title] is in the shade (opens it). */
    fun notificationShown(title: String): Boolean {
        app.device.openNotification()
        return app.find(By.text(title)) != null
    }

    fun closeNotificationShade() {
        app.device.pressBack()
        SystemClock.sleep(Waits.POLL_MS)
    }

    companion object {
        /** The server name in the pairing links (the app shows it until the daemon's own name arrives). */
        const val SERVER_NAME = "e2e-pc"

        /** The class Compose reports for text (as opposed to the text fields, EditText). */
        const val TEXT_VIEW = "android.widget.TextView"

        /** Dragging a notification's title down expands it (shows its actions). */
        private const val EXPAND_SWIPE_PERCENT = 1f

        fun pairingLink(wsUrl: String, code: String, name: String): Uri {
            fun enc(s: String) = URLEncoder.encode(s, Charsets.UTF_8.name())
            return Uri.parse("aas://pair?u=${enc(wsUrl)}&c=${enc(code)}&n=${enc(name)}")
        }

        /** A name no earlier test (on the same server) used. */
        fun unique(prefix: String): String = "$prefix-${java.lang.Long.toString(System.currentTimeMillis(), Character.MAX_RADIX)}"
    }
}
