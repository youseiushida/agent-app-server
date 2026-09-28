package dev.aas.android.domain

import dev.aas.android.domain.timeline.PendingInput
import dev.aas.android.domain.timeline.Timeline
import dev.aas.android.domain.timeline.TimelineRow
import dev.aas.android.protocol.AasJson
import dev.aas.android.protocol.Delivery
import dev.aas.android.protocol.FsRoot
import dev.aas.android.protocol.InputPart
import dev.aas.android.protocol.Item
import dev.aas.android.protocol.ItemStatus
import dev.aas.android.protocol.Methods
import dev.aas.android.protocol.TurnStartParams
import dev.aas.android.protocol.TurnStatus
import dev.aas.android.sync.OutboxEntry
import dev.aas.android.sync.Samples
import dev.aas.android.sync.ThreadEntry
import dev.aas.android.sync.ThreadState
import dev.aas.android.sync.ThreadSync
import dev.aas.android.sync.WorkspaceState
import dev.aas.android.testing.Fixtures
import dev.aas.android.testing.TestEngine
import kotlinx.serialization.json.JsonObject
import kotlinx.serialization.json.JsonPrimitive
import kotlinx.serialization.json.jsonObject
import org.junit.Test
import kotlin.test.assertEquals
import kotlin.test.assertIs
import kotlin.test.assertNull
import kotlin.test.assertTrue

class TimelineTest {
    private val read = Fixtures.threadRead

    private fun state(turns: List<dev.aas.android.protocol.Turn> = read.turns, items: List<Item> = read.items, hasMore: Boolean = false) =
        ThreadState(read.thread.id, ThreadSync.Live, read.thread, turns, items, read.interactions, read.queued, hasMore, 0, emptyList())

    private fun kinds(rows: List<TimelineRow>) = rows.map { it::class.simpleName }

    @Test
    fun aFinishedTurnFoldsItsActivityAndEndsWithItsSummary() {
        val rows = Timeline.build(state(), emptyList(), emptyMap())
        assertEquals(
            listOf("TurnStart", "ItemRow", "ActivityGroup", "ItemRow", "ItemRow", "ItemRow", "InteractionRow", "InteractionRow", "TurnEnd"),
            kinds(rows),
        )
        val group = assertIs<TimelineRow.ActivityGroup>(rows[2])
        assertEquals(listOf("reasoning", "commandExecution", "fileChange", "toolCall"), group.items.map { AasJson.encodeToJsonElement(Item.serializer(), it).jsonObject["kind"]!!.let { k -> (k as JsonPrimitive).content } })
        assertEquals(false, group.expanded)
        assertEquals(read.items.first { it is Item.Reasoning }.id, group.groupKey)
        // Keys are stable across updates (the list keeps its position by them).
        assertEquals(rows.map { it.key }, Timeline.build(state(), emptyList(), emptyMap()).map { it.key })
        assertEquals(rows.size, rows.map { it.key }.toSet().size)
    }

    @Test
    fun theLiveGroupOfARunningTurnIsOpenUnlessTheUserClosedIt() {
        val running = read.turns.map { it.copy(status = TurnStatus.Running, completedAt = null) }
        val rows = Timeline.build(state(turns = running), emptyList(), emptyMap())
        val group = rows.filterIsInstance<TimelineRow.ActivityGroup>().single()
        assertTrue(group.expanded)
        assertEquals(4, rows.count { it is TimelineRow.GroupedItem })
        val working = assertIs<TimelineRow.Working>(rows.last())
        // What it is doing: the latest item in progress (the answer being written).
        assertIs<Item.AgentMessage>(working.current)

        val closed = Timeline.build(state(turns = running), emptyList(), mapOf(group.groupKey to false))
        assertEquals(false, closed.filterIsInstance<TimelineRow.ActivityGroup>().single().expanded)
        assertEquals(0, closed.count { it is TimelineRow.GroupedItem })
    }

    @Test
    fun interactionsGoWhereTheyHappenedAndSplitGroups() {
        val approval = read.interactions.first().copy(createdAt = 1790000002003)
        val st = state().copy(interactions = listOf(approval))
        val rows = Timeline.build(st, emptyList(), emptyMap())
        val index = rows.indexOfFirst { it is TimelineRow.InteractionRow }
        // Reasoning and the command before it form one group, the file change and tool call another.
        val before = assertIs<TimelineRow.ActivityGroup>(rows[index - 1])
        val after = assertIs<TimelineRow.ActivityGroup>(rows[index + 1])
        assertEquals(2, before.items.size)
        assertEquals(2, after.items.size)
    }

