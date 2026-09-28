package dev.aas.android.domain

import dev.aas.android.R
import dev.aas.android.data.UploadPlan
import dev.aas.android.protocol.Interaction
import dev.aas.android.protocol.InteractionRequest
import dev.aas.android.protocol.InteractionStatus
import dev.aas.android.protocol.Question
import dev.aas.android.protocol.RpcError
import dev.aas.android.protocol.ThreadBackground
import dev.aas.android.protocol.ThreadError
import dev.aas.android.protocol.ThreadStatus
import dev.aas.android.protocol.TurnStatus
import dev.aas.android.sync.OutboxEntry
import dev.aas.android.sync.OutboxResult
import dev.aas.android.sync.Samples
import dev.aas.android.sync.ThreadEntry
import dev.aas.android.sync.WorkspaceState
import dev.aas.android.ui.common.UiText
import kotlinx.serialization.json.JsonObject
import kotlinx.serialization.json.JsonPrimitive
import org.junit.Test
import kotlin.test.assertEquals
import kotlin.test.assertNull
import kotlin.test.assertTrue

class ThreadActivityTest {
    private fun question(id: String, threadId: String) = Interaction(
        id = id, threadId = threadId, status = InteractionStatus.Pending, createdAt = 1,
        request = InteractionRequest.Question("Which?", listOf(Question("q1", prompt = "Pick"))),
    )

    @Test
    fun approvalsOutrankQuestionsOutrankRunningOutrankErrors() {
        val thread = Samples.thread("thr_1").copy(status = ThreadStatus.Running)
        assertEquals(ThreadActivity.NeedsApproval, ThreadActivity.of(thread, listOf(question("i1", "thr_1"), Samples.approval("i2"))))
        assertEquals(ThreadActivity.NeedsInput, ThreadActivity.of(thread, listOf(question("i1", "thr_1"))))
        assertEquals(ThreadActivity.Running, ThreadActivity.of(thread, emptyList()))
        // Interactions of other threads and resolved ones do not count.
        assertEquals(ThreadActivity.Running, ThreadActivity.of(thread, listOf(Samples.approval("i3", threadId = "thr_2"), Samples.approval("i4", status = InteractionStatus.Resolved))))
    }

    @Test
    fun errorsComeFromTheLastTurnOrALaterError() {
        val failed = Samples.thread("thr_1", lastTurn = Samples.turnSummary("t", 0, TurnStatus.Failed))
        assertEquals(ThreadActivity.Error, ThreadActivity.of(failed, emptyList()))
        val ok = Samples.thread("thr_1", lastTurn = Samples.turnSummary("t", 0, TurnStatus.Completed))
        assertEquals(ThreadActivity.Idle, ThreadActivity.of(ok, emptyList()))
        // A spawn failure recorded after the last (completed) turn started is an error…
        assertEquals(ThreadActivity.Error, ThreadActivity.of(ok.copy(lastError = ThreadError("spawn failed", "spawnFailed", at = 5)), emptyList()))
        // …an error older than the last turn is history.
        val later = Samples.thread("thr_1", lastTurn = Samples.turnSummary("t", 1, TurnStatus.Completed).copy(startedAt = 10))
        assertEquals(ThreadActivity.Idle, ThreadActivity.of(later.copy(lastError = ThreadError("old", "agentExited", at = 5)), emptyList()))
        for (status in listOf(ThreadStatus.Queued, ThreadStatus.Starting, ThreadStatus.Stopping)) {
            assertEquals(ThreadActivity.Running, ThreadActivity.of(Samples.thread("thr_1").copy(status = status), emptyList()))
        }
    }

    /**
     * A ready thread whose harness reports background work running: バックグラウンドで実行中. A turn,
     * an error or a pending request says more and wins; an ambient-only thread (running 0) is idle.
     */
    @Test
    fun backgroundWorkIsItsOwnActivityBelowTurnsErrorsAndRequests() {
        val ready = Samples.thread("thr_1", lastTurn = Samples.turnSummary("t", 0, TurnStatus.Completed), background = ThreadBackground(running = 2))
            .copy(status = ThreadStatus.Ready)
        assertEquals(ThreadActivity.Background, ThreadActivity.of(ready, emptyList()))
        assertTrue(ThreadActivity.Background.working)
        assertTrue(!ThreadActivity.Background.needsAction)
        assertEquals(ThreadActivity.Running, ThreadActivity.of(ready.copy(status = ThreadStatus.Running), emptyList()))
        assertEquals(ThreadActivity.Error, ThreadActivity.of(ready.copy(lastTurn = Samples.turnSummary("t", 0, TurnStatus.Failed)), emptyList()))
        assertEquals(ThreadActivity.NeedsApproval, ThreadActivity.of(ready, listOf(Samples.approval("int_1", threadId = "thr_1"))))
        assertEquals(ThreadActivity.Idle, ThreadActivity.of(ready.copy(background = ThreadBackground(running = 0)), emptyList()))
    }
}

class InboxModelTest {
    private fun respond(crid: String, interactionId: String) = OutboxEntry(
        crid, "interaction/respond",
        JsonObject(mapOf("clientRequestId" to JsonPrimitive(crid), "interactionId" to JsonPrimitive(interactionId))), 1,
    )

