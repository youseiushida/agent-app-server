package dev.aas.android.service

import android.app.ForegroundServiceStartNotAllowedException
import android.content.Context
import android.content.Intent
import android.os.Build
import androidx.core.content.ContextCompat
import dev.aas.android.diagnostics.ConnectionLog
import dev.aas.android.sync.Clock
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.flow.asStateFlow

/** Why the app asks for the connection service. */
enum class StartReason {
    /** An activity of the app came to the foreground (always allowed). */
    AppForeground,

    /** A pairing just completed. */
    Paired,

    /** `BOOT_COMPLETED` (allowed for `specialUse`; see docs/android.md). */
    Boot,

    /** `MY_PACKAGE_REPLACED`: the app was updated. */
    PackageReplaced,

    /** A notification action (許可 / 拒否) queued a request. */
    NotificationAction,

    /** The user pressed 再接続. */
    UserAction,
}

/** A start request the system refused (shown in diagnostics). */
data class StartFailure(val atMs: Long, val reason: StartReason, val message: String)

/** Starts and stops [ConnectionService]; an interface so receivers can be tested without a service. */
interface ServiceStarter {
    /** Asks for the service to run; returns whether the system accepted the request. */
    fun requestStart(reason: StartReason): Boolean

    /** Stops the service (unpairing). */
    fun stop()
}

/**
 * Knows whether the connection service runs and starts it within Android's rules for
 * foreground services started from the background (Android 12+):
 *
 * * From an activity, from a notification action and after `BOOT_COMPLETED` /
 *   `MY_PACKAGE_REPLACED` the start is exempt from the background restriction.
 * * Anything else in the background is only allowed while the app is exempt from battery
 *   optimisation; a refused start is recorded in [lastStartFailure] (and the log) and retried
 *   the next time the app comes to the foreground.
 */
class ConnectionController(
    private val context: Context,
    private val log: ConnectionLog,
    private val clock: Clock = Clock.System,
) : ServiceStarter {
    private val _serviceRunning = MutableStateFlow(false)

    /** The service is between `onCreate` and `onDestroy`. */
    val serviceRunning: StateFlow<Boolean> = _serviceRunning.asStateFlow()

    private val _lastStartFailure = MutableStateFlow<StartFailure?>(null)
    val lastStartFailure: StateFlow<StartFailure?> = _lastStartFailure.asStateFlow()

    override fun requestStart(reason: StartReason): Boolean {
        val intent = Intent(context, ConnectionService::class.java).setAction(ConnectionService.ACTION_START)
        return try {
            ContextCompat.startForegroundService(context, intent)
            _lastStartFailure.value = null
            true
        } catch (e: IllegalStateException) {
            // ForegroundServiceStartNotAllowedException (API 31+) is an IllegalStateException.
            val refused = Build.VERSION.SDK_INT >= Build.VERSION_CODES.S && e is ForegroundServiceStartNotAllowedException
            val message = if (refused) "the system refused to start the service from the background" else e.message ?: e.javaClass.simpleName
            _lastStartFailure.value = StartFailure(clock.nowMs(), reason, message)
            log.warn(ConnectionLog.SOURCE_SERVICE, "start ($reason) refused: $message", e)
            false
        }
    }

    override fun stop() {
        context.stopService(Intent(context, ConnectionService::class.java))
    }

    internal fun onServiceCreated() {
        _serviceRunning.value = true
    }

    internal fun onServiceDestroyed() {
        _serviceRunning.value = false
    }
}
