package dev.aas.android.ui.common

import dev.aas.android.R
import dev.aas.android.data.HarnessRepository
import dev.aas.android.protocol.Harness
import dev.aas.android.protocol.RpcException
import dev.aas.android.sync.NotConnectedException
import kotlinx.coroutines.CancellationException
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.launch

/**
 * The 再確認 action of the screens that show harnesses (`harness/refresh`). The list itself
 * follows `harness/updated`; this reports what cannot be seen there: that the phone is offline,
 * that the server refused, and for a single harness whether it is usable now.
 */
class HarnessRefresher(private val harnesses: HarnessRepository, private val messages: UserMessages) {
    /** Probes [harnessId] (every harness when `null`) in [scope]. */
    fun refresh(scope: CoroutineScope, harnessId: String? = null) {
        scope.launch { refreshNow(harnessId) }
    }

    /** Like [refresh], waiting for the answer: the harnesses, or `null` when it failed (reported). */
    suspend fun refreshNow(harnessId: String? = null): List<Harness>? = try {
        val result = harnesses.refresh(harnessId)
        harnessId?.let { id -> result.firstOrNull { it.id == id } }?.let { harness ->
            messages.show(
                if (harness.available) {
                    UiText.of(R.string.harness_now_available, harness.displayName)
                } else {
                    UiText.of(R.string.harness_still_unavailable, harness.displayName, harness.unavailableReason ?: UiText.of(R.string.unknown))
                },
            )
        }
        result
    } catch (e: CancellationException) {
        throw e
    } catch (e: NotConnectedException) {
        messages.show(UiText.of(R.string.harness_refresh_offline))
        null
    } catch (e: RpcException) {
        messages.show(UiText.of(R.string.harness_refresh_failed, e.error.message))
        null
    } catch (e: Exception) {
        messages.show(UiText.of(R.string.harness_refresh_failed, e.message ?: e.javaClass.simpleName))
        null
    }
}
