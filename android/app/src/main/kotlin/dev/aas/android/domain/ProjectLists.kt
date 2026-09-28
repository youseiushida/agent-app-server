package dev.aas.android.domain

import dev.aas.android.protocol.Harness
import dev.aas.android.protocol.Interaction
import dev.aas.android.protocol.InteractionRequest
import dev.aas.android.protocol.InteractionStatus
import dev.aas.android.protocol.Project
import dev.aas.android.protocol.Thread
import dev.aas.android.sync.WorkspaceState

/** How the project list is ordered (設定 of the list, per device). */
enum class ProjectSort {
    /** Most recent activity of the project's threads (or the project itself) first. */
    Recent,

    /** By name, case-insensitive. */
    Name,
}

/** A project in the list with the summary of its (non-archived) threads. */
data class ProjectRow(
    val project: Project,
    val threads: Int,
    /** The most urgent status among its threads (承認が必要 > 入力が必要 > エラー > 実行中 > バックグラウンド > 待機中). */
    val activity: ThreadActivity,
    val approvals: Int,
    val questions: Int,
    /** Threads whose agent works (a turn or background work). */
    val running: Int,
    /** Background tasks running in threads whose status is バックグラウンドで実行中 (the chip's count). */
    val backgroundRunning: Int,
    val errors: Int,
    val unread: Int,
    /** Messages waiting in the daemon's queues of its threads (`Thread.queuedInputs`). */
    val queued: Int,
    val lastActivityAt: Long,
)

/** A thread in a project's list. */
data class ThreadRow(
    val thread: Thread,
    val activity: ThreadActivity,
    val unread: Boolean,
    val harnessName: String?,
    /** Pending approvals and questions of this thread (from the workspace's pending interactions). */
    val approvals: Int,
    val questions: Int,
) {
    val id: String get() = thread.id
    val title: String get() = thread.title
    val pinned: Boolean get() = thread.pinned
    val lastActivityAt: Long get() = thread.lastActivityAt
    val queued: Int get() = thread.queuedInputs
}

/**
 * The project and thread lists of the プロジェクト tab. Statuses come from [ThreadActivity]
 * (explicit protocol state), unread from the engine's per-device read positions
 * (`Thread.head` against the last viewed head).
 */
object ProjectLists {
    /** Non-archived projects matching [query] (name or path, case-insensitive), ordered by [sort]. */
    fun projects(workspace: WorkspaceState, sort: ProjectSort = ProjectSort.Recent, query: String = ""): List<ProjectRow> {
        val q = query.trim().lowercase()
        val rows = workspace.projects
            .filter { !it.archived }
            .filter { q.isEmpty() || it.name.lowercase().contains(q) || it.path.lowercase().contains(q) }
            .map { project -> row(workspace, project) }
        return when (sort) {
            ProjectSort.Recent -> rows.sortedWith(
                compareByDescending<ProjectRow> { it.lastActivityAt }.thenBy { it.project.name.lowercase() }.thenBy { it.project.id },
            )
            ProjectSort.Name -> rows.sortedWith(compareBy<ProjectRow> { it.project.name.lowercase() }.thenBy { it.project.id })
        }
    }

    private fun row(workspace: WorkspaceState, project: Project): ProjectRow {
        val threads = workspace.threads.filter { it.thread.projectId == project.id && !it.thread.archived }
        val activities = threads.map { ThreadActivity.of(it.thread, workspace.pendingInteractions) }
        val ids = threads.map { it.thread.id }.toSet()
        val pending = workspace.pendingInteractions.filter { it.threadId in ids && it.status == InteractionStatus.Pending }
        return ProjectRow(
            project = project,
            threads = threads.size,
            activity = activities.minByOrNull { it.ordinal } ?: ThreadActivity.Idle,
            approvals = pending.count { it.request !is InteractionRequest.Question },
            questions = pending.count { it.request is InteractionRequest.Question },
            running = activities.count { it.working },
            backgroundRunning = threads.filter { ThreadActivity.of(it.thread, workspace.pendingInteractions) == ThreadActivity.Background }
                .sumOf { it.thread.background.running },
            errors = activities.count { it == ThreadActivity.Error },
            unread = threads.count { it.unread },
            queued = threads.sumOf { it.thread.queuedInputs },
            lastActivityAt = maxOf(project.updatedAt, threads.maxOfOrNull { it.thread.lastActivityAt } ?: 0L),
        )
    }

    /** Non-archived threads of [projectId]: pinned first, then by last activity (UX §8.2). */
    fun threads(workspace: WorkspaceState, projectId: String): List<ThreadRow> {
        val harnesses = workspace.harnesses.associateBy { it.id }
        return workspace.threads
            .filter { it.thread.projectId == projectId && !it.thread.archived }
            .map { entry -> threadRow(entry.thread, entry.unread, workspace.pendingInteractions, harnesses) }
            .sortedWith(compareByDescending<ThreadRow> { it.pinned }.thenByDescending { it.lastActivityAt }.thenByDescending { it.id })
    }

    fun threadRow(thread: Thread, unread: Boolean, pendingInteractions: List<Interaction>, harnesses: Map<String, Harness>): ThreadRow {
        val pending = pendingInteractions.filter { it.threadId == thread.id && it.status == InteractionStatus.Pending }
        return ThreadRow(
            thread = thread,
            activity = ThreadActivity.of(thread, pendingInteractions),
            unread = unread,
            harnessName = harnesses[thread.harnessId]?.displayName,
            approvals = pending.count { it.request !is InteractionRequest.Question },
            questions = pending.count { it.request is InteractionRequest.Question },
        )
    }
}
