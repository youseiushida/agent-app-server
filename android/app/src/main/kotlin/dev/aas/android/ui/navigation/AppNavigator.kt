package dev.aas.android.ui.navigation

import android.content.Intent
import android.net.Uri
import androidx.navigation.NavDestination.Companion.hasRoute
import androidx.navigation.NavHostController
import androidx.navigation.toRoute

/** A target an incoming intent asks for (notification tap, `aas://pair` link). */
sealed interface IntentTarget {
    data class Thread(val threadId: String, val interactionId: String?) : IntentTarget

    data class Pair(val link: String) : IntentTarget

    companion object {
        /**
         * Reads the data URI of [intent]. The app routes these itself instead of registering
         * them as Navigation deep links: `NavController.handleDeepLink` restarts the whole task
         * for intents with `FLAG_ACTIVITY_NEW_TASK` (every notification tap), which would
         * recreate the activity.
         */
        fun of(intent: Intent?): IntentTarget? = parse(intent?.data)

        fun parse(uri: Uri?): IntentTarget? {
            if (uri == null || uri.scheme != DeepLinks.SCHEME) return null
            return when (uri.host) {
                DeepLinks.HOST_THREAD -> uri.pathSegments.firstOrNull()?.takeIf { it.isNotBlank() }?.let {
                    Thread(it, uri.getQueryParameter(DeepLinks.PARAM_INTERACTION)?.takeIf { id -> id.isNotBlank() })
                }
                DeepLinks.HOST_PAIR -> Pair(uri.toString())
                else -> null
            }
        }
    }
}

/**
 * Navigation actions of the app. Screens receive this instead of the NavController, so the
 * back-stack rules live in one place.
 */
class AppNavigator(private val nav: NavHostController) {
    fun back() {
        nav.popBackStack()
    }

    fun openTab(tab: TopLevelTab) {
        nav.navigate(tab.route) {
            // Tabs keep their own back stacks; the projects tab is the root of the main graph.
            popUpTo<ProjectsRoute> { saveState = true }
            launchSingleTop = true
            restoreState = true
        }
    }

    fun openProject(projectId: String) {
        nav.navigate(ProjectThreadsRoute(projectId))
    }

    fun openThread(threadId: String, interactionId: String? = null) {
        val route = ThreadRoute(threadId, interactionId)
        val top = nav.currentBackStackEntry
        // The same screen is already on top (a second tap on the same notification).
        if (top != null && top.destination.hasRoute<ThreadRoute>() && top.toRoute<ThreadRoute>() == route) return
        nav.navigate(route)
    }

    fun newProject() = nav.navigate(NewProjectRoute) { launchSingleTop = true }

    /**
     * A project was created or opened from the new-project flow: its threads replace the flow,
     * and the new-thread sheet opens on top when [startThread] (UX §8.2 作成すると最初のスレッド
     * の作成画面へ進む).
     */
    fun projectCreated(projectId: String, startThread: Boolean) {
        nav.navigate(ProjectThreadsRoute(projectId)) { popUpTo<NewProjectRoute> { inclusive = true } }
        if (startThread) nav.navigate(NewThreadRoute(projectId))
    }

    fun newThread(projectId: String, harnessId: String? = null) = nav.navigate(NewThreadRoute(projectId, harnessId))

    /** A thread was created from the new-thread screen: the thread replaces it. */
    fun threadCreated(threadId: String) {
        nav.navigate(ThreadRoute(threadId)) { popUpTo<NewThreadRoute> { inclusive = true } }
    }

    fun openArchived(projectId: String) = nav.navigate(ArchivedThreadsRoute(projectId))

    fun importSession(projectId: String) = nav.navigate(ImportSessionRoute(projectId))

    /** An imported session: its thread replaces the import screen. */
    fun sessionImported(threadId: String) {
        nav.navigate(ThreadRoute(threadId)) { popUpTo<ImportSessionRoute> { inclusive = true } }
    }

    fun openDiff(threadId: String, turnId: String?) = nav.navigate(DiffRoute(threadId, turnId))

    fun openOutput(threadId: String, itemId: String) = nav.navigate(ItemOutputRoute(threadId, itemId))

    fun openImage(blobId: String) = nav.navigate(ImageRoute(blobId))

    fun openDevices() = nav.navigate(DevicesRoute)

    fun openDiagnostics() = nav.navigate(DiagnosticsRoute)

    fun openBattery() = nav.navigate(BatteryRoute)

    /** Pairing on top of the current screen ([repair]) or as the only screen. */
    fun startPairing(repair: Boolean, link: String? = null) {
        nav.navigate(PairingRoute(repair, link)) {
            if (!repair) popUpTo(nav.graph.id) { inclusive = true }
            launchSingleTop = true
        }
    }

    /** A pairing completed: first time → setup, re-pairing → back to where it started. */
    fun pairingCompleted(repair: Boolean, setupCompleted: Boolean) {
        when {
            repair && nav.previousBackStackEntry != null -> nav.popBackStack()
            setupCompleted -> resetTo(ProjectsRoute)
            else -> resetTo(SetupRoute)
        }
    }

    fun setupCompleted() = resetTo(ProjectsRoute)

    /** Unpaired: only the pairing screen remains. */
    fun unpaired() = resetTo(PairingRoute())

    /** Routes an incoming intent (see [IntentTarget]). */
    fun handle(target: IntentTarget, paired: Boolean) {
        when (target) {
            is IntentTarget.Thread -> if (paired) openThread(target.threadId, target.interactionId)
            is IntentTarget.Pair -> startPairing(repair = paired, link = target.link)
        }
    }

    private fun resetTo(route: Any) {
        nav.navigate(route) {
            popUpTo(nav.graph.id) { inclusive = true }
            launchSingleTop = true
        }
    }
}
