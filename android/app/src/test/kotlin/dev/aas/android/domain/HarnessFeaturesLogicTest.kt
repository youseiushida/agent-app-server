package dev.aas.android.domain

import dev.aas.android.R
import dev.aas.android.domain.composer.LocalCommand
import dev.aas.android.domain.composer.Palette
import dev.aas.android.domain.composer.PaletteAction
import dev.aas.android.domain.composer.PaletteContext
import dev.aas.android.domain.composer.PaletteSource
import dev.aas.android.domain.composer.TypedCommands
import dev.aas.android.protocol.AasJson
import dev.aas.android.protocol.Attachment
import dev.aas.android.protocol.Command
import dev.aas.android.protocol.CommandAction
import dev.aas.android.protocol.CommandListResult
import dev.aas.android.protocol.CommandSource
import dev.aas.android.protocol.HarnessFeatures
import dev.aas.android.protocol.Item
import dev.aas.android.protocol.ItemStatus
import dev.aas.android.protocol.Mention
import dev.aas.android.protocol.Methods
import dev.aas.android.protocol.PickerKind
import dev.aas.android.protocol.PlanModeFeature
import dev.aas.android.protocol.RpcError
import dev.aas.android.protocol.Thread
import dev.aas.android.protocol.Turn
import dev.aas.android.protocol.TurnError
import dev.aas.android.protocol.TurnStatus
import dev.aas.android.protocol.TurnSummary
import dev.aas.android.protocol.Workspace
import dev.aas.android.protocol.WorkspaceSpec
import dev.aas.android.testing.Fixtures
import dev.aas.android.testing.TestEngine
import dev.aas.android.ui.common.UiText
import org.junit.Test
import kotlin.test.assertEquals
import kotlin.test.assertFalse
import kotlin.test.assertNull
import kotlin.test.assertTrue

/**
 * The app's side of the harnesses' own features (docs/android.md 31): which commands the palette
 * offers and a typed first word runs, how errors are said, and which actions on turns and items
 * apply.
 */
class PaletteFeaturesTest {
    private val fixture = Fixtures.result("command_list", CommandListResult.serializer()).commands

    private fun harnessCommand(name: String) = Command(name, "the harness's $name", CommandSource.Harness, action = CommandAction.InsertText("/$name "))

    @Test
    fun theAppsOwnThreadCommandsWinOverTheHarnessesOfTheSameName() {
        // Claude and Devin list `rename`, Devin `status`; the app's own run and the harness's are left out.
        val commands = fixture + listOf("rename", "status", "new", "pin", "goal").map(::harnessCommand)
        val entries = Palette.entries(commands, PaletteContext(inThread = true))
        for (name in listOf("rename", "status", "new", "pin")) {
            assertEquals(listOf(PaletteSource.Local), entries.filter { it.name == name }.map { it.source }, name)
        }
        // Other harness commands (Codex's `/goal`) stay the harness's.
        assertEquals(PaletteSource.Harness, entries.single { it.name == "goal" }.source)
    }

    @Test
    fun clearAndResetAreTheAppsNewThread() {
        val commands = fixture + listOf("clear", "reset").map(::harnessCommand)
        val entries = Palette.entries(commands, PaletteContext(inThread = true))
        assertTrue(entries.none { it.name == "clear" || it.name == "reset" }, "a CLI's new session is the app's /new")
        val new = entries.single { it.name == "new" }
        assertEquals(listOf("clear", "reset"), new.aliases)
        assertEquals(listOf(new), Palette.filter(entries, "cle"), "filtering matches the aliases")
        // Before a thread the harness's `clear` is not offered either.
        assertTrue(Palette.entries(commands, PaletteContext(inThread = false)).none { it.name == "clear" })
    }

    @Test
    fun planIsTheAppsOnlyWhereTheHarnessOffersItsPlanMode() {
        val devinPlan = harnessCommand("plan")
        // Devin (no feature): its own /plan stays.
        val devin = Palette.entries(fixture + devinPlan, PaletteContext(inThread = true))
        assertEquals(listOf(PaletteSource.Harness), devin.filter { it.name == "plan" }.map { it.source })
        // Claude and Codex (feature planMode): the app's /plan, also in the new-thread composer.
        val claude = Palette.entries(fixture + devinPlan, PaletteContext(inThread = true, planMode = true))
        val plan = claude.single { it.name == "plan" }
        assertEquals(PaletteAction.Local(LocalCommand.Plan), plan.action)
        assertEquals(UiText.of(R.string.command_hint_plan), plan.argumentHint)
        assertTrue(Palette.entries(emptyList(), PaletteContext(inThread = false, planMode = true)).any { it.name == "plan" })
    }

