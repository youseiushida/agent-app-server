package dev.aas.android.ui.pairing

import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.verticalScroll
import androidx.compose.material3.Button
import androidx.compose.material3.ExperimentalMaterial3Api
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Scaffold
import androidx.compose.material3.Text
import androidx.compose.material3.TopAppBar
import androidx.compose.runtime.Composable
import androidx.compose.runtime.rememberCoroutineScope
import androidx.compose.ui.Modifier
import androidx.compose.ui.res.stringResource
import androidx.compose.ui.unit.dp
import dev.aas.android.R
import dev.aas.android.ui.common.LocalAppContainer
import dev.aas.android.ui.common.UiText
import dev.aas.android.ui.navigation.AppNavigator
import dev.aas.android.ui.settings.BatteryOptimizationCard
import dev.aas.android.ui.settings.NotificationPermissionCard
import dev.aas.android.ui.settings.rememberSystemPermissions
import kotlinx.coroutines.CancellationException
import kotlinx.coroutines.launch

/**
 * After the first pairing: explains and asks for the notification permission and the
 * battery-optimisation exemption. Both can be skipped and changed later in 設定.
 */
@OptIn(ExperimentalMaterial3Api::class)
@Composable
fun SetupScreen(navigator: AppNavigator) {
    val container = LocalAppContainer.current
    val scope = rememberCoroutineScope()
    val permissions = rememberSystemPermissions()
    Scaffold(topBar = { TopAppBar(title = { Text(stringResource(R.string.setup_title)) }) }) { padding ->
        Column(Modifier.fillMaxSize().padding(padding).padding(16.dp).verticalScroll(rememberScrollState())) {
            Text(stringResource(R.string.setup_body), style = MaterialTheme.typography.bodyLarge)
            Spacer(Modifier.height(16.dp))
            NotificationPermissionCard(permissions, onChanged = {})
            Spacer(Modifier.height(12.dp))
            BatteryOptimizationCard(permissions)
            Spacer(Modifier.height(24.dp))
            Button(
                onClick = {
                    scope.launch {
                        try {
                            container.settings.setSetupCompleted(true)
                        } catch (e: CancellationException) {
                            throw e
                        } catch (e: Exception) {
                            // Only means the setup is offered again after the next pairing.
                            container.userMessages.show(UiText.of(R.string.error_local_store, e.message ?: e.javaClass.simpleName))
                        }
                        navigator.setupCompleted()
                    }
                },
                modifier = Modifier.fillMaxWidth(),
            ) { Text(stringResource(if (permissions.notificationsAllowed && permissions.batteryOptimizationIgnored) R.string.done else R.string.setup_later)) }
        }
    }
}
