package dev.aas.android.ui

import android.net.Uri
import androidx.test.ext.junit.runners.AndroidJUnit4
import dev.aas.android.protocol.ApprovalOption
import dev.aas.android.protocol.ApprovalOptionKind
import dev.aas.android.protocol.InteractionResolution
import dev.aas.android.protocol.Question
import dev.aas.android.protocol.QuestionAnswer
import dev.aas.android.protocol.QuestionChoice
import dev.aas.android.domain.InteractionTexts
import dev.aas.android.ui.interaction.ApprovalChoices
import dev.aas.android.ui.interaction.DraftAnswer
import dev.aas.android.ui.interaction.QuestionAnswers
import dev.aas.android.ui.navigation.DeepLinks
import dev.aas.android.ui.navigation.IntentTarget
import dev.aas.android.domain.ProjectLists
import dev.aas.android.sync.Samples
import dev.aas.android.sync.ThreadEntry
import dev.aas.android.sync.WorkspaceState
import org.junit.Test
import org.junit.runner.RunWith
import kotlin.test.assertEquals
import kotlin.test.assertFalse
import kotlin.test.assertNull
import kotlin.test.assertTrue

class QuestionAnswersTest {
    private val single = Question("q1", prompt = "Which DB?", choices = listOf(QuestionChoice("pg", "Postgres"), QuestionChoice("my", "MySQL")))
    private val multi = single.copy(id = "q2", multiSelect = true)
    private val withOther = single.copy(id = "q3", allowFreeText = true)
    private val freeOnly = Question("q4", prompt = "Name?", allowFreeText = true)
    private val info = Question("q5", prompt = "FYI")

    @Test
    fun completenessFollowsTheQuestionsShape() {
        assertFalse(QuestionAnswers.isComplete(single, DraftAnswer()))
        assertTrue(QuestionAnswers.isComplete(single, DraftAnswer(setOf("pg"))))
        assertTrue(QuestionAnswers.isComplete(withOther, DraftAnswer(text = "SQLite")))
        assertFalse(QuestionAnswers.isComplete(withOther, DraftAnswer(text = "  ")))
        assertFalse(QuestionAnswers.isComplete(freeOnly, DraftAnswer()))
        assertTrue(QuestionAnswers.isComplete(freeOnly, DraftAnswer(text = "x")))
        assertTrue(QuestionAnswers.isComplete(info, DraftAnswer()))
    }

    @Test
    fun togglingRespectsSingleAndMultiSelect() {
        assertEquals(setOf("my"), DraftAnswer(setOf("pg")).toggle("my", multiSelect = false).choiceIds)
        assertEquals(setOf("pg", "my"), DraftAnswer(setOf("pg")).toggle("my", multiSelect = true).choiceIds)
        assertEquals(setOf("pg"), DraftAnswer(setOf("pg", "my")).toggle("my", multiSelect = true).choiceIds)
    }

    @Test
    fun theResolutionKeepsServerOrderAndDropsWhatIsNotAllowed() {
        val resolution = QuestionAnswers.resolution(
            listOf(single, multi, withOther, freeOnly),
            mapOf(
                "q1" to DraftAnswer(setOf("my", "pg"), text = "ignored: no free text"),
                "q2" to DraftAnswer(setOf("my", "pg", "gone")),
                "q3" to DraftAnswer(text = " SQLite "),
                "q4" to DraftAnswer(text = "svc"),
            ),
        )
        assertEquals(
            InteractionResolution.Question(
                listOf(
                    QuestionAnswer("q1", listOf("pg")),
                    QuestionAnswer("q2", listOf("pg", "my")),
                    QuestionAnswer("q3", emptyList(), "SQLite"),
                    QuestionAnswer("q4", emptyList(), "svc"),
                ),
            ),
            resolution,
        )
    }
}

class ApprovalChoicesTest {
    private fun o(id: String, kind: ApprovalOptionKind) = ApprovalOption(id, id, kind)