    @Test
    fun btwNeedsSideQuestionsAndAThread() {
        assertTrue(Palette.entries(fixture, PaletteContext(inThread = true, sideQuestion = true)).any { it.name == "btw" && it.source == PaletteSource.Local })
        assertFalse(Palette.entries(fixture, PaletteContext(inThread = true)).any { it.name == "btw" })
        assertFalse(Palette.entries(emptyList(), PaletteContext(inThread = false, sideQuestion = true)).any { it.name == "btw" })
    }

    @Test
    fun theProtocolsAppCommandsFollowTheHarness() {
        val harness = TestEngine.fakeHarness()
        val names = Palette.protocolAppCommands(harness, inThread = true).map { it.name }
        assertEquals(listOf("model", "effort", "permissions", "fork", "diff", "stop", "resume-queue", "archive"), names)
        assertEquals(listOf("model", "effort", "permissions"), Palette.protocolAppCommands(harness, inThread = false).map { it.name })
        val bare = harness.copy(models = emptyList(), effortLevels = emptyList(), permissionModes = emptyList(), capabilities = harness.capabilities.copy(fork = false))
        assertEquals(listOf("diff", "stop", "resume-queue", "archive"), Palette.protocolAppCommands(bare, inThread = true).map { it.name })
        assertEquals(
            CommandAction.Method(Methods.ThreadStop.name),
            Palette.protocolAppCommands(harness, inThread = true).single { it.name == "stop" }.action,
        )
    }
}

class TypedCommandsTest {
    private val entries = Palette.entries(
        Palette.protocolAppCommands(TestEngine.fakeHarness(), inThread = true) +
            Command("goal", "Set a goal", CommandSource.Harness, action = CommandAction.InsertText("/goal ")),
        PaletteContext(inThread = true, canImport = true, planMode = true, sideQuestion = true),
    )

    @Test
    fun theFirstWordAndItsArgument() {
        assertEquals("rename" to "New title here", TypedCommands.split("  /rename New title here  "))
        assertEquals("status" to "", TypedCommands.split("/status"))
        assertNull(TypedCommands.split("please /rename x"), "only the first word")
        assertNull(TypedCommands.split("/"), "a slash alone is no command")
        assertNull(TypedCommands.split("plain text"))
    }

    @Test
    fun theAppsCommandsAreRecognisedWithTheirArgumentsAndAliases() {
        val rename = TypedCommands.resolve("/rename Login work", entries)!!
        assertEquals(PaletteAction.Local(LocalCommand.Rename), rename.entry.action)
        assertEquals("Login work", rename.args)
        val clear = TypedCommands.resolve("/clear start over", entries)!!
        assertEquals(PaletteAction.Local(LocalCommand.New), clear.entry.action)
        assertEquals("clear", clear.typedName)
        assertEquals("start over", clear.args)
        assertEquals(PaletteAction.Local(LocalCommand.New), TypedCommands.resolve("/reset", entries)!!.entry.action)
        assertEquals(PaletteAction.Picker(PickerKind.Model), TypedCommands.resolve("/model large", entries)!!.entry.action)
        assertEquals(PaletteAction.Method(CommandAction.Method(Methods.ThreadStop.name)), TypedCommands.resolve("/stop", entries)!!.entry.action)
        assertEquals(PaletteAction.Local(LocalCommand.Plan), TypedCommands.resolve("/plan add a login page", entries)!!.entry.action)
        assertEquals(PaletteAction.Local(LocalCommand.Btw), TypedCommands.resolve("/btw which file?", entries)!!.entry.action)
    }

    @Test
    fun harnessCommandsAndTextAreSentAsTheyAre() {
        assertNull(TypedCommands.resolve("/goal ship it", entries), "Codex's /goal goes to the harness")
        assertNull(TypedCommands.resolve("/compact", entries), "a command nobody lists goes to the harness")
        assertNull(TypedCommands.resolve("/Rename x", entries), "names are exact")
        assertNull(TypedCommands.resolve("fix the /status page", entries))
    }

