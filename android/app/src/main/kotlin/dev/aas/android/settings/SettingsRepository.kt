package dev.aas.android.settings

import androidx.datastore.core.DataStore
import androidx.datastore.preferences.core.Preferences
import androidx.datastore.preferences.core.booleanPreferencesKey
import androidx.datastore.preferences.core.edit
import androidx.datastore.preferences.core.stringPreferencesKey
import dev.aas.android.domain.ProjectSort
import dev.aas.android.domain.composer.FollowUpDelivery
import kotlinx.coroutines.flow.Flow
import kotlinx.coroutines.flow.distinctUntilChanged
import kotlinx.coroutines.flow.first
import kotlinx.coroutines.flow.map

/** When to notify about a finished turn (docs/ux/codex-desktop.md §8.3). */
enum class TurnNotificationMode {
    /** Every finished turn. */
    Always,

    /** Unless the thread is on screen in the foreground app. */
    WhenNotViewing,

    /** Never. */
    Never,
}

/** User preferences of the app (per device). */
data class AppSettings(
    val notifyApprovals: Boolean = true,
    val notifyQuestions: Boolean = true,
    val turnNotifications: TurnNotificationMode = TurnNotificationMode.WhenNotViewing,
    val notifyErrors: Boolean = true,
    /** The post-pairing setup (notifications, battery) was completed or skipped once. */
    val setupCompleted: Boolean = false,
    /**
     * What sending while a turn runs does by default (docs/ux/codex-desktop.md §2.5
     * フォローアップの動作); long-pressing the send button does the other. The queue is the
     * default: it never changes what the agent is doing right now.
     */
    val followUp: FollowUpDelivery = FollowUpDelivery.Queue,
    /** Order of the project list. */
    val projectSort: ProjectSort = ProjectSort.Recent,
)

/** Reads and writes [AppSettings] (a DataStore file). */
class SettingsRepository(private val dataStore: DataStore<Preferences>) {
    val settings: Flow<AppSettings> = dataStore.data.map { prefs ->
        AppSettings(
            notifyApprovals = prefs[Keys.APPROVALS] ?: true,
            notifyQuestions = prefs[Keys.QUESTIONS] ?: true,
            turnNotifications = prefs[Keys.TURNS]?.let { name -> TurnNotificationMode.entries.firstOrNull { it.name == name } }
                ?: TurnNotificationMode.WhenNotViewing,
            notifyErrors = prefs[Keys.ERRORS] ?: true,
            setupCompleted = prefs[Keys.SETUP_COMPLETED] ?: false,
            followUp = prefs[Keys.FOLLOW_UP]?.let { name -> FollowUpDelivery.entries.firstOrNull { it.name == name } } ?: FollowUpDelivery.Queue,
            projectSort = prefs[Keys.PROJECT_SORT]?.let { name -> ProjectSort.entries.firstOrNull { it.name == name } } ?: ProjectSort.Recent,
        )
    }.distinctUntilChanged()

    suspend fun current(): AppSettings = settings.first()

    suspend fun setNotifyApprovals(value: Boolean) = dataStore.edit { it[Keys.APPROVALS] = value }

    suspend fun setNotifyQuestions(value: Boolean) = dataStore.edit { it[Keys.QUESTIONS] = value }

    suspend fun setTurnNotifications(value: TurnNotificationMode) = dataStore.edit { it[Keys.TURNS] = value.name }

    suspend fun setNotifyErrors(value: Boolean) = dataStore.edit { it[Keys.ERRORS] = value }

    suspend fun setSetupCompleted(value: Boolean) = dataStore.edit { it[Keys.SETUP_COMPLETED] = value }

    suspend fun setFollowUp(value: FollowUpDelivery) = dataStore.edit { it[Keys.FOLLOW_UP] = value.name }

    suspend fun setProjectSort(value: ProjectSort) = dataStore.edit { it[Keys.PROJECT_SORT] = value.name }

    private object Keys {
        val APPROVALS = booleanPreferencesKey("notifyApprovals")
        val QUESTIONS = booleanPreferencesKey("notifyQuestions")
        val TURNS = stringPreferencesKey("turnNotifications")
        val ERRORS = booleanPreferencesKey("notifyErrors")
        val SETUP_COMPLETED = booleanPreferencesKey("setupCompleted")
        val FOLLOW_UP = stringPreferencesKey("followUp")
        val PROJECT_SORT = stringPreferencesKey("projectSort")
    }
}