    @Test
    fun primaryButtonsAndNotificationActions() {
        val codex = listOf(o("once", ApprovalOptionKind.AllowOnce), o("session", ApprovalOptionKind.AllowForSession), o("no", ApprovalOptionKind.Deny), o("stop", ApprovalOptionKind.Abort))
        assertEquals("once", ApprovalChoices.primaryAllow(codex)?.id)
        assertEquals("no", ApprovalChoices.primaryDeny(codex)?.id)
        assertEquals("once", InteractionTexts.notificationAllow(codex)?.id)
        assertEquals("no", InteractionTexts.notificationDeny(codex)?.id)

        // Only broad allows: the app still has a primary button, the notification has none.
        val broad = listOf(o("always", ApprovalOptionKind.AllowAlways), o("fb", ApprovalOptionKind.DenyWithFeedback), o("abort", ApprovalOptionKind.Abort))
        assertEquals("always", ApprovalChoices.primaryAllow(broad)?.id)
        assertEquals("abort", ApprovalChoices.primaryDeny(broad)?.id)
        assertNull(InteractionTexts.notificationAllow(broad))
        assertNull(InteractionTexts.notificationDeny(broad))
    }
}

class ProjectListsTest {
    @Test
    fun pinnedThreadsComeFirstThenByActivity() {
        val workspace = WorkspaceState(
            synced = true,
            harnesses = emptyList(),
            projects = listOf(Samples.project("prj_1"), Samples.project("prj_2"), Samples.project("prj_old").copy(archived = true)),
            threads = listOf(
                ThreadEntry(Samples.thread("a", lastActivityAt = 30), unread = false),
                ThreadEntry(Samples.thread("b", lastActivityAt = 20).copy(pinned = true), unread = true),
                ThreadEntry(Samples.thread("c", lastActivityAt = 40).copy(archived = true), unread = true),
                ThreadEntry(Samples.thread("d", lastActivityAt = 10), unread = false),
                ThreadEntry(Samples.thread("e", lastActivityAt = 50, projectId = "prj_2"), unread = false),
            ),
            pendingInteractions = listOf(Samples.approval("i", threadId = "d")),
            operations = emptyList(),
        )
        assertEquals(listOf("b", "a", "d"), ProjectLists.threads(workspace, "prj_1").map { it.id })
        val projects = ProjectLists.projects(workspace, dev.aas.android.domain.ProjectSort.Name)
        assertEquals(listOf("prj_1", "prj_2"), projects.map { it.project.id })
        val first = projects.first()
        assertEquals(3, first.threads)
        assertEquals(1, first.approvals)
        assertEquals(1, first.unread)
        assertEquals(30L, first.lastActivityAt)
    }
}

@RunWith(AndroidJUnit4::class)
class IntentTargetTest {
    @Test
    fun threadLinksRoundTrip() {
        assertEquals(IntentTarget.Thread("thr_01J", null), IntentTarget.parse(DeepLinks.thread("thr_01J")))
        assertEquals(IntentTarget.Thread("thr_01J", "int_9"), IntentTarget.parse(DeepLinks.thread("thr_01J", "int_9")))
        assertEquals("aas://thread/thr_01J?interactionId=int_9", DeepLinks.thread("thr_01J", "int_9").toString())
    }

    @Test
    fun pairingLinksAndForeignUrisAreRecognised() {
        val pair = "aas://pair?u=wss%3A%2F%2Fpc.ts.net%2Fv1%2Fws&c=ABCD-1234&n=pc"
        assertEquals(IntentTarget.Pair(pair), IntentTarget.parse(Uri.parse(pair)))
        assertNull(IntentTarget.parse(Uri.parse("https://example.com/thread/x")))
        assertNull(IntentTarget.parse(Uri.parse("aas://thread/")))
        assertNull(IntentTarget.parse(Uri.parse("aas://other/x")))
        assertNull(IntentTarget.parse(null))
    }
}
