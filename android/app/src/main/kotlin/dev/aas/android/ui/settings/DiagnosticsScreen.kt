package dev.aas.android.ui.settings

import android.content.ClipData
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.foundation.lazy.items
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.automirrored.outlined.ArrowBack
import androidx.compose.material3.ExperimentalMaterial3Api
import androidx.compose.material3.HorizontalDivider
import androidx.compose.material3.Icon
import androidx.compose.material3.IconButton
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Scaffold
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.material3.TopAppBar
import androidx.compose.runtime.Composable
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.rememberCoroutineScope
import androidx.compose.runtime.saveable.rememberSaveable
import androidx.compose.runtime.setValue
import androidx.compose.ui.Modifier
import androidx.compose.ui.platform.ClipEntry
import androidx.compose.ui.platform.LocalClipboard
import androidx.compose.ui.platform.LocalResources
import androidx.compose.ui.res.stringResource
import androidx.compose.ui.unit.dp
import androidx.lifecycle.ViewModel
import androidx.lifecycle.compose.collectAsStateWithLifecycle
import androidx.lifecycle.viewModelScope
import dev.aas.android.AppContainer
import dev.aas.android.R
import dev.aas.android.diagnostics.LogEntry
import dev.aas.android.domain.RequestLabels
import dev.aas.android.domain.ResultMessages
import dev.aas.android.security.PairingState
import dev.aas.android.service.ConnectionPresentation
import dev.aas.android.service.ConnectionTexts
import dev.aas.android.service.StartFailure
import dev.aas.android.sync.OutboxEntry
import dev.aas.android.sync.SyncStatus
import dev.aas.android.ui.components.ConfirmDialog
import dev.aas.android.ui.components.SectionHeader
import dev.aas.android.ui.components.relativeTime
import dev.aas.android.ui.icons.ContentCopy
import dev.aas.android.ui.navigation.AppNavigator
import dev.aas.android.ui.theme.codeStyle
import kotlinx.coroutines.CancellationException
import kotlinx.coroutines.flow.SharingStarted
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.flow.combine
import kotlinx.coroutines.flow.stateIn
import kotlinx.coroutines.launch
import java.time.Instant
import java.time.ZoneId

data class DiagnosticsState(
    val status: SyncStatus,
    val outbox: List<OutboxEntry>,
    val log: List<LogEntry>,
    val serviceRunning: Boolean,
    val lastStartFailure: StartFailure?,
    val pairing: PairingState?,
)

class DiagnosticsViewModel(private val container: AppContainer) : ViewModel() {
    val state: StateFlow<DiagnosticsState> = combine(
        combine(container.engine.status, container.engine.outbox, container.pairingState) { status, outbox, pairing -> Triple(status, outbox, pairing) },
        container.connectionLog.entries,
        container.connectionController.serviceRunning,
        container.connectionController.lastStartFailure,
    ) { (status, outbox, pairing), log, running, failure -> DiagnosticsState(status, outbox, log, running, failure, pairing) }
        .stateIn(
            viewModelScope,
            SharingStarted.WhileSubscribed(container.policy.uiStopTimeoutMs),
            DiagnosticsState(
                container.engine.status.value, container.engine.outbox.value, container.connectionLog.entries.value, false, null,
                container.pairingState.value,
            ),
        )

    fun reconnectNow() = container.reconnectNow()

    /** Drops a request from the outbox (it is never sent again); the outcome is shown as a snackbar. */
    fun discard(entry: OutboxEntry) {
        viewModelScope.launch {
            val message = try {
                ResultMessages.discarded(container.workspaceRepository.discard(entry.clientRequestId))
            } catch (e: CancellationException) {
                throw e
            } catch (e: Exception) {
                dev.aas.android.ui.common.UiText.of(R.string.error_local_store, e.message ?: e.javaClass.simpleName)
            }
            container.userMessages.show(message)
        }
    }

    fun logText(): String = container.connectionLog.asText()
}

/**
 * 設定 → 診断: connection state and counters, epoch and cursors (with the server's heads), the
 * outbox, the service, and the connection log (copyable).
 */
