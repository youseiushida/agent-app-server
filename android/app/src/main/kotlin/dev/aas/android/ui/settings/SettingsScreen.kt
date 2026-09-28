package dev.aas.android.ui.settings

import androidx.compose.foundation.clickable
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.selection.selectable
import androidx.compose.foundation.verticalScroll
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.automirrored.outlined.KeyboardArrowRight
import androidx.compose.material3.ExperimentalMaterial3Api
import androidx.compose.material3.HorizontalDivider
import androidx.compose.material3.Icon
import androidx.compose.material3.ListItem
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.OutlinedButton
import androidx.compose.material3.RadioButton
import androidx.compose.material3.Scaffold
import androidx.compose.material3.Switch
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.material3.TopAppBar
import androidx.compose.runtime.Composable
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.saveable.rememberSaveable
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.platform.LocalResources
import androidx.compose.ui.res.stringResource
import androidx.compose.ui.semantics.Role
import androidx.compose.ui.unit.dp
import androidx.lifecycle.compose.LifecycleResumeEffect
import androidx.lifecycle.compose.collectAsStateWithLifecycle
import androidx.navigation.NavGraphBuilder
import androidx.navigation.compose.composable
import dev.aas.android.R
import dev.aas.android.domain.HarnessState
import dev.aas.android.domain.InteractionTexts
import dev.aas.android.domain.composer.FollowUpDelivery
import dev.aas.android.security.info
import dev.aas.android.service.ConnectionPresentation
import dev.aas.android.service.ConnectionTexts
import dev.aas.android.settings.TurnNotificationMode
import dev.aas.android.ui.common.aasViewModel
import dev.aas.android.ui.common.asString
import dev.aas.android.ui.components.ConfirmDialog
import dev.aas.android.ui.components.HarnessStatusRow
import dev.aas.android.ui.components.SectionHeader
import dev.aas.android.ui.components.relativeTime
import dev.aas.android.ui.navigation.AppNavigator
import dev.aas.android.ui.navigation.BatteryRoute
import dev.aas.android.ui.navigation.DevicesRoute
import dev.aas.android.ui.navigation.DiagnosticsRoute
import dev.aas.android.ui.navigation.SettingsRoute

fun NavGraphBuilder.settingsDestinations(navigator: AppNavigator) {
    composable<SettingsRoute> { SettingsScreen(aasViewModel { c, _ -> SettingsViewModel(c) }, navigator) }
    composable<DevicesRoute> { DevicesScreen(aasViewModel { c, _ -> DevicesViewModel(c) }, navigator) }
    composable<DiagnosticsRoute> { DiagnosticsScreen(aasViewModel { c, _ -> DiagnosticsViewModel(c) }, navigator) }
    composable<BatteryRoute> { BatteryScreen(onDone = navigator::back, onBack = navigator::back) }
}

/** Re-reads the system permissions every time the screen resumes (the user may change them). */
@Composable
fun rememberSystemPermissions(): SystemPermissions {
    val context = LocalContext.current
    var permissions by remember { mutableStateOf(SystemPermissions.read(context)) }
    LifecycleResumeEffect(Unit) {
        permissions = SystemPermissions.read(context)
        onPauseOrDispose { }
    }
    return permissions
}

/**
 * 設定: server, this device, notifications, battery, diagnostics, app version (UX §8.1:
 * devices list/revoke, the daemon's sleep policy).
 */
