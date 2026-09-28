package dev.aas.android.ui.settings

import android.Manifest
import android.os.Build
import androidx.activity.compose.LocalActivity
import androidx.activity.compose.rememberLauncherForActivityResult
import androidx.activity.result.contract.ActivityResultContracts
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.width
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.outlined.CheckCircle
import androidx.compose.material.icons.outlined.Notifications
import androidx.compose.material3.Button
import androidx.compose.material3.Card
import androidx.compose.material3.Icon
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.OutlinedButton
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.saveable.rememberSaveable
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.graphics.vector.ImageVector
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.res.stringResource
import androidx.compose.ui.unit.dp
import dev.aas.android.R
import dev.aas.android.ui.icons.BatteryAlert
import dev.aas.android.ui.theme.statusColors

/**
 * Asks for the notification permission (Android 13+) after explaining why. When the user has
 * denied it for good (the system no longer shows the dialog), the button opens the app's
 * notification settings instead.
 */
@Composable
fun NotificationPermissionCard(permissions: SystemPermissions, onChanged: () -> Unit) {
    val context = LocalContext.current
    val activity = LocalActivity.current
    var asked by rememberSaveable { mutableStateOf(false) }
    val launcher = rememberLauncherForActivityResult(ActivityResultContracts.RequestPermission()) {
        asked = true
        onChanged()
    }
    PermissionCard(
        icon = Icons.Outlined.Notifications,
        title = stringResource(R.string.setup_notifications_title),
        body = stringResource(R.string.setup_notifications_body),
        done = permissions.notificationsAllowed,
        doneText = stringResource(R.string.setup_notifications_done),
    ) {
        val permanentlyDenied = asked && Build.VERSION.SDK_INT >= Build.VERSION_CODES.TIRAMISU &&
            activity?.shouldShowRequestPermissionRationale(Manifest.permission.POST_NOTIFICATIONS) == false
        if (permissions.notificationPermissionRequired && !permanentlyDenied && Build.VERSION.SDK_INT >= Build.VERSION_CODES.TIRAMISU) {
            Button(onClick = { launcher.launch(Manifest.permission.POST_NOTIFICATIONS) }) { Text(stringResource(R.string.setup_notifications_allow)) }
        } else {
            OutlinedButton(onClick = { SystemIntents.openNotificationSettings(context) }) { Text(stringResource(R.string.open_settings)) }
        }
    }
}

/**
 * Explains the battery-optimisation exemption and opens the system dialog. Without it, Doze
 * cuts the app's network while the screen is off (approvals arrive late) and Android may refuse
 * to restart the connection service in the background.
 */
@Composable
fun BatteryOptimizationCard(permissions: SystemPermissions) {
    val context = LocalContext.current
    PermissionCard(
        icon = Icons.Outlined.BatteryAlert,
        title = stringResource(R.string.setup_battery_title),
        body = stringResource(R.string.setup_battery_body),
        done = permissions.batteryOptimizationIgnored,
        doneText = stringResource(R.string.battery_exempt),
    ) {
        Button(onClick = { SystemIntents.requestIgnoreBatteryOptimizations(context) }) { Text(stringResource(R.string.setup_battery_allow)) }
    }
}

@Composable
private fun PermissionCard(
    icon: ImageVector,
    title: String,
    body: String,
    done: Boolean,
    doneText: String,
    action: @Composable () -> Unit,
) {
    Card(Modifier.fillMaxWidth()) {
        Column(Modifier.padding(16.dp)) {
            Row(verticalAlignment = Alignment.CenterVertically) {
                Icon(icon, contentDescription = null, tint = MaterialTheme.colorScheme.primary)
                Spacer(Modifier.width(12.dp))
                Text(title, style = MaterialTheme.typography.titleMedium)
            }
            Spacer(Modifier.height(8.dp))
            Text(body, style = MaterialTheme.typography.bodyMedium)
            Spacer(Modifier.height(12.dp))
            if (done) {
                Row(verticalAlignment = Alignment.CenterVertically) {
                    Icon(Icons.Outlined.CheckCircle, contentDescription = null, tint = MaterialTheme.statusColors.connected)
                    Spacer(Modifier.width(8.dp))
                    Text(doneText, style = MaterialTheme.typography.labelLarge)
                }
            } else {
                action()
            }
        }
    }
}
