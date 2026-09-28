package dev.aas.android.ui.settings

import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.foundation.lazy.items
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.automirrored.outlined.ArrowBack
import androidx.compose.material.icons.outlined.Refresh
import androidx.compose.material3.CircularProgressIndicator
import androidx.compose.material3.ExperimentalMaterial3Api
import androidx.compose.material3.HorizontalDivider
import androidx.compose.material3.Icon
import androidx.compose.material3.IconButton
import androidx.compose.material3.ListItem
import androidx.compose.material3.Scaffold
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.material3.TopAppBar
import androidx.compose.runtime.Composable
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.saveable.rememberSaveable
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.res.stringResource
import androidx.lifecycle.compose.collectAsStateWithLifecycle
import dev.aas.android.R
import dev.aas.android.protocol.Device
import dev.aas.android.ui.common.asString
import dev.aas.android.ui.components.ConfirmDialog
import dev.aas.android.ui.components.EmptyState
import dev.aas.android.ui.components.relativeTime
import dev.aas.android.ui.icons.CloudOff
import dev.aas.android.ui.icons.Devices
import dev.aas.android.ui.navigation.AppNavigator

/** 設定 → デバイス: the devices paired with the daemon; any but this one can be revoked. */
@OptIn(ExperimentalMaterial3Api::class)
@Composable
fun DevicesScreen(vm: DevicesViewModel, navigator: AppNavigator) {
    val devices by vm.devices.collectAsStateWithLifecycle()
    val revoking by vm.revoking.collectAsStateWithLifecycle()
    var confirm by rememberSaveable { mutableStateOf<String?>(null) }
    LaunchedEffect(Unit) { vm.refresh() }
    Scaffold(
        topBar = {
            TopAppBar(
                title = { Text(stringResource(R.string.settings_devices)) },
                navigationIcon = { IconButton(onClick = navigator::back) { Icon(Icons.AutoMirrored.Outlined.ArrowBack, stringResource(R.string.back)) } },
                actions = { IconButton(onClick = vm::refresh) { Icon(Icons.Outlined.Refresh, stringResource(R.string.refresh)) } },
            )
        },
    ) { padding ->
        when (val state = devices) {
            Remote.Idle, Remote.Loading -> Box(Modifier.fillMaxSize().padding(padding), contentAlignment = Alignment.Center) { CircularProgressIndicator() }
            is Remote.Failed -> EmptyState(Icons.Outlined.CloudOff, state.message.asString(), Modifier.padding(padding), action = stringResource(R.string.action_retry), onAction = vm::refresh)
            is Remote.Loaded -> if (state.value.isEmpty()) {
                EmptyState(Icons.Outlined.Devices, stringResource(R.string.devices_empty), Modifier.padding(padding))
            } else {
                LazyColumn(Modifier.fillMaxSize(), contentPadding = padding) {
                    items(state.value, key = { it.id }) { device ->
                        DeviceRow(device, busy = revoking == device.id, onRevoke = { confirm = device.id })
                        HorizontalDivider()
                    }
                }
            }
        }
    }
    val target = confirm?.let { id -> (devices as? Remote.Loaded)?.value?.firstOrNull { it.id == id } }
    if (target != null) {
        ConfirmDialog(
            title = stringResource(R.string.devices_revoke_title, target.name),
            text = stringResource(R.string.devices_revoke_body),
            confirm = stringResource(R.string.devices_revoke),
            onConfirm = {
                confirm = null
                vm.revoke(target)
            },
            onDismiss = { confirm = null },
        )
    }
}

@Composable
private fun DeviceRow(device: Device, busy: Boolean, onRevoke: () -> Unit) {
    val details = listOfNotNull(
        device.platform,
        stringResource(R.string.devices_added, relativeTime(device.createdAt)),
        device.lastSeenAt?.let { stringResource(R.string.devices_last_seen, relativeTime(it)) },
    ).joinToString(" · ")
    ListItem(
        headlineContent = { Text(if (device.current) stringResource(R.string.devices_this_device, device.name) else device.name) },
        supportingContent = { Text(details) },
        trailingContent = {
            when {
                device.current -> Unit
                busy -> CircularProgressIndicator()
                else -> TextButton(onClick = onRevoke) { Text(stringResource(R.string.devices_revoke)) }
            }
        },
    )
}