    @Test
    fun olderPagesOrphansAndOutboxMessages() {
        val orphan = Samples.agentMessage("itm_orphan", "late", threadId = read.thread.id, turnId = "trn_unknown", status = ItemStatus.Completed)
        val entry = OutboxEntry(
            clientRequestId = "crid-1",
            method = Methods.TurnStart.name,
            params = AasJson.encodeToJsonElement(
                TurnStartParams.serializer(),
                TurnStartParams("crid-1", read.thread.id, listOf(InputPart.Text("hi"), InputPart.Image("blb_1"), InputPart.Mention("a.rs")), Delivery.Queue),
            ) as JsonObject,
            createdAtMs = 5,
            failures = 2,
            lastError = "draining",
        )
        val other = OutboxEntry("crid-2", Methods.ThreadUpdate.name, JsonObject(mapOf("clientRequestId" to JsonPrimitive("crid-2"))), 6)
        val broken = OutboxEntry("crid-3", Methods.TurnStart.name, JsonObject(mapOf("clientRequestId" to JsonPrimitive("crid-3"))), 7)
        val pending = PendingInput.of(listOf(entry, other, broken))
        assertEquals(2, pending.size)
        // The text as the daemon will write it: the mention as its "@path" token in its place.
        assertEquals(PendingInput("crid-1", "hi @a.rs", 1, listOf("a.rs"), Delivery.Queue, 5, 2, "draining"), pending[0])
        // Params this client cannot read are still shown (as a generic waiting message).
        assertNull(pending[1].text)

        val rows = Timeline.build(state(items = read.items + orphan, hasMore = true), pending, emptyMap())
        assertEquals(TimelineRow.LoadOlder, rows.first())
        assertEquals("item-itm_orphan", rows[rows.size - 3].key)
        assertEquals(listOf("pending-crid-1", "pending-crid-3"), rows.takeLast(2).map { it.key })
    }
}

class ProjectListSortingTest {
    private val workspace = WorkspaceState(
        synced = true,
        harnesses = listOf(TestEngine.fakeHarness()),
        projects = listOf(
            Samples.project("prj_a", name = "alpha").copy(updatedAt = 5),
            Samples.project("prj_b", name = "Beta").copy(updatedAt = 50),
            Samples.project("prj_c", name = "gamma").copy(updatedAt = 1),
        ),
        threads = listOf(
            ThreadEntry(Samples.thread("t1", lastActivityAt = 100, projectId = "prj_c").copy(queuedInputs = 2), unread = true),
            ThreadEntry(Samples.thread("t2", lastActivityAt = 10, projectId = "prj_c").copy(lastTurn = Samples.turnSummary("r", 0, TurnStatus.Failed)), unread = false),
            ThreadEntry(Samples.thread("t3", lastActivityAt = 20, projectId = "prj_a").copy(pinned = true), unread = false),
            ThreadEntry(Samples.thread("t4", lastActivityAt = 30, projectId = "prj_a"), unread = true),
        ),
        pendingInteractions = listOf(Samples.approval("i1", threadId = "t2")),
        operations = emptyList(),
    )

    @Test
    fun projectsByRecentActivityOrByNameWithTheirCounts() {
        assertEquals(listOf("prj_c", "prj_b", "prj_a"), ProjectLists.projects(workspace, ProjectSort.Recent).map { it.project.id })
        assertEquals(listOf("prj_a", "prj_b", "prj_c"), ProjectLists.projects(workspace, ProjectSort.Name).map { it.project.id })
        val c = ProjectLists.projects(workspace).first()
        assertEquals(2, c.threads)
        assertEquals(1, c.approvals)
        assertEquals(0, c.questions)
        assertEquals(1, c.unread)
        assertEquals(2, c.queued)
        assertEquals(ThreadActivity.NeedsApproval, c.activity)
        assertEquals(100L, c.lastActivityAt)
        // A project without threads is dated by the project itself.
        assertEquals(50L, ProjectLists.projects(workspace).first { it.project.id == "prj_b" }.lastActivityAt)
    }

