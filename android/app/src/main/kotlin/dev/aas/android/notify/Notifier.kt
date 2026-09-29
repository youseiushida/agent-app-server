package dev.aas.android.notify

import android.Manifest
import android.app.Notification
import android.app.PendingIntent
import android.content.Context
import android.content.Intent
import android.content.pm.PackageManager
import android.net.Uri
import android.os.Build
import androidx.core.app.NotificationCompat
import androidx.core.app.NotificationManagerCompat
import androidx.core.content.ContextCompat
import dev.aas.android.AppPolicy
import dev.aas.android.MainActivity
import dev.aas.android.R
import dev.aas.android.diagnostics.ConnectionLog
import dev.aas.android.domain.ErrorTexts
import dev.aas.android.domain.InteractionTexts
import dev.aas.android.domain.RequestLabels
import dev.aas.android.protocol.BackgroundTask
import dev.aas.android.protocol.BackgroundTaskEnded
import dev.aas.android.protocol.BackgroundTaskStatus
import dev.aas.android.protocol.Interaction
import dev.aas.android.protocol.InteractionId
import dev.aas.android.protocol.InteractionRequest
import dev.aas.android.protocol.JsonKeys
import dev.aas.android.protocol.Methods
import dev.aas.android.protocol.Operation
import dev.aas.android.protocol.OperationStatus
import dev.aas.android.protocol.Thread
import dev.aas.android.protocol.ThreadId
import dev.aas.android.protocol.TurnStatus
import dev.aas.android.protocol.TurnSummary
import dev.aas.android.service.ConnectionPresentation
import dev.aas.android.service.ConnectionService
import dev.aas.android.service.ConnectionSummary
import dev.aas.android.service.ConnectionTexts
import dev.aas.android.settings.AppSettings
import dev.aas.android.settings.SettingsRepository
import dev.aas.android.settings.TurnNotificationMode
import dev.aas.android.sync.Clock
import dev.aas.android.sync.OutboxResult
import dev.aas.android.sync.SyncSignal
import dev.aas.android.sync.WorkspaceState
import dev.aas.android.ui.navigation.DeepLinks
import kotlinx.serialization.json.JsonPrimitive
import kotlinx.serialization.json.contentOrNull
import java.util.concurrent.ConcurrentHashMap

/**
 * Posts the app's notifications (docs/ux/codex-desktop.md §8.3) from the engine's signals and
 * outbox results, and builds the connection service's persistent notification.
 *
 * * Approvals: one notification per interaction with 許可（一度だけ）/ 拒否 actions (both require
 *   unlocking the device on Android 12+), removed when the interaction closes anywhere.
 * * Questions: open the thread with the answer sheet.
 * * Finished turns: one notification per thread, replaced on the next turn; failures go to the
 *   errors channel. Both go away when the thread is shown or becomes read in the app.
 * * Finished background tasks (the summary's `background.lastEnded` moved to a later end; ambient
 *   work, which the harness says is not activity, is never in it): the same one
 *   notification per thread as finished turns (tag `turn:<thread>`), so the turn the agent then
 *   starts about it replaces it instead of adding a second one; a lost task goes to the errors
 *   channel (tag `error:<thread>`). They follow the same settings as turns and failures.
 * * Clones: finished (turns channel) or failed (errors channel).
 * * Requests the server rejected definitively while the app is in the background: one
 *   notification per thread (or one for requests of no thread), showing the latest failure.
 *
 * Android silently drops every further notification of a package that has 50 posted, so
 * [AppPolicy.notificationBudget] bounds what this app keeps posted: the oldest informational
 * notifications (turns, clones, refused requests) give way first; approvals and questions are
 * never trimmed. Nothing is posted from the engine's signals without a pairing ([hasPairing]),
 * and [clearAll] removes everything when the pairing goes away.
 */
