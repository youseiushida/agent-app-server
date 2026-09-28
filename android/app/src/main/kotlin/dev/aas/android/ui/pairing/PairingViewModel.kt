package dev.aas.android.ui.pairing

import android.os.Build
import androidx.lifecycle.ViewModel
import androidx.lifecycle.viewModelScope
import dev.aas.android.AppContainer
import dev.aas.android.diagnostics.ConnectionLog
import dev.aas.android.pairing.OutboxPlan
import dev.aas.android.pairing.PairedDevice
import dev.aas.android.pairing.PairingError
import dev.aas.android.pairing.PairingException
import dev.aas.android.pairing.PairingInputError
import dev.aas.android.pairing.PairingParseResult
import dev.aas.android.pairing.PairingTarget
import dev.aas.android.ui.navigation.PairingRoute
import kotlinx.coroutines.CancellationException
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.flow.asStateFlow
import kotlinx.coroutines.launch

/** Where the pairing flow is. */
sealed interface PairingStep {
    /** Explanation with 読み取る / 手で入力. */
    data object Choose : PairingStep

    /** The camera is looking for the QR code; [lastError] is why the last code was not used. */
    data class Scanning(val lastError: PairingInputError?) : PairingStep

    /** Manual entry; [error] is why the last attempt was rejected. */
    data class Manual(val error: PairingInputError?) : PairingStep

    /** Values are valid; the user confirms (and may rename the device). */
    data class Confirm(val target: PairingTarget) : PairingStep

    /** `POST /v1/pair` is running. */
    data class Pairing(val target: PairingTarget) : PairingStep

    data class Failed(val target: PairingTarget, val error: PairingError) : PairingStep

    /** Requests are waiting in the outbox: send them as the new device, or discard them. */
    data class DecideOutbox(val device: PairedDevice, val plan: OutboxPlan.Ask) : PairingStep

    /** Stored and connecting; the screen moves on. */
    data class Done(val setupCompleted: Boolean) : PairingStep
}

class PairingViewModel(private val route: PairingRoute, private val container: AppContainer) : ViewModel() {
    private val _step = MutableStateFlow<PairingStep>(PairingStep.Choose)
    val step: StateFlow<PairingStep> = _step.asStateFlow()

    private val _deviceName = MutableStateFlow(defaultDeviceName())

    /** The name this device registers with (editable on the confirmation step). */
    val deviceName: StateFlow<String> = _deviceName.asStateFlow()

    val repair: Boolean get() = route.repair

    init {
        // An aas://pair link opened from another app: confirm its values.
        route.link?.let { link ->
            _step.value = when (val parsed = container.pairingParser.parseLink(link)) {
                is PairingParseResult.Ok -> PairingStep.Confirm(parsed.target)
                is PairingParseResult.Invalid -> PairingStep.Manual(parsed.error)
            }
        }
    }

    fun scan() {
        _step.value = PairingStep.Scanning(null)
    }

    fun manual() {
        _step.value = PairingStep.Manual(null)
    }

    fun backToStart() {
        _step.value = PairingStep.Choose
    }

    /** A QR code was decoded; repeats of the same frame while not scanning are ignored. */
    fun onScanned(text: String) {
        if (_step.value !is PairingStep.Scanning) return
        _step.value = when (val parsed = container.pairingParser.parseLink(text)) {
            is PairingParseResult.Ok -> PairingStep.Confirm(parsed.target)
            is PairingParseResult.Invalid -> PairingStep.Scanning(parsed.error)
        }
    }

    fun submitManual(url: String, code: String) {
        _step.value = when (val parsed = container.pairingParser.parseManual(url, code)) {
            is PairingParseResult.Ok -> PairingStep.Confirm(parsed.target)
            is PairingParseResult.Invalid -> PairingStep.Manual(parsed.error)
        }
    }

    fun setDeviceName(name: String) {
        _deviceName.value = name
    }

    fun pair(target: PairingTarget) {
        val name = _deviceName.value.trim().ifEmpty { defaultDeviceName() }
        _step.value = PairingStep.Pairing(target)
        viewModelScope.launch {
            try {
                val device = container.pairingRepository.pair(target, name)
                when (val plan = container.pairingRepository.plan(device)) {
                    OutboxPlan.NothingPending -> apply(device, discard = false)
                    is OutboxPlan.Ask -> _step.value = PairingStep.DecideOutbox(device, plan)
                }
            } catch (e: CancellationException) {
                throw e
            } catch (e: PairingException) {
                _step.value = PairingStep.Failed(target, e.error)
            }
        }
    }

    /** The user decided about the waiting requests. */
    fun decideOutbox(device: PairedDevice, send: Boolean) {
        viewModelScope.launch { apply(device, discard = !send) }
    }

    private suspend fun apply(device: PairedDevice, discard: Boolean) {
        try {
            container.pairingRepository.apply(device, discardLocal = discard)
            _step.value = PairingStep.Done(container.settings.current().setupCompleted)
        } catch (e: CancellationException) {
            throw e
        } catch (e: Exception) {
            // Storing the token failed (keystore, disk): the code is used up, so pair again.
            container.connectionLog.warn(ConnectionLog.SOURCE_PAIRING, "storing the pairing failed", e)
            _step.value = PairingStep.Failed(device.target, PairingError.LocalStorage(e.message ?: e.javaClass.simpleName))
        }
    }

    private fun defaultDeviceName(): String = listOfNotNull(Build.MANUFACTURER?.replaceFirstChar { it.uppercase() }, Build.MODEL)
        .joinToString(" ")
        .ifBlank { DEFAULT_NAME }

    private companion object {
        const val DEFAULT_NAME = "Android"
    }
}
