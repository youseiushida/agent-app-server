package dev.aas.android.ui.common

import androidx.compose.runtime.Composable
import androidx.compose.runtime.staticCompositionLocalOf
import androidx.lifecycle.SavedStateHandle
import androidx.lifecycle.ViewModel
import androidx.lifecycle.createSavedStateHandle
import androidx.lifecycle.viewmodel.compose.viewModel
import androidx.lifecycle.viewmodel.initializer
import androidx.lifecycle.viewmodel.viewModelFactory
import dev.aas.android.AppContainer
import dev.aas.android.AppPolicy

/** The process's [AppContainer], provided at the root of the composition (AasApp). */
val LocalAppContainer = staticCompositionLocalOf<AppContainer> { error("LocalAppContainer is not provided") }

/**
 * The app's policy values for composables ([AppPolicy.display] above all), provided at the root
 * with the container's policy. Components rendered on their own (previews, UI tests of a single
 * item) get the defaults, which are the app's values too.
 */
val LocalAppPolicy = staticCompositionLocalOf { AppPolicy() }

/**
 * Creates a view model of the current navigation entry with its dependencies from the
 * [AppContainer]. [SavedStateHandle] carries the typed route (`handle.toRoute<ThreadRoute>()`).
 *
 * ```
 * val vm = aasViewModel { container, handle -> ThreadViewModel(handle.toRoute(), container.threadRepository, …) }
 * ```
 */
@Composable
inline fun <reified VM : ViewModel> aasViewModel(
    key: String? = null,
    crossinline create: (container: AppContainer, handle: SavedStateHandle) -> VM,
): VM {
    val container = LocalAppContainer.current
    return viewModel(
        key = key,
        factory = viewModelFactory { initializer { create(container, createSavedStateHandle()) } },
    )
}
