package dev.aas.android.ui

import android.content.Intent
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.WindowInsets
import androidx.compose.foundation.layout.consumeWindowInsets
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.statusBarsPadding
import androidx.compose.material3.Badge
import androidx.compose.material3.BadgedBox
import androidx.compose.material3.Icon
import androidx.compose.material3.NavigationBar
import androidx.compose.material3.NavigationBarItem
import androidx.compose.material3.Scaffold
import androidx.compose.material3.ScaffoldDefaults
import androidx.compose.material3.SnackbarDuration
import androidx.compose.material3.SnackbarHost
import androidx.compose.material3.SnackbarHostState
import androidx.compose.material3.SnackbarResult
import androidx.compose.material3.Surface
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.CompositionLocalProvider
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.remember
import androidx.compose.ui.Modifier
import androidx.compose.ui.platform.LocalResources
import androidx.compose.ui.res.stringResource
import androidx.compose.ui.semantics.contentDescription
import androidx.compose.ui.semantics.semantics
import androidx.lifecycle.Lifecycle
import androidx.lifecycle.compose.LocalLifecycleOwner
import androidx.lifecycle.compose.collectAsStateWithLifecycle
import androidx.lifecycle.repeatOnLifecycle
import androidx.navigation.NavDestination.Companion.hasRoute
import androidx.navigation.compose.currentBackStackEntryAsState
import androidx.navigation.compose.rememberNavController
import dev.aas.android.AppContainer
import dev.aas.android.R
import dev.aas.android.security.PairingState
import dev.aas.android.security.info
import dev.aas.android.ui.common.LocalAppContainer
import dev.aas.android.ui.common.LocalAppPolicy
import dev.aas.android.ui.common.aasViewModel
import dev.aas.android.ui.navigation.AasNavHost
import dev.aas.android.ui.navigation.AppNavigator
import dev.aas.android.ui.navigation.IntentTarget
import dev.aas.android.ui.navigation.PairingRoute
import dev.aas.android.ui.navigation.PendingIntents
import dev.aas.android.ui.navigation.ProjectsRoute
import dev.aas.android.ui.navigation.SetupRoute
import dev.aas.android.ui.navigation.TopLevelTab
import dev.aas.android.ui.shell.ConnectionAttentionBanner
import dev.aas.android.ui.shell.ConnectionStatusBar
import dev.aas.android.ui.shell.ShellViewModel

/**
 * The root of the UI: waits for the stored pairing, then shows the shell — connection strip and
 * attention banner on top, the navigation graph, the bottom navigation on tab screens, and the
 * app-wide snackbar.
 */
@Composable
fun AasApp(container: AppContainer, intents: PendingIntents<Intent>) {
    CompositionLocalProvider(LocalAppContainer provides container, LocalAppPolicy provides container.policy) {
        val pairing by container.pairingState.collectAsStateWithLifecycle()
        val loaded = pairing
        if (loaded == null) {
            // Reading the pairing from disk takes milliseconds; show the window background.
            Surface(Modifier.fillMaxSize()) {}
            return@CompositionLocalProvider
        }
        val startDestination: Any = remember { if (loaded is PairingState.NotPaired) PairingRoute() else ProjectsRoute }
        Shell(container, startDestination, loaded, intents)
    }
}

