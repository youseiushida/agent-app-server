package dev.aas.android.ui.common

import android.content.res.Resources
import androidx.annotation.StringRes
import androidx.compose.runtime.Composable
import androidx.compose.ui.platform.LocalResources
import dev.aas.android.R
import dev.aas.android.sync.CallTimeoutException
import dev.aas.android.sync.ConnectionLostException
import kotlinx.coroutines.channels.BufferOverflow
import kotlinx.coroutines.channels.Channel
import kotlinx.coroutines.flow.Flow
import kotlinx.coroutines.flow.receiveAsFlow

/**
 * Text produced outside the UI (view models, repositories) and resolved against resources when
 * shown, so view models never hold a Context.
 */
sealed interface UiText {
    data class Res(@param:StringRes val id: Int, val args: List<Any> = emptyList()) : UiText

    /** Text that is already final (a server message). */
    data class Plain(val text: String) : UiText

    fun resolve(res: Resources): String = when (this) {
        is Res -> if (args.isEmpty()) res.getString(id) else res.getString(id, *args.map { if (it is UiText) it.resolve(res) else it }.toTypedArray())
        is Plain -> text
    }

    companion object {
        fun of(@StringRes id: Int, vararg args: Any): UiText = Res(id, args.toList())
    }
}

@Composable
fun UiText.asString(): String = resolve(LocalResources.current)

/**
 * A request that failed without an error answer from the server (the callers handle
 * `RpcException` and `NotConnectedException` themselves): the sync engine's own failures in the
 * app's words (their messages are English, for logs), anything else with its message.
 */
fun requestFailed(e: Exception): UiText = when (e) {
    is ConnectionLostException -> UiText.of(R.string.error_connection_lost)
    is CallTimeoutException -> UiText.of(R.string.error_no_response)
    else -> UiText.of(R.string.error_request, e.message ?: e.javaClass.simpleName)
}

/**
 * A message for the app-wide snackbar, with an optional [action] (its label) and [onAction].
 *
 * The shell runs [onAction] in the app's scope (`ShellViewModel.runAction`), not in the scope of
 * the screen that posted the message: snackbars queue up and wait while the app is in the
 * background, so the action may be tapped after that screen is gone (its view model cleared,
 * where a launch would silently do nothing). [onAction] must therefore not use a view model's
 * scope, and reports its own failures.
 */
data class UserMessage(val text: UiText, val action: UiText? = null, val onAction: (suspend () -> Unit)? = null)

/**
 * The app-wide snackbar queue: view models and the shell post here; the shell's
 * `SnackbarHost` shows them one after another.
 */
class UserMessages {
    private val channel = Channel<UserMessage>(CAPACITY, BufferOverflow.DROP_OLDEST)
    val messages: Flow<UserMessage> = channel.receiveAsFlow()

    /** Never fails: beyond [CAPACITY] unseen messages the oldest are dropped (they are stale by then). */
    fun show(message: UserMessage) {
        channel.trySend(message)
    }

    fun show(text: UiText) = show(UserMessage(text))

    private companion object {
        /** Unseen messages kept while no snackbar host collects (the app is in the background). */
        const val CAPACITY = 16
    }
}