@OptIn(ExperimentalMaterial3Api::class)
@Composable
fun SettingsScreen(vm: SettingsViewModel, navigator: AppNavigator) {
    val ui by vm.state.collectAsStateWithLifecycle()
    val context = LocalContext.current
    val res = LocalResources.current
    val permissions = rememberSystemPermissions()
    var confirmUnpair by rememberSaveable { mutableStateOf(false) }
    LaunchedEffect(Unit) { vm.refreshServerStatus() }
    // After unpairing, the shell replaces everything with the pairing screen (AasApp).

    Scaffold(topBar = { TopAppBar(title = { Text(stringResource(R.string.tab_settings)) }) }) { padding ->
        Column(Modifier.fillMaxSize().padding(padding).verticalScroll(rememberScrollState())) {
            // ----- server ----------------------------------------------------------------------
            SectionHeader(stringResource(R.string.settings_server))
            val info = ui.pairing?.info
            val server = ui.status.server
            val presentation = ConnectionPresentation.of(ui.status, ui.pairing)
            Info(stringResource(R.string.settings_server_name), server?.name ?: info?.serverName ?: stringResource(R.string.unknown))
            Info(stringResource(R.string.settings_server_url), info?.wsUrl ?: stringResource(R.string.unknown))
            Info(stringResource(R.string.settings_connection), ConnectionTexts.title(res, presentation.summary))
            server?.let {
                Info(stringResource(R.string.settings_server_version), it.version)
                Info(stringResource(R.string.settings_server_host), it.hostname)
            }
            when (val status = ui.serverStatus) {
                is Remote.Loaded -> {
                    Info(stringResource(R.string.settings_uptime), InteractionTexts.duration(res, status.value.uptimeMs))
                    Info(stringResource(R.string.settings_running), stringResource(R.string.settings_running_value, status.value.runningTurns, status.value.runningProcesses))
                    Info(
                        stringResource(R.string.settings_prevent_sleep),
                        stringResource(if (status.value.preventSleepWhileRunning) R.string.on else R.string.off),
                        supporting = stringResource(R.string.settings_prevent_sleep_note),
                    )
                    if (status.value.draining) Info(stringResource(R.string.settings_draining), stringResource(R.string.settings_draining_value))
                }
                is Remote.Failed -> Info(stringResource(R.string.settings_server_status), status.message.asString())
                Remote.Loading -> Info(stringResource(R.string.settings_server_status), stringResource(R.string.loading))
                Remote.Idle -> Unit
            }
            Row(Modifier.padding(horizontal = 8.dp)) {
                TextButton(onClick = vm::refreshServerStatus) { Text(stringResource(R.string.refresh)) }
                TextButton(onClick = vm::reconnectNow) { Text(stringResource(R.string.action_reconnect)) }
            }
            Link(stringResource(R.string.settings_devices), stringResource(R.string.settings_devices_summary)) { navigator.openDevices() }

            // ----- harnesses --------------------------------------------------------------------
            HorizontalDivider()
            SectionHeader(stringResource(R.string.settings_harnesses)) {
                TextButton(onClick = { vm.refreshHarness(null) }, enabled = ui.harnesses.isNotEmpty()) { Text(stringResource(R.string.harness_refresh_all)) }
            }
            if (ui.harnesses.isEmpty()) {
                Text(
                    stringResource(R.string.settings_harnesses_empty),
                    style = MaterialTheme.typography.bodyMedium,
                    color = MaterialTheme.colorScheme.onSurfaceVariant,
                    modifier = Modifier.padding(horizontal = 16.dp, vertical = 8.dp),
                )
            }
            ui.harnesses.forEach { harness -> HarnessStatusRow(harness, HarnessState.of(harness, ui.probing)) { vm.refreshHarness(harness.id) } }
            if (ui.harnesses.any { !it.available }) {
                Text(
                    stringResource(R.string.settings_harnesses_note),
                    style = MaterialTheme.typography.bodySmall,
                    color = MaterialTheme.colorScheme.onSurfaceVariant,
                    modifier = Modifier.padding(horizontal = 16.dp, vertical = 4.dp),
                )
            }

            // ----- this device ------------------------------------------------------------------
            HorizontalDivider()
            SectionHeader(stringResource(R.string.settings_this_device))
            Info(stringResource(R.string.settings_device_name), info?.deviceName ?: stringResource(R.string.unknown))
            Info(stringResource(R.string.settings_device_id), ui.status.deviceId ?: info?.deviceId ?: stringResource(R.string.unknown))
            info?.pairedAtMs?.takeIf { it > 0 }?.let { Info(stringResource(R.string.settings_paired_at), relativeTime(it)) }
            Row(Modifier.padding(horizontal = 8.dp)) {
                TextButton(onClick = { navigator.startPairing(repair = true) }) { Text(stringResource(R.string.settings_pair_again)) }
                TextButton(onClick = { confirmUnpair = true }, enabled = !ui.unpairing) { Text(stringResource(R.string.settings_unpair)) }
            }

            // ----- notifications -------------------------------------------------------------
            HorizontalDivider()
            SectionHeader(stringResource(R.string.settings_notifications))
            if (!permissions.notificationsAllowed) {
                ListItem(
                    headlineContent = { Text(stringResource(R.string.settings_notifications_blocked)) },
                    supportingContent = { Text(stringResource(R.string.settings_notifications_blocked_body)) },
                    trailingContent = { OutlinedButton(onClick = { SystemIntents.openNotificationSettings(context) }) { Text(stringResource(R.string.open_settings)) } },
                )
            }
            Toggle(stringResource(R.string.settings_notify_approvals), ui.settings.notifyApprovals, vm::setNotifyApprovals)
            Toggle(stringResource(R.string.settings_notify_questions), ui.settings.notifyQuestions, vm::setNotifyQuestions)
            Toggle(stringResource(R.string.settings_notify_errors), ui.settings.notifyErrors, vm::setNotifyErrors)
            Text(
                stringResource(R.string.settings_notify_turns),
                style = MaterialTheme.typography.bodyLarge,
                modifier = Modifier.padding(start = 16.dp, top = 8.dp),
            )
            for (mode in TurnNotificationMode.entries) {
                Row(
                    Modifier.fillMaxWidth().selectable(selected = ui.settings.turnNotifications == mode, role = Role.RadioButton) { vm.setTurnNotifications(mode) }
                        .padding(horizontal = 16.dp, vertical = 4.dp),
                    verticalAlignment = Alignment.CenterVertically,
                ) {
                    RadioButton(selected = ui.settings.turnNotifications == mode, onClick = null)
                    Text(
                        stringResource(
                            when (mode) {
                                TurnNotificationMode.Always -> R.string.turns_always
                                TurnNotificationMode.WhenNotViewing -> R.string.turns_when_not_viewing
                                TurnNotificationMode.Never -> R.string.turns_never
                            },
                        ),
                        modifier = Modifier.padding(start = 12.dp),
                    )
                }
            }
            TextButton(onClick = { SystemIntents.openNotificationSettings(context) }, modifier = Modifier.padding(horizontal = 8.dp)) {
                Text(stringResource(R.string.settings_system_notifications))
            }

            // ----- sending ----------------------------------------------------------------------
            HorizontalDivider()
            SectionHeader(stringResource(R.string.settings_sending))
            Text(
                stringResource(R.string.settings_follow_up),
                style = MaterialTheme.typography.bodyLarge,
                modifier = Modifier.padding(start = 16.dp, top = 8.dp, end = 16.dp),
            )
            Text(
                stringResource(R.string.settings_follow_up_note),
                style = MaterialTheme.typography.bodySmall,
                color = MaterialTheme.colorScheme.onSurfaceVariant,
                modifier = Modifier.padding(horizontal = 16.dp),
            )
            for (mode in FollowUpDelivery.entries) {
                Row(
                    Modifier.fillMaxWidth().selectable(selected = ui.settings.followUp == mode, role = Role.RadioButton) { vm.setFollowUp(mode) }
                        .padding(horizontal = 16.dp, vertical = 4.dp),
                    verticalAlignment = Alignment.CenterVertically,
                ) {
                    RadioButton(selected = ui.settings.followUp == mode, onClick = null)
                    Text(
                        stringResource(
                            when (mode) {
                                FollowUpDelivery.Queue -> R.string.settings_follow_up_queue
                                FollowUpDelivery.Steer -> R.string.settings_follow_up_steer
                            },
                        ),
                        modifier = Modifier.padding(start = 12.dp),
                    )
                }
            }

            // ----- battery -----------------------------------------------------------------------
            HorizontalDivider()
            SectionHeader(stringResource(R.string.settings_battery))
            Info(
                stringResource(R.string.settings_battery_optimization),
                stringResource(if (permissions.batteryOptimizationIgnored) R.string.battery_exempt else R.string.battery_optimized),
                supporting = if (permissions.batteryOptimizationIgnored) null else stringResource(R.string.battery_optimized_note),
            )
            Link(stringResource(R.string.settings_battery_details), null) { navigator.openBattery() }

            // ----- diagnostics and app ------------------------------------------------------------
            HorizontalDivider()
            SectionHeader(stringResource(R.string.settings_diagnostics))
            Link(stringResource(R.string.settings_diagnostics_open), stringResource(R.string.settings_diagnostics_summary)) { navigator.openDiagnostics() }
            Info(stringResource(R.string.settings_app_version), ui.appVersion)
        }
    }

    if (confirmUnpair) {
        ConfirmDialog(
            title = stringResource(R.string.unpair_confirm_title),
            text = stringResource(R.string.unpair_confirm_body),
            confirm = stringResource(R.string.settings_unpair),
            onConfirm = {
                confirmUnpair = false
                vm.unpair()
            },
            onDismiss = { confirmUnpair = false },
        )
    }
}

@Composable
internal fun Info(label: String, value: String, supporting: String? = null) {
    ListItem(
        headlineContent = { Text(value) },
        overlineContent = { Text(label) },
        supportingContent = supporting?.let { { Text(it) } },
    )
}

@Composable
private fun Toggle(label: String, checked: Boolean, onChange: (Boolean) -> Unit) {
    ListItem(
        headlineContent = { Text(label) },
        trailingContent = { Switch(checked = checked, onCheckedChange = null) },
        modifier = Modifier.clickable(role = Role.Switch) { onChange(!checked) },
    )
}

@Composable
private fun Link(label: String, supporting: String?, onClick: () -> Unit) {
    ListItem(
        headlineContent = { Text(label) },
        supportingContent = supporting?.let { { Text(it) } },
        trailingContent = { Icon(Icons.AutoMirrored.Outlined.KeyboardArrowRight, contentDescription = null) },
        modifier = Modifier.clickable(onClick = onClick),
    )
}
