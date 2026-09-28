package dev.aas.android.domain

import android.content.res.Resources
import dev.aas.android.R
import dev.aas.android.protocol.ApprovalOption
import dev.aas.android.protocol.ApprovalOptionKind
import dev.aas.android.protocol.InteractionRequest
import dev.aas.android.protocol.Subject

/** Words for interactions shared by notifications and the in-app cards. */
object InteractionTexts {
    /** One or a few lines saying what an approval is about (command, files, tool, …). */
    fun subjectPreview(res: Resources, subject: Subject, previewFiles: Int): String = when (subject) {
        is Subject.Command -> "$ ${subject.command}"
        is Subject.FileChanges -> {
            val names = subject.changes.take(previewFiles).joinToString(", ") { it.path.substringAfterLast('/').substringAfterLast('\\') }
            val more = subject.changes.size - previewFiles
            if (more > 0) {
                res.getString(R.string.subject_files_more, subject.changes.size, names, more)
            } else {
                res.getString(R.string.subject_files, subject.changes.size, names)
            }
        }
        is Subject.Tool -> res.getString(R.string.subject_tool, subject.name)
        is Subject.Plan -> subject.text
        is Subject.Permissions -> subject.description
        is Subject.Other -> subject.description
        is Subject.Unknown -> res.getString(R.string.subject_unknown, subject.type)
    }

    /** The first line of the request for a list row. */
    fun summary(res: Resources, request: InteractionRequest): String = when (request) {
        is InteractionRequest.Approval -> request.title
        is InteractionRequest.Question -> request.questions.firstOrNull()?.prompt ?: request.title
        is InteractionRequest.Unknown -> request.title.ifEmpty { res.getString(R.string.interaction_unknown_kind, request.kind) }
    }

    /**
     * The option behind the notification's 許可（一度だけ）action: the one-time allow. Broader
     * allows are only offered inside the app (UX §8.2: no wide grants from the lock screen).
     */
    fun notificationAllow(options: List<ApprovalOption>): ApprovalOption? = options.firstOrNull { it.kind == ApprovalOptionKind.AllowOnce }

    /** The option behind the 拒否 action: a plain deny (one that needs no feedback text). */
    fun notificationDeny(options: List<ApprovalOption>): ApprovalOption? = options.firstOrNull { it.kind == ApprovalOptionKind.Deny }

    /** "3 分 12 秒" / "45 秒" / "2 時間 5 分". */
    fun duration(res: Resources, ms: Long): String {
        val totalSeconds = (ms / MILLIS_PER_SECOND).coerceAtLeast(0)
        val hours = totalSeconds / SECONDS_PER_HOUR
        val minutes = (totalSeconds % SECONDS_PER_HOUR) / SECONDS_PER_MINUTE
        val seconds = totalSeconds % SECONDS_PER_MINUTE
        return when {
            hours > 0 -> res.getString(R.string.duration_hours_minutes, hours, minutes)
            minutes > 0 -> res.getString(R.string.duration_minutes_seconds, minutes, seconds)
            else -> res.getString(R.string.duration_seconds, seconds)
        }
    }

    private const val MILLIS_PER_SECOND = 1000L
    private const val SECONDS_PER_MINUTE = 60L
    private const val SECONDS_PER_HOUR = 3600L
}
