package dev.aas.android.ui.shell

import androidx.lifecycle.ViewModel
import androidx.lifecycle.viewModelScope
import dev.aas.android.AppContainer
import dev.aas.android.R
import dev.aas.android.domain.HarnessWait
import dev.aas.android.domain.RequestLabels
import dev.aas.android.domain.ResultMessages
import dev.aas.android.security.PairingState
import dev.aas.android.service.ConnectionPresentation
import dev.aas.android.ui.common.HarnessRefresher
import dev.aas.android.ui.common.UiText
import dev.aas.android.ui.common.UserMessage
import kotlinx.coroutines.flow.Flow
import kotlinx.coroutines.flow.SharingStarted
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.flow.combine
import kotlinx.coroutines.flow.distinctUntilChanged
import kotlinx.coroutines.flow.flow
import kotlinx.coroutines.flow.map
import kotlinx.coroutines.flow.mapNotNull
import kotlinx.coroutines.flow.merge
import kotlinx.coroutines.flow.stateIn
import kotlinx.coroutines.launch

/** State of the app shell: connection strip, attention banner, 要対応 badge. */
data class ShellState(
    val presentation: ConnectionPresentation,
    /** The stored token cannot be decrypted (pairing again is the only way out). */
    val tokenUnreadable: Boolean,
    val inboxBadge: Int,
)

class ShellViewModel(private val container: AppContainer) : ViewModel() {
    private val refresher = HarnessRefresher(container.harnessRepository, container.userMessages)

    val state: StateFlow<ShellState> = combine(
        container.engine.status,
        container.pairingState,
        container.workspaceRepository.inbox.map { it.badgeCount }.distinctUntilChanged(),
    ) { status, pairing, badge ->
        ShellState(ConnectionPresentation.of(status, pairing), pairing is PairingState.Unreadable, badge)
    }.distinctUntilChanged().stateIn(
        viewModelScope,
        SharingStarted.WhileSubscribed(container.policy.uiStopTimeoutMs),
        ShellState(ConnectionPresentation.of(container.engine.status.value, container.pairingState.value), false, 0),
    )

    /**
     * A request that just started waiting for its harness (the server answered
     * `harnessUnavailable`), said at once wherever the user is, with 再確認. Requests already
     * waiting when the shell starts are not repeated: their screens show them.
     */
    private val harnessWaits: Flow<UserMessage> = flow {
        var known: Set<String>? = null
        combine(container.engine.outbox, container.engine.workspace) { outbox, workspace -> HarnessWait.all(outbox, workspace.harnesses) }
            .collect { waits ->
                val before = known
                known = waits.map { it.clientRequestId }.toSet()
                if (before == null) return@collect
                for (wait in waits.filter { it.clientRequestId !in before }) {
                    val text = UiText.of(R.string.harness_wait_request, UiText.of(RequestLabels.of(wait.method)), wait.harnessName, wait.reason ?: UiText.of(R.string.unknown))
                    emit(UserMessage(text, UiText.of(R.string.harness_refresh)) { refresher.refreshNow(wait.harnessId) })
                }
            }
    }

    /** Snackbar messages: posted ones and the outcomes of queued requests while the shell is shown. */
    val messages: Flow<UserMessage> = merge(
        container.userMessages.messages,
        container.engine.results.mapNotNull { result ->
            ResultMessages.describe(result, container.engine.status.value.deviceId)?.let { UserMessage(it) }
        },
        harnessWaits,
    )

    fun reconnectNow() = container.reconnectNow()

    /**
     * Runs a snackbar's action (the user tapped it). In the app's scope: a snackbar outlives the
     * screen that posted it (messages queue up, a long one shows for seconds, and they wait
     * while the app is in the background), so the screen's view model may be cleared by then,
     * and so may this one right after the tap (the activity finishing).
     */
    fun runAction(action: suspend () -> Unit) {
        container.applicationScope.launch { action() }
    }
}