    /**
     * The prompt templates yield to a harness command of their name, which only the daemon's
     * loaded list shows: without it (offline, loading, failed) `/review` and `/init` go as typed,
     * so the daemon runs Codex's `review/start` and bundled `/init` (and Claude's own commands)
     * whether or not the list happened to load. The app's other commands resolve either way.
     */
    @Test
    fun thePromptTemplatesNeedTheLoadedListToKnowTheHarnessHasNoCommandOfTheirName() {
        val harness = TestEngine.fakeHarness()
        val context = PaletteContext(inThread = true, planMode = true)
        val fallback = Palette.protocolAppCommands(harness, inThread = true)
        val codex = fallback + listOf(
            Command("review", "Review", CommandSource.Harness, action = CommandAction.InsertText("/review ")),
            Command("init", "AGENTS.md", CommandSource.Harness, action = CommandAction.InsertText("/init")),
        )
        // Not loaded: sent as typed, never the app's template.
        assertNull(TypedCommands.resolve("/review focus on auth", null, fallback, context))
        assertNull(TypedCommands.resolve("/init", null, fallback, context))
        // Loaded: the harness has them (sent as typed), or has not (the app's templates).
        assertNull(TypedCommands.resolve("/review focus on auth", codex, fallback, context))
        assertNull(TypedCommands.resolve("/init", codex, fallback, context))
        val review = TypedCommands.resolve("/review focus on auth", fallback, fallback, context)!!
        assertEquals(PaletteAction.Local(LocalCommand.Review), review.entry.action)
        assertEquals("focus on auth", review.args)
        assertEquals(PaletteAction.Local(LocalCommand.Init), TypedCommands.resolve("/init", fallback, fallback, context)!!.entry.action)
        // The app's own commands and the daemon's app commands do not depend on the list.
        assertEquals(PaletteAction.Local(LocalCommand.Plan), TypedCommands.resolve("/plan x", null, fallback, context)!!.entry.action)
        assertEquals(PaletteAction.Local(LocalCommand.Rename), TypedCommands.resolve("/rename x", null, fallback, context)!!.entry.action)
        assertEquals(PaletteAction.Picker(PickerKind.Model), TypedCommands.resolve("/model small", null, fallback, context)!!.entry.action)
    }
}

class ErrorTextsTest {
    private fun error(kind: String, code: Int, data: String) = RpcError(code, "the harness reported an error: raw", AasJson.parseToJsonElement(data))

    @Test
    fun theHarnessesOwnWordsFollowAJapaneseLeadIn() {
        val adapter = error("adapterError", -32011, """{"kind":"adapterError","detail":"not logged in","harnessId":"codex"}""")
        assertEquals(UiText.of(R.string.error_harness_reported, "not logged in"), ErrorTexts.server(adapter))
        val what = UiText.of(R.string.request_turn_start)
        assertEquals(UiText.of(R.string.request_failed_harness, what, "not logged in"), ErrorTexts.requestFailed(what, adapter))
        // An older daemon without `detail`: its message as it is.
        val old = RpcError(-32011, "the harness reported an error: x")
        assertEquals(UiText.of(R.string.error_harness_reported, "the harness reported an error: x"), ErrorTexts.server(old))
    }

    @Test
    fun aRefusedSessionSwitchSaysWhatToUseInstead() {
        val refused = error("sessionSwitchingCommand", -32014, """{"kind":"sessionSwitchingCommand","command":"clear","harnessId":"claude"}""")
        assertEquals(UiText.of(R.string.request_refused_switching, "clear"), ErrorTexts.requestFailed(UiText.of(R.string.request_turn_start), refused))
        assertEquals(UiText.of(R.string.request_refused_switching, "clear"), ErrorTexts.server(refused))
    }

    @Test
    fun turnErrorsHaveALeadInForTheirKind() {
        assertEquals(UiText.of(R.string.turn_error_format, UiText.of(R.string.turn_error_resume), "session is held"), ErrorTexts.turnError("resumeFailed", "session is held"))
        assertEquals(UiText.of(R.string.turn_error_spawn), ErrorTexts.leadIn("spawnFailed"))
        assertEquals(UiText.of(R.string.turn_error_codex, "usageLimitExceeded"), ErrorTexts.leadIn("codex:usageLimitExceeded"))
        assertEquals(UiText.of(R.string.turn_error_other, "somethingNew"), ErrorTexts.leadIn("somethingNew"))
        assertEquals(UiText.of(R.string.turn_error_line, UiText.of(R.string.turn_error_agent_exited), "code 1"), ErrorTexts.turnErrorLine("agentExited", "code 1"))
    }
}

class ThreadActionsTest {
    private val base = TestEngine.fakeHarness()
    private val atTurn = base.copy(features = HarnessFeatures(forkAtTurn = true, forkWhileHeld = true, moveToBackground = true))
    private val thread: Thread = Fixtures.threadRead.thread.copy(
        lastTurn = TurnSummary("trn_3", 2, TurnStatus.Completed, 1),
        nativeSessionId = "ses",
        archived = false,
        workspace = Workspace.Local,
    )

