package dev.aas.android.ui.navigation

import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.flow.asStateFlow
import kotlinx.coroutines.flow.update

/**
 * Intents for the navigation graph (notification taps, `aas://pair` links) that are not
 * handled yet, oldest first.
 *
 * The graph can navigate only after the activity's first composition, while Android delivers
 * `onNewIntent` to a recreated activity before that (between `onStart` and `onResume`). So an
 * intent waits here until the graph took it ([handled]) instead of being offered once to
 * whoever listens at that moment. The activity saves the ones still waiting in its instance
 * state, so they also outlive the activity being recreated before it handled them.
 */
class PendingIntents<T : Any>(initial: List<T> = emptyList()) {
    private val _items = MutableStateFlow(initial)

    /** The intents waiting to be handled. */
    val items: StateFlow<List<T>> = _items.asStateFlow()

    fun add(item: T) {
        _items.update { it + item }
    }

    /** [item] was handled (this very instance: equal intents delivered twice are two taps). */
    fun handled(item: T) {
        _items.update { list ->
            val at = list.indexOfFirst { it === item }
            if (at < 0) list else list.subList(0, at) + list.subList(at + 1, list.size)
        }
    }
}
