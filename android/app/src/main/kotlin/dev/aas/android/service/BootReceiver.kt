package dev.aas.android.service

import android.content.BroadcastReceiver
import android.content.Context
import android.content.Intent
import dev.aas.android.appContainer
import dev.aas.android.diagnostics.ConnectionLog
import dev.aas.android.security.hasPairing
import kotlinx.coroutines.CancellationException
import kotlinx.coroutines.launch

/**
 * Starts the connection service after a reboot (`BOOT_COMPLETED`) and after an app update
 * (`MY_PACKAGE_REPLACED`) when the device is paired. Both broadcasts are exempt from the
 * background foreground-service start restriction (Android 12+), and `specialUse` is not among
 * the types Android 15+ forbids to start from `BOOT_COMPLETED` (dataSync, camera, mediaPlayback,
 * phoneCall, mediaProjection, microphone).
 */
class BootReceiver : BroadcastReceiver() {
    override fun onReceive(context: Context, intent: Intent) {
        val reason = when (intent.action) {
            Intent.ACTION_BOOT_COMPLETED -> StartReason.Boot
            Intent.ACTION_MY_PACKAGE_REPLACED -> StartReason.PackageReplaced
            else -> return
        }
        val container = context.appContainer
        val pending = goAsync()
        container.applicationScope.launch {
            try {
                // A pairing waiting for the keystore counts: the service runs while it is retried.
                if (container.credentialStore.current().hasPairing) {
                    container.connectionLog.info(ConnectionLog.SOURCE_SERVICE, "starting after $reason")
                    container.connectionController.requestStart(reason)
                }
            } catch (e: CancellationException) {
                throw e
            } catch (e: Exception) {
                container.connectionLog.warn(ConnectionLog.SOURCE_SERVICE, "reading the pairing after $reason failed", e)
            } finally {
                pending.finish()
            }
        }
    }
}
