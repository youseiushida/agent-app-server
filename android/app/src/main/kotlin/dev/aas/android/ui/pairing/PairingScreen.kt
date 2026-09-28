package dev.aas.android.ui.pairing

import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.imePadding
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.text.KeyboardOptions
import androidx.compose.foundation.verticalScroll
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.automirrored.outlined.ArrowBack
import androidx.compose.material.icons.outlined.Close
import androidx.compose.material3.AlertDialog
import androidx.compose.material3.Button
import androidx.compose.material3.CircularProgressIndicator
import androidx.compose.material3.ExperimentalMaterial3Api
import androidx.compose.material3.Icon
import androidx.compose.material3.IconButton
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.OutlinedButton
import androidx.compose.material3.OutlinedTextField
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
import androidx.compose.ui.text.input.ImeAction
import androidx.compose.ui.text.input.KeyboardCapitalization
import androidx.compose.ui.text.input.KeyboardType
import androidx.compose.ui.unit.dp
import androidx.lifecycle.compose.collectAsStateWithLifecycle
import androidx.navigation.NavGraphBuilder
import androidx.navigation.compose.composable
import androidx.navigation.toRoute
import dev.aas.android.R
import dev.aas.android.domain.RequestLabels
import dev.aas.android.pairing.OutboxPlan
import dev.aas.android.pairing.PairingError
import dev.aas.android.pairing.PairingInputError
import dev.aas.android.ui.common.aasViewModel
import dev.aas.android.ui.icons.Link
import dev.aas.android.ui.navigation.AppNavigator
import dev.aas.android.ui.navigation.PairingRoute
import dev.aas.android.ui.navigation.SetupRoute

fun NavGraphBuilder.pairingDestinations(navigator: AppNavigator) {
    composable<PairingRoute> { entry ->
        val route = entry.toRoute<PairingRoute>()
        val vm = aasViewModel(key = "pairing/${route.repair}/${route.link}") { c, _ -> PairingViewModel(route, c) }
        PairingScreen(vm, navigator)
    }
    composable<SetupRoute> { SetupScreen(navigator) }
}

/**
 * Pairing: scan the QR code printed by `agent-app-server pair` (or enter URL and code), confirm,
 * `POST /v1/pair`, store the token (Keystore-encrypted) and connect.
 */
@OptIn(ExperimentalMaterial3Api::class)
@Composable
fun PairingScreen(vm: PairingViewModel, navigator: AppNavigator) {
    val step by vm.step.collectAsStateWithLifecycle()
    val deviceName by vm.deviceName.collectAsStateWithLifecycle()
    LaunchedEffect(step) {
        (step as? PairingStep.Done)?.let { navigator.pairingCompleted(vm.repair, it.setupCompleted) }
    }
    Scaffold(
        topBar = {
            TopAppBar(
                title = { Text(stringResource(if (vm.repair) R.string.pairing_title_repair else R.string.pairing_title)) },
                navigationIcon = {
                    when {
                        step is PairingStep.Scanning || step is PairingStep.Manual || step is PairingStep.Confirm || step is PairingStep.Failed ->
                            IconButton(onClick = vm::backToStart) { Icon(Icons.AutoMirrored.Outlined.ArrowBack, stringResource(R.string.back)) }
                        vm.repair -> IconButton(onClick = navigator::back) { Icon(Icons.Outlined.Close, stringResource(R.string.cancel)) }
                    }
                },
            )
        },
    ) { padding ->
        Box(Modifier.fillMaxSize().padding(padding).imePadding()) {
            when (val s = step) {
                PairingStep.Choose -> Intro(vm.repair, onScan = vm::scan, onManual = vm::manual)
                is PairingStep.Scanning -> Column(Modifier.fillMaxSize()) {
                    s.lastError?.let {
                        Text(inputErrorText(it), color = MaterialTheme.colorScheme.error, modifier = Modifier.padding(16.dp))
                    }
                    QrScanner(onText = vm::onScanned, onManualEntry = vm::manual, modifier = Modifier.weight(1f))
                }
                is PairingStep.Manual -> ManualEntry(s.error, onSubmit = vm::submitManual)
                is PairingStep.Confirm -> Confirm(s.target.serverName, s.target.wsUrl, s.target.code, deviceName, vm::setDeviceName) { vm.pair(s.target) }
                is PairingStep.Pairing -> Column(Modifier.fillMaxSize(), verticalArrangement = Arrangement.Center, horizontalAlignment = Alignment.CenterHorizontally) {
                    CircularProgressIndicator()
                    Spacer(Modifier.height(16.dp))
                    Text(stringResource(R.string.pairing_in_progress, s.target.host))
                }
                is PairingStep.Failed -> Failed(s.error, onRetry = { vm.pair(s.target) }, onRestart = vm::backToStart)
                is PairingStep.DecideOutbox -> Column(Modifier.fillMaxSize()) {}
                is PairingStep.Done -> Column(Modifier.fillMaxSize()) {}
            }
        }
    }
    (step as? PairingStep.DecideOutbox)?.let { decide ->
        OutboxDecisionDialog(decide.plan, onSend = { vm.decideOutbox(decide.device, send = true) }, onDiscard = { vm.decideOutbox(decide.device, send = false) })
    }
}