    @Test
    fun sortsThreadsIntoSectionsAndMarksAnswersInTheOutbox() {
        val waiting = Samples.thread("thr_wait", lastActivityAt = 50)
        val failedUnread = Samples.thread("thr_err", lastTurn = Samples.turnSummary("t1", 0, TurnStatus.Failed), lastActivityAt = 40)
        val failedRead = Samples.thread("thr_err_read", lastTurn = Samples.turnSummary("t2", 0, TurnStatus.Failed), lastActivityAt = 35)
        val running = Samples.thread("thr_run", lastActivityAt = 30).copy(status = ThreadStatus.Running)
        val unread = Samples.thread("thr_new", lastActivityAt = 20)
        val quiet = Samples.thread("thr_quiet", lastActivityAt = 10)
        val background = Samples.thread("thr_bg", lastActivityAt = 25, background = ThreadBackground(running = 1)).copy(status = ThreadStatus.Ready)
        val archived = Samples.thread("thr_arch", lastActivityAt = 60).copy(archived = true)
        val workspace = WorkspaceState(
            synced = true,
            harnesses = emptyList(),
            projects = listOf(Samples.project("prj_1", name = "app")),
            threads = listOf(
                ThreadEntry(archived, unread = true),
                ThreadEntry(waiting, unread = true),
                ThreadEntry(failedUnread, unread = true),
                ThreadEntry(failedRead, unread = false),
                ThreadEntry(running, unread = false),
                ThreadEntry(background, unread = false),
                ThreadEntry(unread, unread = true),
                ThreadEntry(quiet, unread = false),
            ),
            pendingInteractions = listOf(Samples.approval("int_1", threadId = "thr_wait"), Samples.approval("int_2", threadId = "thr_gone")),
            operations = emptyList(),
        )
        val inbox = InboxModel.build(workspace, listOf(respond("c1", "int_2")))
        assertEquals(listOf("int_1" to false, "int_2" to true), inbox.interactions.map { it.interaction.id to it.responsePending })
        assertEquals("app", inbox.interactions[0].project?.name)
        assertNull(inbox.interactions[1].thread)
        assertEquals(listOf("thr_err"), inbox.errors.map { it.thread.id })
        // Background work is listed with the running threads (the agent works; nothing needs the user).
        assertEquals(listOf("thr_run", "thr_bg"), inbox.running.map { it.thread.id })
        assertEquals(listOf("thr_new"), inbox.unread.map { it.thread.id })
        assertEquals(3, inbox.badgeCount)
    }
}

class ResultMessagesTest {
    private val entry = OutboxEntry("c1", "turn/start", JsonObject(mapOf("clientRequestId" to JsonPrimitive("c1"))), 1)
    private val respond = entry.copy(method = "interaction/respond")

    private fun answer(alreadyResolved: Boolean, resolvedBy: String?) = JsonObject(
        mapOf(
            "alreadyResolved" to JsonPrimitive(alreadyResolved),
            "interaction" to JsonObject(listOfNotNull(resolvedBy?.let { "resolvedBy" to JsonPrimitive(it) }).toMap()),
        ),
    )

    @Test
    fun definitiveErrorsAreShownWithWhatFailed() {
        val text = ResultMessages.describe(OutboxResult.Failed(entry, RpcError(-32003, "thread is archived")), "dev_1")
        assertEquals(UiText.of(R.string.request_failed, UiText.of(R.string.request_turn_start), "thread is archived"), text)
    }

    @Test
    fun anAnswerAppliedElsewhereIsReportedButNotOurOwnRepeat() {
        assertEquals(UiText.of(R.string.answer_already_resolved), ResultMessages.describe(OutboxResult.Succeeded(respond, answer(true, "dev_other")), "dev_1"))
        assertEquals(UiText.of(R.string.answer_already_resolved), ResultMessages.describe(OutboxResult.Succeeded(respond, answer(true, null)), "dev_1"))
        assertNull(ResultMessages.describe(OutboxResult.Succeeded(respond, answer(true, "dev_1")), "dev_1"))
        assertNull(ResultMessages.describe(OutboxResult.Succeeded(respond, answer(false, "dev_1")), "dev_1"))
        assertNull(ResultMessages.describe(OutboxResult.Succeeded(entry, JsonObject(emptyMap())), "dev_1"))
        assertNull(ResultMessages.describe(OutboxResult.Discarded(entry), "dev_1"))
    }

    @Test
    fun everyMutatingMethodHasALabel() {
        for (method in dev.aas.android.protocol.Methods.all.filter { it.mutating }) {
            assert(RequestLabels.of(method.name) != R.string.request_other) { "no label for ${method.name}" }
        }
        assertEquals(R.string.request_other, RequestLabels.of("future/method"))
    }
}

class UploadPlanTest {
    @Test
    fun screenshotsPassThroughPhotosAreReencoded() {
        val limit = 25L * 1024 * 1024
        assertEquals(UploadPlan.SendAsIs("image/png"), UploadPlan.decide("image/png", 3_000_000, limit))
        assertEquals(UploadPlan.SendAsIs("image/webp"), UploadPlan.decide("image/webp", limit, limit))
        assertEquals(UploadPlan.SendAsIs("image/gif"), UploadPlan.decide("image/gif", 10, limit))
        // JPEG always: EXIF orientation applied, metadata (GPS) dropped.
        assertEquals(UploadPlan.Reencode, UploadPlan.decide("image/jpeg", 10, limit))
        assertEquals(UploadPlan.Reencode, UploadPlan.decide("image/heic", 10, limit))
        assertEquals(UploadPlan.Reencode, UploadPlan.decide(null, 10, limit))
        assertEquals(UploadPlan.Reencode, UploadPlan.decide("image/png", limit + 1, limit))
    }
}
