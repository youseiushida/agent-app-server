package dev.aas.android.ui.settings

import androidx.lifecycle.ViewModel
import androidx.lifecycle.viewModelScope
import dev.aas.android.AppContainer
import dev.aas.android.AppVersion
import dev.aas.android.R
import dev.aas.android.domain.ErrorTexts
import dev.aas.android.domain.composer.FollowUpDelivery
import dev.aas.android.protocol.Device
import dev.aas.android.protocol.Harness
import dev.aas.android.protocol.RpcException
import dev.aas.android.protocol.ServerStatusResult
import dev.aas.android.security.PairingState
import dev.aas.android.settings.AppSettings
import dev.aas.android.settings.TurnNotificationMode
import dev.aas.android.sync.NotConnectedException
import dev.aas.android.sync.SyncStatus
import dev.aas.android.ui.common.HarnessRefresher
import dev.aas.android.ui.common.UiText
import dev.aas.android.ui.common.requestFailed
import kotlinx.coroutines.CancellationException
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.SharingStarted
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.flow.asStateFlow
import kotlinx.coroutines.flow.combine
import kotlinx.coroutines.flow.stateIn
import kotlinx.coroutines.launch

/** A value fetched from the server on demand. */
sealed interface Remote<out T> {
    data object Idle : Remote<Nothing>

    data object Loading : Remote<Nothing>

    data class Loaded<T>(val value: T) : Remote<T>

    data class Failed(val message: UiText) : Remote<Nothing>
}

/** Runs a read-only server call and describes its failure in the user's words. */
internal suspend fun <T> fetch(block: suspend () -> T): Remote<T> = try {
    Remote.Loaded(block())
} catch (e: CancellationException) {
    throw e
} catch (e: NotConnectedException) {
    Remote.Failed(UiText.of(R.string.error_not_connected))
} catch (e: RpcException) {
    Remote.Failed(ErrorTexts.server(e.error))
} catch (e: Exception) {
    Remote.Failed(requestFailed(e))
}

data class SettingsUiState(
    val pairing: PairingState?,
    val status: SyncStatus,
    val settings: AppSettings,
    val serverStatus: Remote<ServerStatusResult>,
    val unpairing: Boolean,
    /** The daemon's harnesses (synced), with the ones this app is probing right now. */
    val harnesses: List<Harness> = emptyList(),
    val probing: Set<String> = emptySet(),
) {
    val appVersion: String get() = AppVersion.Current.name
}

class SettingsViewModel(private val container: AppContainer) : ViewModel() {
    private val serverStatus = MutableStateFlow<Remote<ServerStatusResult>>(Remote.Idle)
    private val unpairing = MutableStateFlow(false)
    private val refresher = HarnessRefresher(container.harnessRepository, container.userMessages)

    val state: StateFlow<SettingsUiState> = combine(
        container.pairingState,
        container.engine.status,
        container.settings.settings,
        combine(serverStatus, unpairing) { server, busy -> server to busy },
        combine(container.harnessRepository.harnesses, container.harnessRepository.refreshing) { harnesses, probing -> harnesses to probing },
    ) { pairing, status, settings, (server, busy), (harnesses, probing) ->
        SettingsUiState(pairing, status, settings, server, busy, harnesses, probing)
    }
        .stateIn(
            viewModelScope,
            SharingStarted.WhileSubscribed(container.policy.uiStopTimeoutMs),
            SettingsUiState(container.pairingState.value, container.engine.status.value, AppSettings(), Remote.Idle, false),
        )

    fun refreshServerStatus() {
        serverStatus.value = Remote.Loading
        viewModelScope.launch { serverStatus.value = fetch { container.serverRepository.status() } }
    }

    fun setNotifyApprovals(value: Boolean) = edit { container.settings.setNotifyApprovals(value) }

    fun setNotifyQuestions(value: Boolean) = edit { container.settings.setNotifyQuestions(value) }

    fun setTurnNotifications(value: TurnNotificationMode) = edit { container.settings.setTurnNotifications(value) }

    fun setNotifyErrors(value: Boolean) = edit { container.settings.setNotifyErrors(value) }

    fun setFollowUp(value: FollowUpDelivery) = edit { container.settings.setFollowUp(value) }

    fun reconnectNow() = container.reconnectNow()

    /** 再確認: `harness/refresh` of one harness, or of all when [harnessId] is `null`. */
    fun refreshHarness(harnessId: String?) = refresher.refresh(viewModelScope, harnessId)

    /** Revokes this device on the server when connected, then forgets everything local. */
    fun unpair() {
        if (unpairing.value) return
        unpairing.value = true
        viewModelScope.launch {
            try {
                val result = container.pairingRepository.unpair()
                if (!result.revokedOnServer && result.deviceId != null) {
                    container.userMessages.show(UiText.of(R.string.unpair_not_revoked, result.deviceId))
                }
            } catch (e: CancellationException) {
                throw e
            } catch (e: Exception) {
                container.userMessages.show(UiText.of(R.string.unpair_failed, e.message ?: e.javaClass.simpleName))
            } finally {
                unpairing.value = false
            }
        }
    }

    private fun edit(block: suspend () -> Unit) {
        viewModelScope.launch {
            try {
                block()
            } catch (e: CancellationException) {
                throw e
            } catch (e: Exception) {
                container.userMessages.show(UiText.of(R.string.error_local_store, e.message ?: e.javaClass.simpleName))
            }
        }
    }
}

class DevicesViewModel(private val container: AppContainer) : ViewModel() {
    private val _devices = MutableStateFlow<Remote<List<Device>>>(Remote.Idle)
    val devices: StateFlow<Remote<List<Device>>> = _devices.asStateFlow()

    private val _revoking = MutableStateFlow<String?>(null)
    val revoking: StateFlow<String?> = _revoking.asStateFlow()

    fun refresh() {
        _devices.value = Remote.Loading
        viewModelScope.launch { _devices.value = fetch { container.serverRepository.devices() } }
    }

    /**
     * Revokes another device. Only while connected: the list it was picked from is live data,
     * and a revocation queued for later would leave the user unsure whether it happened.
     */
    fun revoke(device: Device) {
        if (_revoking.value != null) return
        if (!container.engine.status.value.isOnline) {
            container.userMessages.show(UiText.of(R.string.error_not_connected))
            return
        }
        _revoking.value = device.id
        viewModelScope.launch {
            try {
                container.serverRepository.revoke(device.id)
                container.userMessages.show(UiText.of(R.string.devices_revoked, device.name))
            } catch (e: CancellationException) {
                throw e
            } catch (e: RpcException) {
                container.userMessages.show(ErrorTexts.server(e.error))
            } catch (e: Exception) {
                container.userMessages.show(requestFailed(e))
            } finally {
                _revoking.value = null
                refresh()
            }
        }
    }
}