@Composable
private fun Shell(
    container: AppContainer,
    startDestination: Any,
    pairing: PairingState,
    intents: PendingIntents<Intent>,
) {
    val nav = rememberNavController()
    val navigator = remember(nav) { AppNavigator(nav) }
    val shellViewModel = aasViewModel { c, _ -> ShellViewModel(c) }
    val shell by shellViewModel.state.collectAsStateWithLifecycle()
    val backStackEntry by nav.currentBackStackEntryAsState()
    val destination = backStackEntry?.destination
    val currentTab = TopLevelTab.of(destination)
    val inPairingFlow = if (destination == null) {
        startDestination is PairingRoute
    } else {
        destination.hasRoute(PairingRoute::class) || destination.hasRoute(SetupRoute::class)
    }
    val snackbarHostState = remember { SnackbarHostState() }
    val res = LocalResources.current

    // The NavController accepts navigation once NavHost has set its graph (first entry shown).
    val graphReady = destination != null
    // Incoming intents (the one that started the activity, those delivered to onNewIntent),
    // each taken out only once handled: they wait while the graph is not ready yet.
    LaunchedEffect(graphReady) {
        if (!graphReady) return@LaunchedEffect
        intents.items.collect { waiting ->
            for (intent in waiting) {
                // With a stored pairing (usable or not) the app shows its stored data.
                val stored = container.pairingState.value?.info != null
                IntentTarget.of(intent)?.let { navigator.handle(it, stored) }
                intents.handled(intent)
            }
        }
    }
    // Unpaired elsewhere (settings): only the pairing screen remains.
    LaunchedEffect(pairing, graphReady) {
        if (graphReady && pairing is PairingState.NotPaired && !inPairingFlow) navigator.unpaired()
    }
    // Snackbars only while the app is visible; in the background the Notifier reports failures.
    val lifecycleOwner = LocalLifecycleOwner.current
    LaunchedEffect(shellViewModel, lifecycleOwner) {
        lifecycleOwner.repeatOnLifecycle(Lifecycle.State.STARTED) {
            shellViewModel.messages.collect { message ->
                val result = snackbarHostState.showSnackbar(
                    message = message.text.resolve(res),
                    actionLabel = message.action?.resolve(res),
                    duration = if (message.action != null) SnackbarDuration.Long else SnackbarDuration.Short,
                )
                if (result == SnackbarResult.ActionPerformed) message.onAction?.let(shellViewModel::runAction)
            }
        }
    }

    Scaffold(
        topBar = {
            if (!inPairingFlow) {
                Column(Modifier.statusBarsPadding()) {
                    ConnectionStatusBar(
                        presentation = shell.presentation,
                        onReconnect = shellViewModel::reconnectNow,
                        onOpenDiagnostics = navigator::openDiagnostics,
                    )
                    ConnectionAttentionBanner(
                        presentation = shell.presentation,
                        tokenUnreadable = shell.tokenUnreadable,
                        onReconnect = shellViewModel::reconnectNow,
                        onRepair = { navigator.startPairing(repair = true) },
                    )
                }
            }
        },
        bottomBar = {
            if (currentTab != null) {
                NavigationBar {
                    for (tab in TopLevelTab.entries) {
                        val badge = if (tab == TopLevelTab.Inbox) shell.inboxBadge else 0
                        val label = stringResource(tab.label)
                        val badgedLabel = stringResource(R.string.tab_badge, label, badge)
                        NavigationBarItem(
                            selected = tab == currentTab,
                            onClick = { navigator.openTab(tab) },
                            // Material 3 clears the icon's semantics (the badge with it) when the
                            // item has a label: the count is part of the item's accessible name.
                            modifier = if (badge > 0) Modifier.semantics { contentDescription = badgedLabel } else Modifier,
                            icon = {
                                if (badge > 0) {
                                    BadgedBox(badge = { Badge { Text(badge.toString()) } }) { Icon(tab.icon, contentDescription = null) }
                                } else {
                                    Icon(tab.icon, contentDescription = null)
                                }
                            },
                            label = { Text(label) },
                        )
                    }
                }
            }
        },
        snackbarHost = { SnackbarHost(snackbarHostState) },
        // The pairing screens draw edge to edge themselves (they have no strip above them).
        contentWindowInsets = if (inPairingFlow) WindowInsets(0) else ScaffoldDefaults.contentWindowInsets,
    ) { padding ->
        Box(Modifier.fillMaxSize().padding(padding).consumeWindowInsets(padding)) {
            AasNavHost(nav, navigator, startDestination)
        }
    }
}