    private fun turn(id: String, index: Int, forkable: Boolean = true, status: TurnStatus = TurnStatus.Completed, error: TurnError? = null) =
        Turn(id, thread.id, index, status, startedAt = index.toLong(), forkable = forkable, error = error)

    @Test
    fun forksAtAnyTurnNeedTheFeatureAndTheTurnsAnchor() {
        val first = turn("trn_1", 0, forkable = false)
        val middle = turn("trn_2", 1)
        val last = turn("trn_3", 2, forkable = false)
        // Without forkAtTurn only the last turn, as a fork of the whole session.
        assertEquals(ForkChoices(here = true, editPrompt = false), ThreadActions.fork(thread, base, last, hasPrompt = true))
        assertEquals(ForkChoices.None, ThreadActions.fork(thread, base, middle, hasPrompt = true))
        // With it: a turn with its anchor, both ways; the first turn's prompt without one (a new session).
        assertEquals(ForkChoices(here = true, editPrompt = true), ThreadActions.fork(thread, atTurn, middle, hasPrompt = true))
        assertEquals(ForkChoices(here = false, editPrompt = true), ThreadActions.fork(thread, atTurn, first, hasPrompt = true))
        assertEquals(ForkChoices(here = true, editPrompt = false), ThreadActions.fork(thread, atTurn, last, hasPrompt = true))
        // No prompt to edit (a turn the agent started by itself).
        assertEquals(ForkChoices(here = true, editPrompt = false), ThreadActions.fork(thread, atTurn, middle, hasPrompt = false))
    }

    @Test
    fun noForkWhileATurnRunsWithoutASessionOrOnAnArchivedThread() {
        val middle = turn("trn_2", 1)
        assertEquals(ForkChoices.None, ThreadActions.fork(thread.copy(lastTurn = thread.lastTurn!!.copy(status = TurnStatus.Running)), atTurn, middle, true))
        assertEquals(ForkChoices.None, ThreadActions.fork(thread.copy(nativeSessionId = null), atTurn, middle, true))
        assertEquals(ForkChoices.None, ThreadActions.fork(thread.copy(archived = true), atTurn, middle, true))
        assertEquals(ForkChoices.None, ThreadActions.fork(thread, atTurn.copy(capabilities = atTurn.capabilities.copy(fork = false)), middle, true))
    }

    @Test
    fun aFailedResumeOffersARetryAndWhereTheHarnessCanAFork() {
        val failed = turn("trn_3", 2, status = TurnStatus.Failed, error = TurnError("held by another process", ErrorTexts.RESUME_FAILED))
        assertEquals(ResumeFailedChoices(retry = true, fork = true), ThreadActions.resumeFailed(thread, atTurn, failed, hasPrompt = true))
        assertEquals(ResumeFailedChoices(retry = true, fork = false), ThreadActions.resumeFailed(thread, base, failed, hasPrompt = true))
        // A fork that has no session yet: only the retry (which starts it).
        assertEquals(ResumeFailedChoices(retry = true, fork = false), ThreadActions.resumeFailed(thread.copy(nativeSessionId = null), atTurn, failed, hasPrompt = true))
        // Only on the latest turn, and only for resumeFailed.
        assertNull(ThreadActions.resumeFailed(thread, atTurn, failed.copy(id = "trn_1"), hasPrompt = true))
        assertNull(ThreadActions.resumeFailed(thread, atTurn, failed.copy(error = TurnError("x", "spawnFailed")), hasPrompt = true))
    }

    @Test
    fun aForkAfterAFailedResumeBranchesAtTheLastTurnThatRan() {
        val held = TurnError("held by another process", ErrorTexts.RESUME_FAILED)
        val ran = turn("trn_1", 0)
        val failedBefore = turn("trn_2", 1, forkable = false, status = TurnStatus.Failed, error = held)
        val failed = turn("trn_3", 2, forkable = false, status = TurnStatus.Failed, error = held)
        // Over earlier failed resumes, at the last turn the agent ran (its anchor was recorded).
        assertEquals(ran, ThreadActions.resumeFailedForkPoint(atTurn, listOf(ran, failedBefore, failed), failed))
        assertEquals(ran, ThreadActions.resumeFailedForkPoint(atTurn, listOf(ran, failed), failed))
        // The whole session: no forkAtTurn, no anchor, a turn in between that ran without one,
        // nothing before the failed turn, or the turn is not loaded.
        assertNull(ThreadActions.resumeFailedForkPoint(base, listOf(ran, failed), failed))
        assertNull(ThreadActions.resumeFailedForkPoint(atTurn, listOf(ran.copy(forkable = false), failed), failed))
        val ranWithoutAnchor = turn("trn_2", 1, forkable = false, status = TurnStatus.Failed, error = TurnError("exited", "agentExited"))
        assertNull(ThreadActions.resumeFailedForkPoint(atTurn, listOf(ran, ranWithoutAnchor, failed), failed))
        assertNull(ThreadActions.resumeFailedForkPoint(atTurn, listOf(failed), failed))
        assertNull(ThreadActions.resumeFailedForkPoint(atTurn, listOf(ran), failed))
    }

