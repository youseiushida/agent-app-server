package dev.aas.android.service

import android.app.ForegroundServiceStartNotAllowedException
import android.app.Service
import android.content.Intent
import android.content.pm.ServiceInfo
import android.net.ConnectivityManager
import android.os.Build
import android.os.IBinder
import androidx.core.app.NotificationManagerCompat
import androidx.core.app.ServiceCompat
import dev.aas.android.AppContainer
import dev.aas.android.appContainer
import dev.aas.android.diagnostics.ConnectionLog
import dev.aas.android.notify.Notifier
import dev.aas.android.security.hasPairing
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.SupervisorJob
import kotlinx.coroutines.cancel
import kotlinx.coroutines.flow.combine
import kotlinx.coroutines.flow.distinctUntilChanged
import kotlinx.coroutines.flow.map
import kotlinx.coroutines.launch

/**
 * The foreground service (type `specialUse`) that keeps the connection to the daemon while the
 * device is paired. The [dev.aas.android.sync.SyncEngine] is created once per process (so the UI
 * and notification actions can use it at any time); this service owns its *running* lifetime:
 *
 * * `onStartCommand`: promote to foreground at once, then register the default-network
 *   callback, `engine.start()` (after the callback: no attempt before the engine knows whether
 *   there is a network), and collect the engine's signals and outbox results into
 *   notifications. `START_STICKY`: the system recreates the service after killing it.
 * * `onDestroy`: stop the engine and unregister everything.
 * * Unpairing stops the service; without a pairing it stops itself.
 *
 * Start rules (Android 12–16) live in [ConnectionController]; if the system refuses to promote a
 * restarted service in the background (possible without the battery-optimisation exemption),
 * the service stops itself and the app starts it again when it comes to the foreground.
 */
class ConnectionService : Service() {
    private lateinit var container: AppContainer
    private val scope = CoroutineScope(SupervisorJob() + Dispatchers.Main.immediate)
    private var networkMonitor: NetworkMonitor? = null
    private var running = false

    override fun onCreate() {
        super.onCreate()
        container = appContainer
        container.connectionController.onServiceCreated()
        container.connectionLog.info(ConnectionLog.SOURCE_SERVICE, "service created")
    }

    override fun onStartCommand(intent: Intent?, flags: Int, startId: Int): Int {
        if (!promoteToForeground()) {
            stopSelf()
            return START_NOT_STICKY
        }
        if (!running) {
            running = true
            startConnection()
        }
        if (intent?.action == ACTION_RECONNECT) {
            container.connectionLog.info(ConnectionLog.SOURCE_SERVICE, "reconnect requested from the notification")
            // Also retries a keystore that failed to decrypt the token, and takes the connection
            // back after "connected elsewhere" (a user action, so no fight over the token).
            container.reconnectNow()
        }
        return START_STICKY
    }

    /** `startForeground` with the current connection text; false when the system refuses. */
    private fun promoteToForeground(): Boolean {
        val notification = container.notifier.connectionNotification(
            ConnectionPresentation.of(container.engine.status.value, container.pairingState.value),
            container.pairedServerName.value,
        )
        return try {
            // The specialUse type exists from Android 14; before that a foreground service has no
            // type to declare (the manifest's value is ignored there).
            val type = if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.UPSIDE_DOWN_CAKE) ServiceInfo.FOREGROUND_SERVICE_TYPE_SPECIAL_USE else 0
            ServiceCompat.startForeground(this, Notifier.ID_CONNECTION, notification, type)
            true
        } catch (e: IllegalStateException) {
            val refused = Build.VERSION.SDK_INT >= Build.VERSION_CODES.S && e is ForegroundServiceStartNotAllowedException
            container.connectionLog.warn(
                ConnectionLog.SOURCE_SERVICE,
                if (refused) "the system refused the foreground promotion (background restart)" else "startForeground failed",
                e,
            )
            false
        }
    }

    private fun startConnection() {
        val engine = container.engine
        // The network state first: started without a network, the engine must know before its
        // first attempt (it would otherwise try once on whatever route is left, e.g. loopback).
        val monitor = NetworkMonitor(getSystemService(ConnectivityManager::class.java), engine, container.connectionLog)
        monitor.start()
        networkMonitor = monitor
        engine.start()

        // The persistent notification follows the connection state (only when its text changes).
        scope.launch {
            combine(
                combine(engine.status, container.pairingState) { status, pairing -> ConnectionPresentation.of(status, pairing) }.distinctUntilChanged(),
                container.pairedServerName,
            ) { p, name -> p to name }
                .collect { (presentation, name) ->
                    val notification = container.notifier.connectionNotification(presentation, name)
                    if (container.notifier.canPost()) {
                        try {
                            NotificationManagerCompat.from(this@ConnectionService).notify(Notifier.ID_CONNECTION, notification)
                        } catch (e: SecurityException) {
                            container.connectionLog.warn(ConnectionLog.SOURCE_NOTIFY, "updating the connection notification failed", e)
                        }
                    }
                }
        }
        scope.launch {
            // "Connected elsewhere" stops the connection until the user acts; in the background
            // nothing else would tell them (the banner is in the app).
            combine(
                combine(engine.status, container.pairingState) { status, pairing -> ConnectionPresentation.of(status, pairing).summary }
                    .map { it == ConnectionSummary.ConnectedElsewhere }
                    .distinctUntilChanged(),
                container.visibility.appInForeground,
                container.pairedServerName,
            ) { elsewhere, foreground, name -> Triple(elsewhere, foreground, name) }
                .collect { (elsewhere, foreground, name) ->
                    if (elsewhere && !foreground) {
                        container.connectionLog.info(ConnectionLog.SOURCE_NOTIFY, "connected elsewhere while in the background: notifying")
                        container.notifier.showConnectedElsewhere(name)
                    } else {
                        container.notifier.cancelConnectedElsewhere()
                    }
                }
        }
        scope.launch { engine.signals.collect { container.notifier.onSignal(it) } }
        scope.launch { engine.results.collect { container.notifier.onResult(it) } }
        scope.launch {
            // Only when the set of pending interactions changes (not on every workspace update).
            engine.workspace
                .map { workspace -> if (workspace.synced) workspace.pendingInteractions.map { it.id }.toSet() else null }
                .distinctUntilChanged()
                .collect { pending -> if (pending != null) container.notifier.reconcileInteractions(pending) }
        }
        scope.launch {
            // Only a pairing that is gone (or can never be decrypted) ends the service; one whose
            // token waits for the keystore keeps it running while the credential store retries.
            container.pairingState.collect { state ->
                if (state != null && !state.hasPairing) {
                    container.connectionLog.info(ConnectionLog.SOURCE_SERVICE, "not paired: stopping")
                    stopSelf()
                }
            }
        }
    }

    override fun onDestroy() {
        scope.cancel()
        networkMonitor?.stop()
        networkMonitor = null
        if (running) container.engine.stop()
        running = false
        container.connectionController.onServiceDestroyed()
        container.connectionLog.info(ConnectionLog.SOURCE_SERVICE, "service destroyed")
        super.onDestroy()
    }

    override fun onBind(intent: Intent?): IBinder? = null

    companion object {
        const val ACTION_START = "dev.aas.android.action.START_CONNECTION"
        const val ACTION_RECONNECT = "dev.aas.android.action.RECONNECT"
    }
}