@Composable
private fun Intro(repair: Boolean, onScan: () -> Unit, onManual: () -> Unit) {
    Column(Modifier.fillMaxSize().padding(24.dp).verticalScroll(rememberScrollState())) {
        Icon(Icons.Outlined.Link, contentDescription = null, modifier = Modifier.size(48.dp), tint = MaterialTheme.colorScheme.primary)
        Spacer(Modifier.height(16.dp))
        Text(stringResource(if (repair) R.string.pairing_intro_repair else R.string.pairing_intro_title), style = MaterialTheme.typography.headlineSmall)
        Spacer(Modifier.height(12.dp))
        Text(stringResource(R.string.pairing_intro_body), style = MaterialTheme.typography.bodyLarge)
        Spacer(Modifier.height(8.dp))
        Text(stringResource(R.string.pairing_intro_command), style = MaterialTheme.typography.bodyMedium, color = MaterialTheme.colorScheme.onSurfaceVariant)
        Spacer(Modifier.height(24.dp))
        Button(onClick = onScan, modifier = Modifier.fillMaxWidth()) { Text(stringResource(R.string.pairing_scan)) }
        Spacer(Modifier.height(8.dp))
        OutlinedButton(onClick = onManual, modifier = Modifier.fillMaxWidth()) { Text(stringResource(R.string.pairing_manual)) }
        Spacer(Modifier.height(24.dp))
        Text(stringResource(R.string.pairing_tailscale_note), style = MaterialTheme.typography.bodySmall, color = MaterialTheme.colorScheme.onSurfaceVariant)
    }
}

@Composable
private fun ManualEntry(error: PairingInputError?, onSubmit: (String, String) -> Unit) {
    var url by rememberSaveable { mutableStateOf("") }
    var code by rememberSaveable { mutableStateOf("") }
    Column(Modifier.fillMaxSize().padding(24.dp).verticalScroll(rememberScrollState())) {
        Text(stringResource(R.string.pairing_manual_body), style = MaterialTheme.typography.bodyMedium)
        Spacer(Modifier.height(16.dp))
        OutlinedTextField(
            value = url,
            onValueChange = { url = it },
            label = { Text(stringResource(R.string.pairing_server_url)) },
            placeholder = { Text(stringResource(R.string.pairing_server_url_hint)) },
            singleLine = true,
            keyboardOptions = KeyboardOptions(keyboardType = KeyboardType.Uri, imeAction = ImeAction.Next, autoCorrectEnabled = false),
            modifier = Modifier.fillMaxWidth(),
        )
        Spacer(Modifier.height(12.dp))
        OutlinedTextField(
            value = code,
            onValueChange = { code = it },
            label = { Text(stringResource(R.string.pairing_code)) },
            placeholder = { Text(stringResource(R.string.pairing_code_hint)) },
            singleLine = true,
            keyboardOptions = KeyboardOptions(capitalization = KeyboardCapitalization.Characters, imeAction = ImeAction.Done, autoCorrectEnabled = false),
            modifier = Modifier.fillMaxWidth(),
        )
        error?.let {
            Spacer(Modifier.height(8.dp))
            Text(inputErrorText(it), color = MaterialTheme.colorScheme.error, style = MaterialTheme.typography.bodyMedium)
        }
        Spacer(Modifier.height(24.dp))
        Button(onClick = { onSubmit(url, code) }, enabled = url.isNotBlank() && code.isNotBlank(), modifier = Modifier.fillMaxWidth()) {
            Text(stringResource(R.string.next))
        }
    }
}