    @Test
    fun aProposedPlanIsImplementedTheWaysTheHarnessGives() {
        val planning = atTurn.copy(features = atTurn.features.copy(planMode = PlanModeFeature(implementPrompt = "Implement the plan.", newThreadPreamble = "Implement this plan:")))
        val plan = Item.ProposedPlan("itm_p", thread.id, "trn_3", ItemStatus.Completed, 1, text = "1. Do it")
        assertEquals(PlanChoices(implement = true, newThread = true), ThreadActions.proposedPlan(thread, planning, plan))
        // A thread in a worktree too: the new thread works in that worktree (the plan is about it).
        val inWorktree = thread.copy(workspace = Workspace.Worktree("w", "b", "main"))
        assertEquals(PlanChoices(implement = true, newThread = true), ThreadActions.proposedPlan(inWorktree, planning, plan))
        assertEquals(WorkspaceSpec.Thread(thread.id), ThreadActions.newThreadWorkspace(inWorktree))
        // A thread in the project's folder: the folder, as every daemon takes it.
        assertEquals(WorkspaceSpec.Local, ThreadActions.newThreadWorkspace(thread))
        // Not while its turn runs, not for an older turn's plan, not while it streams, not without the feature.
        assertEquals(PlanChoices.None, ThreadActions.proposedPlan(thread.copy(lastTurn = thread.lastTurn!!.copy(status = TurnStatus.Running)), planning, plan))
        assertEquals(PlanChoices.None, ThreadActions.proposedPlan(thread, planning, plan.copy(turnId = "trn_1")))
        assertEquals(PlanChoices.None, ThreadActions.proposedPlan(thread, planning, plan.copy(status = ItemStatus.InProgress)))
        assertEquals(PlanChoices.None, ThreadActions.proposedPlan(thread, atTurn, plan))
        // Claude continues through the approval of its plan: no implement prompt.
        assertEquals(PlanChoices(implement = false, newThread = false), ThreadActions.proposedPlan(thread, atTurn.copy(features = atTurn.features.copy(planMode = PlanModeFeature())), plan))
        assertEquals("Implement this plan:\n\n1. Do it", ThreadActions.newThreadInput("Implement this plan:", plan))
    }

    @Test
    fun onlyARunningItemTheHarnessMarkedMovesToTheBackground() {
        val running = Item.CommandExecution("itm_c", thread.id, "trn_3", ItemStatus.InProgress, 1, command = "npm test", output = "", backgroundable = true)
        assertTrue(ThreadActions.canMoveToBackground(atTurn, running))
        assertFalse(ThreadActions.canMoveToBackground(base, running), "the harness must offer the move")
        assertFalse(ThreadActions.canMoveToBackground(atTurn, running.copy(backgroundable = false)))
        assertFalse(ThreadActions.canMoveToBackground(atTurn, running.copy(status = ItemStatus.Completed)))
    }

    @Test
    fun aTurnsPromptIsItsUserMessageWithMentionsAndImages() {
        val turn = turn("trn_2", 1)
        val message = Item.UserMessage(
            "itm_u", thread.id, "trn_2", ItemStatus.Completed, 1,
            text = "fix @src/a.rs please",
            attachments = listOf(Attachment.Image("blb_1", "image/png")),
            mentions = listOf(Mention("src/a.rs")),
        )
        val draft = ThreadActions.promptOf(turn, listOf(message))!!
        assertEquals("fix @src/a.rs please", draft.text)
        assertEquals(setOf("src/a.rs"), draft.mentions)
        assertEquals(listOf("blb_1"), draft.images.map { it.image.blobId })
        assertNull(draft.images.single().localUri, "only the daemon has the image")
        assertNull(ThreadActions.promptOf(turn, emptyList()))
    }
}