class Notifier(
    private val context: Context,
    private val settings: SettingsRepository,
    private val visibility: AppVisibility,
    private val log: ConnectionLog,
    private val workspace: () -> WorkspaceState,
    private val policy: AppPolicy,
    /** A pairing is stored (see `PairingState.hasPairing`): the engine's signals may be shown. */
    private val hasPairing: () -> Boolean,
    private val clock: Clock = Clock.System,
) {
    private val manager = NotificationManagerCompat.from(context)
    private val res get() = context.resources

    /** Interactions with a posted notification, so it can be rebuilt (sending, failed). */
    private val shown = ConcurrentHashMap<InteractionId, ShownInteraction>()

    /** [sending]: a notification action queued its answer (the notification says so, without buttons). */
    private data class ShownInteraction(val interaction: Interaction, val thread: Thread?, val backgroundTaskTitle: String?, val sending: Boolean = false)

    // ----- signals ------------------------------------------------------------------------------

    suspend fun onSignal(signal: SyncSignal) {
        if (!hasPairing()) {
            log.add(dev.aas.android.sync.SyncLogger.Level.Debug, ConnectionLog.SOURCE_NOTIFY, "not paired: no notification for $signal")
            return
        }
        val prefs = settings.current()
        when (signal) {
            is SyncSignal.InteractionPending -> showInteraction(signal.interaction, signal.thread, signal.backgroundTask?.title, prefs, error = null)
            is SyncSignal.InteractionClosed -> {
                shown.remove(signal.interactionId)
                manager.cancel(interactionTag(signal.interactionId), ID_INTERACTION)
            }
            is SyncSignal.TurnFinished -> showTurnFinished(signal.thread, signal.turn, prefs)
            is SyncSignal.BackgroundTaskFinished -> showBackgroundTaskFinished(signal.thread, signal.ended, prefs)
            is SyncSignal.InteractionTaskKnown -> nameTheTask(signal.interaction, signal.task, prefs)
            is SyncSignal.OperationFinished -> showOperationFinished(signal.operation, prefs)
            is SyncSignal.ThreadRemoved -> cancelThread(signal.threadId)
            // Shown by the open thread's screen, never notified.
            is SyncSignal.ComposerInsert, is SyncSignal.NativeSessionChanged -> Unit
        }
    }

    suspend fun onResult(result: OutboxResult) {
        if (result !is OutboxResult.Failed || !hasPairing()) return
        val entry = result.entry
        if (entry.method == Methods.InteractionRespond.name) {
            val id = (entry.params[JsonKeys.INTERACTION_ID] as? JsonPrimitive)?.contentOrNull
            val known = id?.let { shown[it] }
            if (known != null) {
                // The answer was rejected: offer the interaction again, with the reason.
                showInteraction(known.interaction, known.thread, known.backgroundTaskTitle, settings.current(), error = ErrorTexts.server(result.error).resolve(res), force = true)
                return
            }
        }
        // In the foreground the app shows the failure itself (snackbar).
        if (visibility.appInForeground.value || !settings.current().notifyErrors) return
        val text = RequestLabels.failure(res, entry, result.error)
        val threadId = entry.threadId
        val builder = base(NotificationChannels.ERRORS)
            .setContentTitle(res.getString(R.string.notify_request_failed_title))
            .setContentText(text)
            .setStyle(NotificationCompat.BigTextStyle().bigText(text))
            .setContentIntent(if (threadId != null) openThread(threadId, null) else openApp())
            .setCategory(NotificationCompat.CATEGORY_ERROR)
        // One per thread, replaced by the next failure (they would otherwise pile up).
        post(requestTag(threadId), ID_REQUEST, builder.build(), threadId)
    }

    /**
     * A thread screen is shown: its finished-turn, error and refused-request notifications have
     * served their purpose.
     */
    fun threadShown(threadId: ThreadId) {
        cancelOf(threadId, THREAD_NOTIFICATION_IDS)
    }

    /**
     * Unread states changed ([read]: the threads that are read now). Threads that became read
     * (in the app, or marked read) lose their finished-turn, error and refused-request
     * notifications, unless their screen is showing (the user chose to be notified there too).
     * [previous] is the set before, `null` for the first one of the process: then only
     * finished-turn and error notifications of read threads go (left over from before).
     */
    fun threadsRead(read: Set<ThreadId>, previous: Set<ThreadId>?) {
        val newlyRead = if (previous == null) read else read - previous
        if (newlyRead.isEmpty()) return
        val ids = if (previous == null) setOf(ID_TURN, ID_ERROR) else THREAD_NOTIFICATION_IDS
        for (sbn in manager.activeNotifications) {
            if (sbn.id !in ids) continue
            val threadId = sbn.notification.extras.getString(EXTRA_THREAD_ID) ?: continue
            if (threadId in newlyRead && !visibility.isThreadVisible(threadId)) manager.cancel(sbn.tag, sbn.id)
        }
    }

    /**
     * The pairing is gone (unpaired, or its token unreadable): every notification of it goes,
     * including approvals whose buttons would otherwise queue answers for a server this device
     * no longer belongs to.
     */
    fun clearAll() {
        shown.clear()
        for (sbn in manager.activeNotifications) {
            if (sbn.id != ID_CONNECTION) manager.cancel(sbn.tag, sbn.id)
        }
    }

    /**
     * Closes interaction notifications whose interaction is no longer pending (e.g. resolved on
     * another device while this process was not running, or wiped by an epoch change).
     */
    fun reconcileInteractions(pending: Set<InteractionId>) {
        val stale = activeTags(ID_INTERACTION).filter { it.removePrefix(TAG_INTERACTION) !in pending }
        for (tag in stale) {
            shown.remove(tag.removePrefix(TAG_INTERACTION))
            manager.cancel(tag, ID_INTERACTION)
        }
    }

    /** A notification action queued an answer: show that it is on its way (no more buttons). */
    fun markSending(interactionId: InteractionId) {
        val known = shown[interactionId]
        if (known == null) {
            manager.cancel(interactionTag(interactionId), ID_INTERACTION)
            return
        }
        shown[interactionId] = known.copy(sending = true)
        val builder = base(channelOf(known.interaction))
            .setContentTitle(titleOf(known.interaction, known.thread))
            .setContentText(res.getString(R.string.notify_answer_sending))
            .setContentIntent(openThread(known.interaction.threadId, known.interaction.id))
            .setOnlyAlertOnce(true)
            .setSilent(true)
        post(interactionTag(interactionId), ID_INTERACTION, builder.build(), known.interaction.threadId)
    }

    /** A notification action could not queue its answer (see [InteractionActionReceiver]). */
    fun showActionFailed(interactionId: InteractionId, threadId: ThreadId?, message: String) {
        showActionProblem(interactionId, threadId, res.getString(R.string.notify_answer_not_queued, message))
    }

    /**
     * A notification action arrived without a pairing (a notification left from before
     * unpairing): the answer was not queued, and the notification says why.
     */
    fun showActionNotPaired(interactionId: InteractionId) {
        shown.remove(interactionId)
        showActionProblem(interactionId, null, res.getString(R.string.notify_answer_not_paired))
    }

    private fun showActionProblem(interactionId: InteractionId, threadId: ThreadId?, text: String) {
        val builder = base(NotificationChannels.ERRORS)
            .setContentTitle(res.getString(R.string.notify_request_failed_title))
            .setContentText(text)
            .setStyle(NotificationCompat.BigTextStyle().bigText(text))
            .setContentIntent(if (threadId != null) openThread(threadId, interactionId) else openApp())
            .setCategory(NotificationCompat.CATEGORY_ERROR)
        post(interactionTag(interactionId), ID_INTERACTION, builder.build(), threadId)
    }

    // ----- the connection service's notification ------------------------------------------------

    /** The persistent notification of [ConnectionService]. */
    fun connectionNotification(presentation: ConnectionPresentation, serverName: String?): Notification {
        val now = clock.nowMs()
        val text = ConnectionTexts.describe(res, presentation, now)
        val summary = presentation.summary
        val builder = NotificationCompat.Builder(context, NotificationChannels.CONNECTION)
            .setSmallIcon(R.drawable.ic_stat_connection)
            .setContentTitle(text.title)
            .setContentText(text.detail ?: serverName)
            .setSubText(serverName)
            .setOngoing(true)
            .setOnlyAlertOnce(true)
            .setSilent(true)
            .setCategory(NotificationCompat.CATEGORY_SERVICE)
            .setPriority(NotificationCompat.PRIORITY_LOW)
            .setContentIntent(openApp())
        if (summary is ConnectionSummary.WaitingToRetry) {
            // A countdown to the next attempt without updating the notification every second.
            builder.setWhen(summary.retryAtMs).setShowWhen(true).setUsesChronometer(true).setChronometerCountDown(true)
        } else {
            builder.setShowWhen(false)
        }
        val canRetry = summary is ConnectionSummary.WaitingToRetry || summary is ConnectionSummary.ConnectedElsewhere ||
            summary is ConnectionSummary.Incompatible || summary is ConnectionSummary.Connecting ||
            summary is ConnectionSummary.KeystoreUnavailable
        if (canRetry) builder.addAction(R.drawable.ic_action_refresh, res.getString(R.string.action_reconnect_now), reconnectIntent())
        return builder.build()
    }

    /**
     * "Connected elsewhere" (close code 4000) while the app is in the background: the engine does
     * not take the connection back on its own (two holders of one token would take turns forever),
     * so until the user acts no approval or result reaches this phone. This notification says so
     * and offers 再接続, which reconnects without opening the app: a notification action may
     * start the foreground service from the background (Android 12+ exemption). Opening the app
     * reconnects too (coming to the foreground resumes).
     */
    fun showConnectedElsewhere(serverName: String?) {
        val text = res.getString(R.string.notify_connected_elsewhere_body)
        val builder = base(NotificationChannels.CONNECTION_ALERTS)
            .setContentTitle(res.getString(R.string.notify_connected_elsewhere_title))
            .setContentText(text)
            .setSubText(serverName)
            .setStyle(NotificationCompat.BigTextStyle().bigText(text))
            .setContentIntent(openApp())
            .setCategory(NotificationCompat.CATEGORY_STATUS)
            .setOnlyAlertOnce(true)
            .addAction(
                NotificationCompat.Action.Builder(R.drawable.ic_action_refresh, res.getString(R.string.action_reconnect), reconnectIntent())
                    .setShowsUserInterface(false)
                    .build(),
            )
        post(TAG_CONNECTED_ELSEWHERE, ID_CONNECTION_ALERT, builder.build(), null)
    }

    /** The connection is back (or the app is in the foreground, where the banner says it). */
    fun cancelConnectedElsewhere() {
        manager.cancel(TAG_CONNECTED_ELSEWHERE, ID_CONNECTION_ALERT)
    }

    /**
     * 再接続 from a notification: starts (or reaches) the connection service, which asks the
     * engine to reconnect at once. A foreground-service start, allowed from the background
     * because the user tapped a notification action.
     */
    private fun reconnectIntent(): PendingIntent = PendingIntent.getForegroundService(
        context,
        0,
        Intent(context, ConnectionService::class.java).setAction(ConnectionService.ACTION_RECONNECT),
        PendingIntent.FLAG_IMMUTABLE or PendingIntent.FLAG_UPDATE_CURRENT,
    )

    // ----- building -----------------------------------------------------------------------------

    /**
     * [backgroundTaskTitle]: the title of the background task that asked, when it is stored on
     * this device; a request of a background task says so either way.
     */
    private fun showInteraction(
        interaction: Interaction,
        thread: Thread?,
        backgroundTaskTitle: String?,
        prefs: AppSettings,
        error: String?,
        force: Boolean = false,
    ) {
        val request = interaction.request
        val enabled = when (request) {
            is InteractionRequest.Question -> prefs.notifyQuestions
            is InteractionRequest.Approval, is InteractionRequest.Unknown -> prefs.notifyApprovals
        }
        shown[interaction.id] = ShownInteraction(interaction, thread, backgroundTaskTitle)
        if (!enabled && !force) return
        val threadVisible = visibility.isThreadVisible(interaction.threadId)
        val summary = InteractionTexts.summary(res, request)
        val body = buildString {
            if (interaction.backgroundTaskId != null) {
                append(
                    backgroundTaskTitle?.let { res.getString(R.string.notify_from_background, it) }
                        ?: res.getString(R.string.notify_from_background_unknown),
                ).append("\n")
            }
            append(summary)
            if (request is InteractionRequest.Approval) {
                request.detail?.takeIf { it.isNotBlank() }?.let { append("\n").append(it) }
                append("\n").append(InteractionTexts.subjectPreview(res, request.subject, policy.display.previewFiles))
            }
            if (error != null) append("\n").append(res.getString(R.string.notify_answer_rejected, error))
        }
        val builder = base(channelOf(interaction))
            .setContentTitle(titleOf(interaction, thread))
            .setContentText(summary)
            .setStyle(NotificationCompat.BigTextStyle().bigText(body))
            .setContentIntent(openThread(interaction.threadId, interaction.id))
            .setCategory(NotificationCompat.CATEGORY_REMINDER)
            .setOnlyAlertOnce(true)
            // The thread is on screen: its card shows the request; no heads-up or sound.
            .setSilent(threadVisible)
            .setWhen(interaction.createdAt)
            .setShowWhen(true)
            .setVisibility(NotificationCompat.VISIBILITY_PRIVATE)
            .setPublicVersion(
                base(channelOf(interaction))
                    .setContentTitle(res.getString(if (request is InteractionRequest.Question) R.string.notify_question_public else R.string.notify_approval_public))
                    .build(),
            )
        if (request is InteractionRequest.Approval) {
            InteractionTexts.notificationDeny(request.options)?.let { option ->
                builder.addAction(action(R.drawable.ic_action_deny, R.string.action_deny, interaction, option.id))
            }
            InteractionTexts.notificationAllow(request.options)?.let { option ->
                builder.addAction(action(R.drawable.ic_action_allow, R.string.action_allow_once, interaction, option.id))
            }
        }
        post(interactionTag(interaction.id), ID_INTERACTION, builder.build(), interaction.threadId)
    }

    /**
     * The background task that asked [interaction] became known after its notification was
     * posted without the task's title: the notification is posted again with it, without alerting
     * (`setOnlyAlertOnce`). One the user dismissed stays dismissed; one that shows an answer on its
     * way keeps saying so.
     */
    private fun nameTheTask(interaction: Interaction, task: BackgroundTask, prefs: AppSettings) {
        val known = shown[interaction.id] ?: return
        if (known.backgroundTaskTitle != null || known.sending) return
        shown[interaction.id] = known.copy(backgroundTaskTitle = task.title)
        if (interactionTag(interaction.id) !in activeTags(ID_INTERACTION)) return
        showInteraction(known.interaction, known.thread, task.title, prefs, error = null)
    }

    private fun action(icon: Int, label: Int, interaction: Interaction, optionId: String): NotificationCompat.Action {
        val intent = InteractionActionReceiver.intent(context, interaction.id, interaction.threadId, optionId)
        val pending = PendingIntent.getBroadcast(context, 0, intent, PendingIntent.FLAG_IMMUTABLE or PendingIntent.FLAG_UPDATE_CURRENT)
        return NotificationCompat.Action.Builder(icon, res.getString(label), pending)
            // Answering an approval from the lock screen requires unlocking first (Android 12+).
            .setAuthenticationRequired(true)
            .setShowsUserInterface(false)
            .build()
    }

    private fun showTurnFinished(thread: Thread, turn: TurnSummary, prefs: AppSettings) {
        val failed = turn.status == TurnStatus.Failed
        if (failed) {
            if (!prefs.notifyErrors || visibility.isThreadVisible(thread.id)) return
        } else {
            when (prefs.turnNotifications) {
                TurnNotificationMode.Never -> return
                TurnNotificationMode.WhenNotViewing -> if (visibility.isThreadVisible(thread.id)) return
                TurnNotificationMode.Always -> Unit
            }
        }
        val duration = turn.completedAt?.let { InteractionTexts.duration(res, it - turn.startedAt) }
        val text = when (turn.status) {
            TurnStatus.Completed -> if (duration != null) res.getString(R.string.notify_turn_completed_after, duration) else res.getString(R.string.notify_turn_completed)
            TurnStatus.Interrupted -> res.getString(R.string.notify_turn_interrupted)
            TurnStatus.Failed -> res.getString(
                R.string.notify_turn_failed,
                thread.lastError?.let { ErrorTexts.turnErrorLine(it.kind, it.message).resolve(res) } ?: res.getString(R.string.error_unknown),
            )
            TurnStatus.Running, TurnStatus.Unknown -> res.getString(R.string.notify_turn_ended)
        }
        val channel = if (failed) NotificationChannels.ERRORS else NotificationChannels.TURNS
        val builder = base(channel)
            .setContentTitle(thread.title)
            .setContentText(text)
            .setStyle(NotificationCompat.BigTextStyle().bigText(text))
            .setContentIntent(openThread(thread.id, null))
            .setCategory(if (failed) NotificationCompat.CATEGORY_ERROR else NotificationCompat.CATEGORY_STATUS)
            .setWhen(turn.completedAt ?: clock.nowMs())
            .setShowWhen(true)
        post(if (failed) errorTag(thread.id) else turnTag(thread.id), if (failed) ID_ERROR else ID_TURN, builder.build(), thread.id)
    }

    /**
     * A background task ended ([ended], the summary's `lastEnded`). Completed, failed and stopped
     * tasks follow the finished-turn setting and share the thread's turn notification (the turn
     * the agent may start about it replaces it); a lost task (its process ended, or the daemon
     * restarted) is an error and follows the error setting.
     */
    private fun showBackgroundTaskFinished(thread: Thread, ended: BackgroundTaskEnded, prefs: AppSettings) {
        val lost = ended.status == BackgroundTaskStatus.Lost
        if (lost) {
            if (!prefs.notifyErrors || visibility.isThreadVisible(thread.id)) return
        } else {
            when (prefs.turnNotifications) {
                TurnNotificationMode.Never -> return
                TurnNotificationMode.WhenNotViewing -> if (visibility.isThreadVisible(thread.id)) return
                TurnNotificationMode.Always -> Unit
            }
        }
        val text = res.getString(
            when (ended.status) {
                BackgroundTaskStatus.Completed -> R.string.notify_bg_completed
                BackgroundTaskStatus.Failed -> R.string.notify_bg_failed
                BackgroundTaskStatus.Stopped -> R.string.notify_bg_stopped
                BackgroundTaskStatus.Lost -> R.string.notify_bg_lost
                BackgroundTaskStatus.Running, BackgroundTaskStatus.Unknown -> R.string.notify_bg_ended
            },
            ended.title,
        )
        val channel = if (lost) NotificationChannels.ERRORS else NotificationChannels.TURNS
        val builder = base(channel)
            .setContentTitle(thread.title)
            .setContentText(text)
            .setStyle(NotificationCompat.BigTextStyle().bigText(text))
            .setContentIntent(openThread(thread.id, null))
            .setCategory(if (lost) NotificationCompat.CATEGORY_ERROR else NotificationCompat.CATEGORY_STATUS)
            .setWhen(ended.endedAt)
            .setShowWhen(true)
        post(if (lost) errorTag(thread.id) else turnTag(thread.id), if (lost) ID_ERROR else ID_TURN, builder.build(), thread.id)
    }

    private fun showOperationFinished(operation: Operation, prefs: AppSettings) {
        val project = operation.projectId?.let { id -> workspace().projects.firstOrNull { it.id == id } }
        when (operation.status) {
            OperationStatus.Succeeded -> {
                if (prefs.turnNotifications == TurnNotificationMode.Never || visibility.appInForeground.value) return
                val text = res.getString(R.string.notify_clone_succeeded, project?.name ?: operation.message.orEmpty())
                val builder = base(NotificationChannels.TURNS)
                    .setContentTitle(res.getString(R.string.notify_clone_title))
                    .setContentText(text)
                    .setContentIntent(openApp())
                    .setCategory(NotificationCompat.CATEGORY_STATUS)
                post(operationTag(operation.id), ID_OPERATION, builder.build(), null)
            }
            OperationStatus.Failed, OperationStatus.Unknown -> {
                if (!prefs.notifyErrors) return
                val text = res.getString(R.string.notify_clone_failed, operation.message ?: res.getString(R.string.error_unknown))
                val builder = base(NotificationChannels.ERRORS)
                    .setContentTitle(res.getString(R.string.notify_clone_title))
                    .setContentText(text)
                    .setStyle(NotificationCompat.BigTextStyle().bigText(text))
                    .setContentIntent(openApp())
                    .setCategory(NotificationCompat.CATEGORY_ERROR)
                post(operationTag(operation.id), ID_OPERATION, builder.build(), null)
            }
            // Cancelled by the user; running is not a finish.
            OperationStatus.Cancelled, OperationStatus.Running -> Unit
        }
    }

    private fun cancelThread(threadId: ThreadId) {
        for (sbn in manager.activeNotifications) {
            if (sbn.notification.extras.getString(EXTRA_THREAD_ID) == threadId) manager.cancel(sbn.tag, sbn.id)
        }
        shown.values.removeAll { it.interaction.threadId == threadId }
    }

    private fun cancelOf(threadId: ThreadId, ids: Set<Int>) {
        for (sbn in manager.activeNotifications) {
            if (sbn.id in ids && sbn.notification.extras.getString(EXTRA_THREAD_ID) == threadId) manager.cancel(sbn.tag, sbn.id)
        }
    }

    /**
     * Keeps the posted notifications within [AppPolicy.notificationBudget] before one more is
     * posted: the oldest informational ones go first. Replacing a posted notification adds none.
     */
    private fun makeRoom(tag: String, id: Int) {
        val active = manager.activeNotifications
        if (active.any { it.id == id && it.tag == tag }) return
        val excess = active.size + 1 - policy.notificationBudget
        if (excess <= 0) return
        val trimmed = active.filter { it.id in TRIMMABLE_IDS }.sortedBy { it.postTime }.take(excess)
        trimmed.forEach { manager.cancel(it.tag, it.id) }
        log.warn(
            ConnectionLog.SOURCE_NOTIFY,
            "${active.size} notifications posted (budget ${policy.notificationBudget}): removed the ${trimmed.size} oldest informational ones" +
                if (trimmed.size < excess) "; the rest are approvals and questions, the system limit is close" else "",
        )
    }

    private fun activeTags(id: Int): List<String> = manager.activeNotifications.filter { it.id == id && it.tag != null }.map { it.tag }

    private fun channelOf(interaction: Interaction): String =
        if (interaction.request is InteractionRequest.Question) NotificationChannels.QUESTIONS else NotificationChannels.APPROVALS

    private fun titleOf(interaction: Interaction, thread: Thread?): String {
        val name = thread?.title ?: res.getString(R.string.thread_unknown)
        return res.getString(if (interaction.request is InteractionRequest.Question) R.string.notify_question_title else R.string.notify_approval_title, name)
    }

    private fun base(channel: String): NotificationCompat.Builder = NotificationCompat.Builder(context, channel)
        .setSmallIcon(R.drawable.ic_stat_connection)
        .setAutoCancel(true)
        .setColor(ContextCompat.getColor(context, R.color.brand))

    private fun openApp(): PendingIntent = PendingIntent.getActivity(
        context,
        0,
        Intent(context, MainActivity::class.java).addFlags(Intent.FLAG_ACTIVITY_NEW_TASK or Intent.FLAG_ACTIVITY_SINGLE_TOP),
        PendingIntent.FLAG_IMMUTABLE or PendingIntent.FLAG_UPDATE_CURRENT,
    )

    private fun openThread(threadId: ThreadId, interactionId: InteractionId?): PendingIntent = PendingIntent.getActivity(
        context,
        0,
        threadIntent(context, DeepLinks.thread(threadId, interactionId)),
        PendingIntent.FLAG_IMMUTABLE or PendingIntent.FLAG_UPDATE_CURRENT,
    )

    /** Whether the app may post notifications (runtime permission on 13+, and not blocked). */
    fun canPost(): Boolean {
        if (!manager.areNotificationsEnabled()) return false
        return Build.VERSION.SDK_INT < Build.VERSION_CODES.TIRAMISU ||
            ContextCompat.checkSelfPermission(context, Manifest.permission.POST_NOTIFICATIONS) == PackageManager.PERMISSION_GRANTED
    }

    private fun post(tag: String, id: Int, notification: Notification, threadId: ThreadId?) {
        if (threadId != null) notification.extras.putString(EXTRA_THREAD_ID, threadId)
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.TIRAMISU &&
            ContextCompat.checkSelfPermission(context, Manifest.permission.POST_NOTIFICATIONS) != PackageManager.PERMISSION_GRANTED
        ) {
            log.add(dev.aas.android.sync.SyncLogger.Level.Debug, ConnectionLog.SOURCE_NOTIFY, "not posted ($tag): notification permission not granted")
            return
        }
        try {
            makeRoom(tag, id)
            manager.notify(tag, id, notification)
        } catch (e: SecurityException) {
            // The permission was revoked between the check and the call.
            log.warn(ConnectionLog.SOURCE_NOTIFY, "posting $tag failed", e)
        }
    }

    companion object {
        /** The connection service's foreground notification. */
        const val ID_CONNECTION = 1
        const val ID_INTERACTION = 10
        const val ID_TURN = 20
        const val ID_ERROR = 30
        const val ID_OPERATION = 40
        const val ID_REQUEST = 50

        /** "Connected elsewhere" while in the background ([showConnectedElsewhere]). */
        const val ID_CONNECTION_ALERT = 60
        const val TAG_CONNECTED_ELSEWHERE = "connection:elsewhere"

        private const val TAG_INTERACTION = "interaction:"
        const val EXTRA_THREAD_ID = "dev.aas.android.threadId"

        fun interactionTag(id: InteractionId) = "$TAG_INTERACTION$id"

        fun turnTag(threadId: ThreadId) = "turn:$threadId"

        fun errorTag(threadId: ThreadId) = "error:$threadId"

        fun operationTag(id: String) = "operation:$id"

        /** Refused requests of [threadId] (or of no thread) share one notification. */
        fun requestTag(threadId: ThreadId?) = if (threadId != null) "request:$threadId" else "request:app"

        /** The notifications about a thread's outcome (not its approvals and questions). */
        private val THREAD_NOTIFICATION_IDS = setOf(ID_TURN, ID_ERROR, ID_REQUEST)

        /** What may be removed to stay within the budget: everything but approvals, questions and the service's own. */
        private val TRIMMABLE_IDS = setOf(ID_TURN, ID_ERROR, ID_OPERATION, ID_REQUEST)

        /** The explicit intent that opens [uri] (a [DeepLinks] URI) in [MainActivity]. */
        fun threadIntent(context: Context, uri: Uri): Intent = Intent(Intent.ACTION_VIEW, uri, context, MainActivity::class.java)
            .addFlags(Intent.FLAG_ACTIVITY_NEW_TASK or Intent.FLAG_ACTIVITY_SINGLE_TOP)
    }
}
