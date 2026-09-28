package dev.aas.android.notify

import dev.aas.android.protocol.ThreadId
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.flow.asStateFlow
import kotlinx.coroutines.flow.update

/**
 * What the user sees right now: whether the app is in the foreground (`ProcessLifecycleOwner`)
 * and which thread screen is resumed. Notifications use it for "見ていないときだけ", and
 * [onThreadShown] (the notifier) drops a thread's finished-turn and error notifications as soon
 * as its screen shows (in the same call, so nothing posted afterwards is affected).
 */
class AppVisibility(private val onThreadShown: (ThreadId) -> Unit) {
    private val _appInForeground = MutableStateFlow(false)
    val appInForeground: StateFlow<Boolean> = _appInForeground.asStateFlow()

    private val _visibleThread = MutableStateFlow<ThreadId?>(null)
    val visibleThread: StateFlow<ThreadId?> = _visibleThread.asStateFlow()

    fun setAppInForeground(value: Boolean) {
        _appInForeground.value = value
    }

    /** A thread screen was resumed. */
    fun threadShown(threadId: ThreadId) {
        _visibleThread.value = threadId
        onThreadShown(threadId)
    }

    /** A thread screen was paused (only clears when it is still the visible one). */
    fun threadHidden(threadId: ThreadId) {
        _visibleThread.update { if (it == threadId) null else it }
    }

    fun isThreadVisible(threadId: ThreadId): Boolean = _appInForeground.value && _visibleThread.value == threadId
}
