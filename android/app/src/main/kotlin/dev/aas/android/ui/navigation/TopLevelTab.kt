package dev.aas.android.ui.navigation

import androidx.annotation.StringRes
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.outlined.Settings
import androidx.compose.ui.graphics.vector.ImageVector
import androidx.navigation.NavDestination
import androidx.navigation.NavDestination.Companion.hasRoute
import dev.aas.android.R
import dev.aas.android.ui.icons.Folder
import dev.aas.android.ui.icons.Inbox
import kotlin.reflect.KClass

/**
 * The bottom navigation tabs (UX §8.2): プロジェクト / 要対応 / 設定. [members] are the routes
 * that belong to a tab's stack (the tab stays selected and the bar stays visible on them).
 */
enum class TopLevelTab(
    val route: Any,
    @param:StringRes val label: Int,
    val icon: ImageVector,
    private val members: List<KClass<*>>,
) {
    Projects(ProjectsRoute, R.string.tab_projects, Icons.Outlined.Folder, listOf(ProjectsRoute::class, ProjectThreadsRoute::class, ArchivedThreadsRoute::class)),
    Inbox(InboxRoute, R.string.tab_inbox, Icons.Outlined.Inbox, listOf(InboxRoute::class)),
    Settings(
        SettingsRoute, R.string.tab_settings, Icons.Outlined.Settings,
        listOf(SettingsRoute::class, DevicesRoute::class, DiagnosticsRoute::class, BatteryRoute::class),
    ),
    ;

    fun contains(destination: NavDestination?): Boolean =
        destination != null && members.any { destination.hasRoute(it) }

    companion object {
        /** The tab whose stack shows [destination], or null (thread screen, pairing: no bar). */
        fun of(destination: NavDestination?): TopLevelTab? = entries.firstOrNull { it.contains(destination) }
    }
}