    @Test
    fun searchMatchesNameOrPathCaseInsensitively() {
        assertEquals(listOf("prj_b"), ProjectLists.projects(workspace, query = "BET").map { it.project.id })
        assertEquals(listOf("prj_a"), ProjectLists.projects(workspace, query = "p\\prj_a").map { it.project.id })
        assertTrue(ProjectLists.projects(workspace, query = "zzz").isEmpty())
    }

    @Test
    fun threadRowsPinnedFirstWithUnreadAndCounts() {
        val rows = ProjectLists.threads(workspace, "prj_a")
        assertEquals(listOf("t3", "t4"), rows.map { it.id })
        assertEquals(listOf(false, true), rows.map { it.unread })
        assertEquals("Fake", rows.first().harnessName)
        val c = ProjectLists.threads(workspace, "prj_c")
        assertEquals(1, c.first { it.id == "t2" }.approvals)
        assertEquals(2, c.first { it.id == "t1" }.queued)
    }
}

class NewProjectLogicTest {
    @Test
    fun folderNamesFollowTheDaemonsRules() {
        assertNull(ProjectNames.validate("new-app"))
        assertEquals(NameProblem.Empty, ProjectNames.validate("  "))
        assertEquals(NameProblem.InvalidCharacters, ProjectNames.validate("a/b"))
        assertEquals(NameProblem.InvalidCharacters, ProjectNames.validate("a:b"))
        assertEquals(NameProblem.InvalidCharacters, ProjectNames.validate("what?"))
        assertEquals(NameProblem.NotAllowed, ProjectNames.validate(".."))
        assertEquals(NameProblem.NotAllowed, ProjectNames.validate("name."))
        assertEquals(NameProblem.NotAllowed, ProjectNames.validate("con"))
        assertEquals(NameProblem.NotAllowed, ProjectNames.validate("LPT1.txt"))
        assertNull(ProjectNames.validate("console"))
    }

    @Test
    fun cloneUrlsNameTheFolderLikeGitDoes() {
        assertEquals("new-app", ProjectNames.fromCloneUrl("https://github.com/example/new-app.git"))
        assertEquals("repo", ProjectNames.fromCloneUrl("git@github.com:owner/repo.git"))
        assertEquals("tool", ProjectNames.fromCloneUrl("https://host/group/tool/"))
        assertEquals("x", ProjectNames.fromCloneUrl("ssh://git@host:2222/x.git"))
        assertEquals("host", ProjectNames.fromCloneUrl("https://host/"))
        assertNull(ProjectNames.fromCloneUrl("https://"))
        assertTrue(ProjectNames.looksLikeCloneUrl("https://github.com/a/b"))
        assertTrue(ProjectNames.looksLikeCloneUrl("git@github.com:a/b.git"))
        assertTrue(ProjectNames.looksLikeCloneUrl("file:///C:/repos/x"))
        assertEquals(false, ProjectNames.looksLikeCloneUrl("not a url"))
        assertEquals(false, ProjectNames.looksLikeCloneUrl("github.com/a/b"))
    }

    @Test
    fun serverPathsWalkWithinTheRoots() {
        val roots = listOf(FsRoot("C:\\Users\\me\\Documents", "Documents"), FsRoot("D:\\", "D"))
        assertEquals("C:\\Users\\me\\Documents\\new", ServerPaths.child("C:\\Users\\me\\Documents", "new"))
        assertEquals("D:\\new", ServerPaths.child("D:\\", "new"))
        assertEquals("C:\\Users\\me\\Documents", ServerPaths.parent("C:\\Users\\me\\Documents\\app", roots))
        // At a root (in any letter case), up means the list of roots.
        assertNull(ServerPaths.parent("c:\\users\\ME\\documents", roots))
        assertEquals("D:\\", ServerPaths.parent("D:\\work", roots))
        assertNull(ServerPaths.parent("C:\\Users\\me", roots))
        assertTrue(ServerPaths.isWithin("C:\\USERS\\me\\Documents\\x", "C:\\Users\\me\\Documents"))
        assertEquals(false, ServerPaths.isWithin("C:\\Users\\me\\DocumentsOld", "C:\\Users\\me\\Documents"))
        assertEquals("Documents", ServerPaths.rootOf("C:\\Users\\me\\Documents\\a\\b", roots)?.name)
        assertEquals("app", ServerPaths.name("C:\\Users\\me\\Documents\\app\\"))
        assertEquals("b", ServerPaths.child("/srv/a", "b").substringAfterLast('/'))
    }
}