@Composable
private fun Confirm(serverName: String?, url: String, code: String, deviceName: String, onDeviceName: (String) -> Unit, onPair: () -> Unit) {
    Column(Modifier.fillMaxSize().padding(24.dp).verticalScroll(rememberScrollState())) {
        Text(stringResource(R.string.pairing_confirm_title), style = MaterialTheme.typography.headlineSmall)
        Spacer(Modifier.height(16.dp))
        serverName?.let { Field(stringResource(R.string.pairing_server_name), it) }
        Field(stringResource(R.string.pairing_server_url), url)
        Field(stringResource(R.string.pairing_code), code)
        Spacer(Modifier.height(12.dp))
        OutlinedTextField(
            value = deviceName,
            onValueChange = onDeviceName,
            label = { Text(stringResource(R.string.pairing_device_name)) },
            supportingText = { Text(stringResource(R.string.pairing_device_name_hint)) },
            singleLine = true,
            modifier = Modifier.fillMaxWidth(),
        )
        Spacer(Modifier.height(8.dp))
        Text(stringResource(R.string.pairing_confirm_note), style = MaterialTheme.typography.bodySmall, color = MaterialTheme.colorScheme.onSurfaceVariant)
        Spacer(Modifier.height(24.dp))
        Button(onClick = onPair, modifier = Modifier.fillMaxWidth()) { Text(stringResource(R.string.pairing_pair)) }
    }
}

@Composable
private fun Field(label: String, value: String) {
    Column(Modifier.padding(vertical = 4.dp)) {
        Text(label, style = MaterialTheme.typography.labelMedium, color = MaterialTheme.colorScheme.onSurfaceVariant)
        Text(value, style = MaterialTheme.typography.bodyLarge)
    }
}

@Composable
private fun Failed(error: PairingError, onRetry: () -> Unit, onRestart: () -> Unit) {
    Column(Modifier.fillMaxSize().padding(24.dp)) {
        Text(stringResource(R.string.pairing_failed), style = MaterialTheme.typography.headlineSmall, color = MaterialTheme.colorScheme.error)
        Spacer(Modifier.height(12.dp))
        Text(pairingErrorText(error), style = MaterialTheme.typography.bodyLarge)
        Spacer(Modifier.height(24.dp))
        Row(horizontalArrangement = Arrangement.spacedBy(8.dp)) {
            // A used or expired code cannot be retried; everything else can.
            if (error !is PairingError.InvalidCode && error !is PairingError.LocalStorage) Button(onClick = onRetry) { Text(stringResource(R.string.action_retry)) }
            OutlinedButton(onClick = onRestart) { Text(stringResource(R.string.pairing_start_over)) }
        }
    }
}

@Composable
private fun OutboxDecisionDialog(plan: OutboxPlan.Ask, onSend: () -> Unit, onDiscard: () -> Unit) {
    val labels = plan.entries.map { stringResource(RequestLabels.of(it.method)) }
    AlertDialog(
        onDismissRequest = {},
        title = { Text(stringResource(R.string.pairing_outbox_title, plan.entries.size)) },
        text = {
            Column {
                Text(stringResource(if (plan.sameServer) R.string.pairing_outbox_same_server else R.string.pairing_outbox_other_server))
                Spacer(Modifier.height(8.dp))
                for (label in labels.distinct()) Text("• $label (${labels.count { it == label }})", style = MaterialTheme.typography.bodySmall)
            }
        },
        confirmButton = { TextButton(onClick = onSend) { Text(stringResource(R.string.pairing_outbox_send)) } },
        dismissButton = { TextButton(onClick = onDiscard) { Text(stringResource(R.string.pairing_outbox_discard)) } },
    )
}

@Composable
private fun inputErrorText(error: PairingInputError): String = when (error) {
    PairingInputError.NotAPairingLink -> stringResource(R.string.pairing_error_not_link)
    is PairingInputError.MalformedEncoding -> stringResource(R.string.pairing_error_encoding)
    PairingInputError.MissingUrl -> stringResource(R.string.pairing_error_missing_url)
    is PairingInputError.InvalidUrl -> stringResource(R.string.pairing_error_invalid_url, error.url)
    is PairingInputError.CleartextNotAllowed -> stringResource(R.string.pairing_error_cleartext, error.host)
    PairingInputError.MissingCode -> stringResource(R.string.pairing_error_missing_code)
}

@Composable
private fun pairingErrorText(error: PairingError): String = when (error) {
    PairingError.InvalidCode -> stringResource(R.string.pairing_error_invalid_code)
    PairingError.RateLimited -> stringResource(R.string.pairing_error_rate_limited)
    is PairingError.UnknownHost -> stringResource(R.string.pairing_error_unknown_host, error.host)
    is PairingError.Unreachable -> stringResource(R.string.pairing_error_unreachable, error.message)
    is PairingError.Tls -> stringResource(R.string.pairing_error_tls, error.message)
    is PairingError.CleartextBlocked -> stringResource(R.string.pairing_error_cleartext_blocked)
    is PairingError.Rejected -> stringResource(R.string.pairing_error_rejected, error.status, error.message)
    is PairingError.InvalidResponse -> stringResource(R.string.pairing_error_invalid_response, error.message)
    is PairingError.LocalStorage -> stringResource(R.string.pairing_error_local, error.message)
}
