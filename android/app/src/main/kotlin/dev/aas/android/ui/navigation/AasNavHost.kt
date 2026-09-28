package dev.aas.android.ui.navigation

import androidx.compose.runtime.Composable
import androidx.navigation.NavHostController
import androidx.navigation.compose.NavHost
import dev.aas.android.ui.diff.diffDestinations
import dev.aas.android.ui.inbox.inboxDestinations
import dev.aas.android.ui.newproject.newProjectDestinations
import dev.aas.android.ui.newthread.newThreadDestinations
import dev.aas.android.ui.pairing.pairingDestinations
import dev.aas.android.ui.projects.projectsDestinations
import dev.aas.android.ui.settings.settingsDestinations
import dev.aas.android.ui.thread.threadDestinations

/**
 * The navigation graph. Each feature package registers its destinations with one
 * `NavGraphBuilder.<feature>Destinations(navigator)` function (one line here each).
 */
@Composable
fun AasNavHost(nav: NavHostController, navigator: AppNavigator, startDestination: Any) {
    NavHost(navController = nav, startDestination = startDestination) {
        pairingDestinations(navigator)
        projectsDestinations(navigator)
        newProjectDestinations(navigator)
        newThreadDestinations(navigator)
        threadDestinations(navigator)
        diffDestinations(navigator)
        inboxDestinations(navigator)
        settingsDestinations(navigator)
    }
}
