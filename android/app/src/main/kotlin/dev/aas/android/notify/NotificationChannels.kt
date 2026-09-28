package dev.aas.android.notify

import android.content.Context
import androidx.core.app.NotificationChannelCompat
import androidx.core.app.NotificationManagerCompat
import dev.aas.android.R

/**
 * Notification channels (docs/ux/codex-desktop.md §8.3). Users can tune each channel in the
 * system settings; the app's own toggles (設定 → 通知) decide whether a notification is posted
 * at all.
 */
object NotificationChannels {
    /** Approval requests: heads-up, with 許可（一度だけ）/ 拒否 actions. */
    const val APPROVALS = "approvals"

    /** Questions from the agent: heads-up, opens the answer sheet. */
    const val QUESTIONS = "questions"

    /** Finished turns and clones. */
    const val TURNS = "turns"

    /** Failed turns, failed clones, requests the server rejected. */
    const val ERRORS = "errors"

    /** The connection service's persistent notification (low importance, silent). */
    const val CONNECTION = "connection"

    /**
     * The connection stopped in a way only the user can resume while the app is in the
     * background ("connected elsewhere", close code 4000): nothing else would reach the phone.
     */
    const val CONNECTION_ALERTS = "connectionAlerts"

    fun createAll(context: Context) {
        val manager = NotificationManagerCompat.from(context)
        fun channel(id: String, importance: Int, name: Int, description: Int, badge: Boolean = true) =
            NotificationChannelCompat.Builder(id, importance)
                .setName(context.getString(name))
                .setDescription(context.getString(description))
                .setShowBadge(badge)
                .build()
        manager.createNotificationChannelsCompat(
            listOf(
                channel(APPROVALS, NotificationManagerCompat.IMPORTANCE_HIGH, R.string.channel_approvals, R.string.channel_approvals_description),
                channel(QUESTIONS, NotificationManagerCompat.IMPORTANCE_HIGH, R.string.channel_questions, R.string.channel_questions_description),
                channel(TURNS, NotificationManagerCompat.IMPORTANCE_DEFAULT, R.string.channel_turns, R.string.channel_turns_description),
                channel(ERRORS, NotificationManagerCompat.IMPORTANCE_DEFAULT, R.string.channel_errors, R.string.channel_errors_description),
                channel(
                    CONNECTION, NotificationManagerCompat.IMPORTANCE_LOW, R.string.channel_connection, R.string.channel_connection_description,
                    badge = false,
                ),
                channel(
                    CONNECTION_ALERTS, NotificationManagerCompat.IMPORTANCE_DEFAULT, R.string.channel_connection_alerts,
                    R.string.channel_connection_alerts_description,
                ),
            ),
        )
    }
}