@OptIn(ExperimentalMaterial3Api::class)
@Composable
fun DiagnosticsScreen(vm: DiagnosticsViewModel, navigator: AppNavigator) {
    val ui by vm.state.collectAsStateWithLifecycle()
    val res = LocalResources.current
    val clipboard = LocalClipboard.current
    val scope = rememberCoroutineScope()
    val status = ui.status
    var discarding by rememberSaveable { mutableStateOf<String?>(null) }
    ui.outbox.firstOrNull { it.clientRequestId == discarding }?.let { entry ->
        ConfirmDialog(
            title = stringResource(R.string.outbox_discard_title),
            text = stringResource(R.string.outbox_discard_text, stringResource(RequestLabels.of(entry.method))),
            confirm = stringResource(R.string.outbox_discard),
            onConfirm = {
                discarding = null
                vm.discard(entry)
            },
            onDismiss = { discarding = null },
        )
    }
    Scaffold(
        topBar = {
            TopAppBar(
                title = { Text(stringResource(R.string.settings_diagnostics)) },
                navigationIcon = { IconButton(onClick = navigator::back) { Icon(Icons.AutoMirrored.Outlined.ArrowBack, stringResource(R.string.back)) } },
                actions = {
                    IconButton(onClick = {
                        scope.launch { clipboard.setClipEntry(ClipEntry(ClipData.newPlainText(res.getString(R.string.diagnostics_log), vm.logText()))) }
                    }) { Icon(Icons.Outlined.ContentCopy, stringResource(R.string.diagnostics_copy_log)) }
                },
            )
        },
    ) { padding ->
        LazyColumn(Modifier.fillMaxWidth(), contentPadding = padding) {
            item {
                SectionHeader(stringResource(R.string.diagnostics_connection)) {
                    TextButton(onClick = vm::reconnectNow) { Text(stringResource(R.string.action_reconnect)) }
                }
                val presentation = ConnectionPresentation.of(status, ui.pairing)
                Kv(stringResource(R.string.diagnostics_state), ConnectionTexts.describe(res, presentation, System.currentTimeMillis()).let { t -> listOfNotNull(t.title, t.detail).joinToString("\n") })
                Kv(stringResource(R.string.diagnostics_raw_state), status.connection.toString())
                Kv(stringResource(R.string.diagnostics_service), stringResource(if (ui.serviceRunning) R.string.diagnostics_service_running else R.string.diagnostics_service_stopped))
                ui.lastStartFailure?.let { Kv(stringResource(R.string.diagnostics_start_failure), "${it.reason}: ${it.message} (${relativeTime(it.atMs)})") }
                Kv(stringResource(R.string.diagnostics_last_sync), status.lastSyncAtMs?.let { relativeTime(it) } ?: stringResource(R.string.never))
                Kv(stringResource(R.string.diagnostics_last_heartbeat), status.lastHeartbeatAtMs?.let { relativeTime(it) } ?: stringResource(R.string.never))
                Kv(stringResource(R.string.diagnostics_counters), stringResource(R.string.diagnostics_counters_value, status.reconnects, status.stallResubscribes, status.droppedSignals))
                status.lastError?.let { Kv(stringResource(R.string.diagnostics_last_error), "${it.message} (${relativeTime(it.atMs)})") }
                status.policy?.let {
                    Kv(
                        stringResource(R.string.diagnostics_policy),
                        stringResource(R.string.diagnostics_policy_value, it.heartbeatIntervalMs, it.clientTimeoutMs, it.maxClientFrameBytes, it.maxBlobBytes),
                    )
                }
                HorizontalDivider()
                SectionHeader(stringResource(R.string.diagnostics_sync))
                Kv(stringResource(R.string.diagnostics_epoch), status.server?.epoch ?: stringResource(R.string.unknown))
                Kv(stringResource(R.string.diagnostics_device), status.deviceId ?: stringResource(R.string.unknown))
                val streams = (status.cursors.keys + status.serverHeads.keys).sorted()
                if (streams.isEmpty()) Kv(stringResource(R.string.diagnostics_cursors), stringResource(R.string.none))
                for (stream in streams) {
                    Kv(stream, stringResource(R.string.diagnostics_cursor_value, status.cursors[stream]?.toString() ?: "-", status.serverHeads[stream]?.toString() ?: "-"))
                }
                HorizontalDivider()
                SectionHeader(stringResource(R.string.diagnostics_outbox, ui.outbox.size))
                if (ui.outbox.isEmpty()) Kv(stringResource(R.string.diagnostics_outbox_empty), "")
            }
            items(ui.outbox, key = { "o-" + it.clientRequestId }) { entry ->
                Column(Modifier.padding(horizontal = 16.dp, vertical = 6.dp)) {
                    Text(entry.method, style = MaterialTheme.typography.bodyMedium)
                    Text(entry.clientRequestId, style = MaterialTheme.codeStyle)
                    val details = listOfNotNull(
                        stringResource(R.string.diagnostics_outbox_created, relativeTime(entry.createdAtMs)),
                        entry.threadId?.let { stringResource(R.string.diagnostics_outbox_thread, it) },
                        if (entry.failures > 0) stringResource(R.string.diagnostics_outbox_failures, entry.failures) else null,
                        entry.lastError,
                        if (entry.nextAttemptAtMs > System.currentTimeMillis()) stringResource(R.string.diagnostics_outbox_next, relativeTime(entry.nextAttemptAtMs)) else null,
                    )
                    Text(details.joinToString(" · "), style = MaterialTheme.typography.labelSmall, color = MaterialTheme.colorScheme.onSurfaceVariant)
                    TextButton(onClick = { discarding = entry.clientRequestId }) { Text(stringResource(R.string.outbox_discard)) }
                }
            }
            item {
                HorizontalDivider()
                SectionHeader(stringResource(R.string.diagnostics_log_count, ui.log.size))
            }
            items(ui.log.asReversed(), key = { "l-${it.seq}" }) { entry ->
                Row(Modifier.padding(horizontal = 16.dp, vertical = 2.dp)) {
                    Text(
                        "${Instant.ofEpochMilli(entry.atMs).atZone(ZoneId.systemDefault()).toLocalTime().withNano(0)} ${entry.level.name.first()} [${entry.source}] ${entry.message}",
                        style = MaterialTheme.codeStyle,
                    )
                }
            }
        }
    }
}

@Composable
private fun Kv(key: String, value: String) {
    Column(Modifier.fillMaxWidth().padding(horizontal = 16.dp, vertical = 4.dp)) {
        Text(key, style = MaterialTheme.typography.labelSmall, color = MaterialTheme.colorScheme.onSurfaceVariant)
        if (value.isNotEmpty()) Text(value, style = MaterialTheme.typography.bodyMedium)
    }
}
