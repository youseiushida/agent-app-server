package dev.aas.android.ui.settings

import android.Manifest
import android.annotation.SuppressLint
import android.content.ActivityNotFoundException
import android.content.Context
import android.content.Intent
import android.content.pm.PackageManager
import android.net.Uri
import android.os.Build
import android.os.PowerManager
import android.provider.Settings
import androidx.core.app.NotificationManagerCompat
import androidx.core.content.ContextCompat

/** What the system currently allows the app (re-read whenever a screen resumes). */
data class SystemPermissions(
    /** Notifications can be shown: the runtime permission (13+) is granted and the app is not blocked. */
    val notificationsAllowed: Boolean,
    /** Android 13+: the runtime permission must be requested. */
    val notificationPermissionRequired: Boolean,
    /** The app is exempt from battery optimisation (Doze does not cut its network). */
    val batteryOptimizationIgnored: Boolean,
) {
    companion object {
        fun read(context: Context): SystemPermissions {
            val permissionRequired = Build.VERSION.SDK_INT >= Build.VERSION_CODES.TIRAMISU
            val granted = !permissionRequired ||
                ContextCompat.checkSelfPermission(context, Manifest.permission.POST_NOTIFICATIONS) == PackageManager.PERMISSION_GRANTED
            val power = context.getSystemService(PowerManager::class.java)
            return SystemPermissions(
                notificationsAllowed = granted && NotificationManagerCompat.from(context).areNotificationsEnabled(),
                notificationPermissionRequired = permissionRequired && !granted,
                batteryOptimizationIgnored = power.isIgnoringBatteryOptimizations(context.packageName),
            )
        }
    }
}

/** Intents into the system settings. Each returns false when the device has no such screen. */
object SystemIntents {
    /** The app's notification settings (channels, the global switch). */
    fun openNotificationSettings(context: Context): Boolean = start(
        context,
        Intent(Settings.ACTION_APP_NOTIFICATION_SETTINGS).putExtra(Settings.EXTRA_APP_PACKAGE, context.packageName),
    ) || openAppDetails(context)

    /**
     * The system dialog "stop optimising battery usage?" for this app. Allowed because the app
     * is sideloaded for one user whose core function (a persistent connection) needs it; the
     * permission `REQUEST_IGNORE_BATTERY_OPTIMIZATIONS` is declared for this (docs/android.md).
     */
    @SuppressLint("BatteryLife")
    fun requestIgnoreBatteryOptimizations(context: Context): Boolean = start(
        context,
        Intent(Settings.ACTION_REQUEST_IGNORE_BATTERY_OPTIMIZATIONS, Uri.fromParts("package", context.packageName, null)),
    ) || openBatteryOptimizationList(context)

    /** The list of apps and their battery optimisation (to undo the exemption). */
    fun openBatteryOptimizationList(context: Context): Boolean =
        start(context, Intent(Settings.ACTION_IGNORE_BATTERY_OPTIMIZATION_SETTINGS)) || openAppDetails(context)

    fun openAppDetails(context: Context): Boolean =
        start(context, Intent(Settings.ACTION_APPLICATION_DETAILS_SETTINGS, Uri.fromParts("package", context.packageName, null)))

    private fun start(context: Context, intent: Intent): Boolean = try {
        context.startActivity(intent.addFlags(Intent.FLAG_ACTIVITY_NEW_TASK))
        true
    } catch (e: ActivityNotFoundException) {
        // A vendor build without this settings screen: the caller falls back to another one.
        false
    }
}
