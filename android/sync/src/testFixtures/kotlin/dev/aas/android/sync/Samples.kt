package dev.aas.android.sync

import dev.aas.android.protocol.ApprovalOption
import dev.aas.android.protocol.ApprovalOptionKind
import dev.aas.android.protocol.Harness
import dev.aas.android.protocol.HarnessCapabilities
import dev.aas.android.protocol.HarnessKind
import dev.aas.android.protocol.Interaction
import dev.aas.android.protocol.InteractionRequest
import dev.aas.android.protocol.InteractionStatus
import dev.aas.android.protocol.Item
import dev.aas.android.protocol.ItemStatus
import dev.aas.android.protocol.Operation
import dev.aas.android.protocol.OperationKind
import dev.aas.android.protocol.OperationStatus
import dev.aas.android.protocol.Project
import dev.aas.android.protocol.QueuedInput
import dev.aas.android.protocol.Subject
import dev.aas.android.protocol.Thread
import dev.aas.android.protocol.ThreadStatus
import dev.aas.android.protocol.Turn
import dev.aas.android.protocol.TurnStatus
import dev.aas.android.protocol.TurnSummary
import dev.aas.android.protocol.Workspace
import kotlinx.coroutines.delay

/** Small builders of protocol objects for tests (of this module and of the app). */
object Samples {
    /** A harness; an unavailable one carries the probe's [reason]. */
    fun harness(id: String = "fake", available: Boolean = true, reason: String? = null, capabilities: HarnessCapabilities = HarnessCapabilities()) =
        Harness(id = id, kind = HarnessKind.Fake, displayName = id, available = available, unavailableReason = reason, capabilities = capabilities)

    fun project(id: String, name: String = id) = Project(id = id, name = name, path = "C:\\p\\$id", createdAt = 1, updatedAt = 1)

    fun thread(
        id: String,
        head: Long = 0,
        title: String = "t",
        lastTurn: TurnSummary? = null,
        lastActivityAt: Long = 1,
        projectId: String = "prj_1",
        harnessId: String = "fake",
    ) = Thread(
        id = id, projectId = projectId, harnessId = harnessId, title = title, cwd = "C:\\p", workspace = Workspace.Local,
        status = ThreadStatus.Idle, lastTurn = lastTurn, createdAt = 1, updatedAt = 1, lastActivityAt = lastActivityAt, head = head,
    )

    fun turnSummary(id: String, index: Int, status: TurnStatus) = TurnSummary(id, index, status, startedAt = 1, completedAt = if (status.isTerminal) 2 else null)

    fun turn(id: String, threadId: String = "thr_1", index: Int = 0, status: TurnStatus = TurnStatus.Running) =
        Turn(id, threadId, index, status, startedAt = 1, completedAt = if (status.isTerminal) 2 else null)

    fun agentMessage(id: String, text: String, threadId: String = "thr_1", turnId: String = "trn_1", status: ItemStatus = ItemStatus.InProgress) =
        Item.AgentMessage(id, threadId, turnId, status, 1, null, text)

    fun approval(id: String, threadId: String = "thr_1", status: InteractionStatus = InteractionStatus.Pending, createdAt: Long = 1) =
        Interaction(
            id = id, threadId = threadId, turnId = "trn_1", status = status, createdAt = createdAt,
            request = InteractionRequest.Approval(
                title = "Run command?",
                subject = Subject.Command("ls"),
                options = listOf(
                    ApprovalOption("allow", "Allow", ApprovalOptionKind.AllowOnce),
                    ApprovalOption("deny", "Deny", ApprovalOptionKind.Deny),
                ),
            ),
        )

    fun operation(id: String, status: OperationStatus, startedAt: Long = 1) =
        Operation(id = id, kind = OperationKind.GitClone, status = status, startedAt = startedAt, finishedAt = if (status.isTerminal) startedAt + 1 else null)

    fun queued(id: String, threadId: String = "thr_1") = QueuedInput(id, threadId, 1, "preview", emptyList())
}

/** Polls [check] until it returns non-null; fails after [timeoutMs] with [what]. */
suspend fun <T : Any> eventually(timeoutMs: Long = 10_000, what: String = "condition", check: suspend () -> T?): T {
    val deadline = System.currentTimeMillis() + timeoutMs
    while (true) {
        check()?.let { return it }
        if (System.currentTimeMillis() > deadline) throw AssertionError("timed out after $timeoutMs ms waiting for $what")
        delay(POLL_INTERVAL_MS)
    }
}

/** Polling interval of [eventually]: short enough not to slow tests down, long enough not to spin. */
private const val POLL_INTERVAL_MS = 10L
