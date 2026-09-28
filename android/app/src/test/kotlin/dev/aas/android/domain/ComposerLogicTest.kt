package dev.aas.android.domain

import dev.aas.android.R
import dev.aas.android.domain.composer.ComposerText
import dev.aas.android.domain.composer.ComposerTextState
import dev.aas.android.domain.composer.ComposerTrigger
import dev.aas.android.domain.composer.FollowUpDelivery
import dev.aas.android.domain.composer.HarnessSettings
import dev.aas.android.domain.composer.LocalCommand
import dev.aas.android.domain.composer.Palette
import dev.aas.android.domain.composer.PaletteAction
import dev.aas.android.domain.composer.PaletteContext
import dev.aas.android.domain.composer.PaletteSource
import dev.aas.android.domain.composer.SendAction
import dev.aas.android.domain.composer.SendBlock
import dev.aas.android.domain.composer.SendLogic
import dev.aas.android.protocol.Command
import dev.aas.android.protocol.CommandAction
import dev.aas.android.protocol.CommandListResult
import dev.aas.android.protocol.CommandSource
import dev.aas.android.protocol.Delivery
import dev.aas.android.protocol.HarnessCapabilities
import dev.aas.android.protocol.InputPart
import dev.aas.android.protocol.PickerKind
import dev.aas.android.protocol.ProjectDefaults
import dev.aas.android.protocol.ThreadSettings
import dev.aas.android.protocol.TurnStatus
import dev.aas.android.sync.Samples
import dev.aas.android.testing.Fixtures
import dev.aas.android.testing.TestEngine
import dev.aas.android.ui.common.UiText
import org.junit.Test
import kotlin.test.assertEquals
import kotlin.test.assertFalse
import kotlin.test.assertNull
import kotlin.test.assertTrue

class ComposerTextTest {
    private fun state(text: String, cursor: Int = text.length) = ComposerTextState(text, cursor)

    @Test
    fun theSlashPaletteOpensOnlyAtTheStartOfTheMessage() {
        assertEquals(ComposerTrigger.Slash("", 1), ComposerText.trigger(state("/")))
        assertEquals(ComposerTrigger.Slash("co", 3), ComposerText.trigger(state("/co")))
        assertEquals(ComposerTrigger.Slash("co", 8), ComposerText.trigger(state("/compact now", 3)))
        // After the command word, or anywhere else, it is ordinary text.
        assertNull(ComposerText.trigger(state("/compact now")))
        assertNull(ComposerText.trigger(state("use /tmp")))
    }

    @Test
    fun mentionsOpenAtAnAtSignThatStartsAWord() {
        assertEquals(ComposerTrigger.Mention("rea", 4, 8), ComposerText.trigger(state("fix @rea please", 8)))
        assertEquals(ComposerTrigger.Mention("", 0, 1), ComposerText.trigger(state("@")))
        // The whole token is replaced even with the cursor inside it.
        assertEquals(ComposerTrigger.Mention("re", 4, 9), ComposerText.trigger(state("fix @read", 7)))
        assertNull(ComposerText.trigger(state("mail me@example.com")))
        assertNull(ComposerText.trigger(state("fix @read ", 10)))
    }

    @Test
    fun choosingInsertsTheCommandOrTheMention() {
        val slash = ComposerText.trigger(state("/co")) as ComposerTrigger.Slash
        assertEquals(state("/compact "), ComposerText.insertCommand(state("/co"), slash, "/compact "))
        val withArgs = state("/co keep this", 3)
        val t = ComposerText.trigger(withArgs) as ComposerTrigger.Slash
        assertEquals(ComposerTextState("/compact keep this", 9), ComposerText.insertCommand(withArgs, t, "/compact "))

        val typed = state("look at @rec and", 12)
        val m = ComposerText.trigger(typed) as ComposerTrigger.Mention
        assertEquals(ComposerTextState("look at @src/reconnect.rs and", 26), ComposerText.insertMention(typed, m, "src/reconnect.rs"))
    }

    @Test
    fun eachMentionTokenBecomesAMentionPartInItsPlace() {
        // The daemon writes a mention part into the message as "@path" (protocol.md §3): the
        // token must not also stay in the text, or the agent and the bubble get it twice.
        val input = ComposerText.input("  see @a.rs, not @b.rs  \n", listOf("a.rs", "b.rs", "c.rs", "a.rs"), listOf("blb_1"))
        assertEquals(
            listOf(
                InputPart.Text("see "), InputPart.Mention("a.rs"), InputPart.Text(", not "), InputPart.Mention("b.rs"),
                InputPart.Image("blb_1"),
            ),
            input,
        )
        assertEquals(listOf(InputPart.Text("see "), InputPart.Mention("src/a.rs"), InputPart.Text(" please")), ComposerText.input("see @src/a.rs please", setOf("src/a.rs"), emptyList()))
        // Every occurrence, at the start too; the longest chosen path wins at a position.
        assertEquals(
            listOf(InputPart.Mention("a.rs"), InputPart.Text(" and "), InputPart.Mention("a.rs.bak"), InputPart.Text(" and "), InputPart.Mention("a.rs")),
            ComposerText.input("@a.rs and @a.rs.bak and @a.rs", listOf("a.rs", "a.rs.bak"), emptyList()),
        )
        // "@a.rsx" must be a whole token: "@a.rsx" is another path; so is "x@a.rs".
        assertEquals(listOf(InputPart.Text("@a.rsx")), ComposerText.input("@a.rsx", listOf("a.rs"), emptyList()))
        assertEquals(listOf(InputPart.Text("x@a.rs")), ComposerText.input("x@a.rs", listOf("a.rs"), emptyList()))
        // A mention typed but no longer in the text is not sent.
        assertEquals(listOf(InputPart.Text("plain")), ComposerText.input("plain", listOf("a.rs"), emptyList()))
        // Whitespace alone is no text; an image alone is a message.
        assertEquals(listOf(InputPart.Image("blb_2")), ComposerText.input("   ", emptyList(), listOf("blb_2")))
        assertTrue(ComposerText.input(" \n ", emptyList(), emptyList()).isEmpty())
    }

    @Test
    fun theTextOfAnInputIsTheMessageTheDaemonWrites() {
        // Rendered like the daemon: parts in order, a mention as "@path", a space before it when needed.
        val input = ComposerText.input("see @src/a.rs please", setOf("src/a.rs"), listOf("blb_1"))
        assertEquals("see @src/a.rs please", ComposerText.textOf(input))
        assertEquals("fix @a.rs", ComposerText.textOf(listOf(InputPart.Text("fix"), InputPart.Mention("a.rs"))))
        assertEquals("@a.rs", ComposerText.textOf(listOf(InputPart.Mention("a.rs"), InputPart.Image("blb_2"))))
        // Editing a queued message and sending it again gives back the same parts.
        assertEquals(input, ComposerText.input(ComposerText.textOf(input), listOf("src/a.rs"), listOf("blb_1")))
    }

    @Test
    fun paragraphsAreAppendedWithABlankLine() {
        assertEquals("x", ComposerText.appendParagraph("", "x"))
        assertEquals("a\n\nx", ComposerText.appendParagraph("a", "x"))
        assertEquals("a\n\nx", ComposerText.appendParagraph("a\n", "x"))
        assertEquals("a\n\nx", ComposerText.appendParagraph("a\n\n", "x"))
    }
}

class PaletteTest {
    private val fixture = Fixtures.result("command_list", CommandListResult.serializer()).commands

    @Test
    fun appCommandsLocalCommandsThenHarnessCommands() {
        val entries = Palette.entries(fixture, PaletteContext(inThread = true))
        assertEquals(
            listOf("model", "fork", "new", "status", "rename", "pin", "review", "init", "compact"),
            entries.map { it.name },
        )
        assertEquals(listOf(PaletteSource.App, PaletteSource.App), entries.take(2).map { it.source })
        assertEquals(PaletteSource.Harness, entries.last().source)
        // The daemon's app commands get Japanese descriptions; harness commands keep theirs.
        assertEquals(UiText.of(R.string.command_model), entries.first().description)
        assertEquals(UiText.Plain("Compact the conversation"), entries.last().description)
        assertEquals("[instructions]", entries.last().argumentHint)
        assertEquals(PaletteAction.Picker(PickerKind.Model), entries.first().action)
        assertEquals(PaletteAction.Insert("/compact "), entries.last().action)
        assertEquals(PaletteAction.Method(CommandAction.Method("thread/fork")), entries[1].action)
    }

    @Test
    fun localCommandsYieldToTheHarnessAndToTheContext() {
        val claude = fixture + Command("init", "Create CLAUDE.md", CommandSource.Harness, action = CommandAction.InsertText("/init "))
        val names = Palette.entries(claude, PaletteContext(inThread = true)).map { it.name }
        assertEquals(1, names.count { it == "init" })
        assertEquals(PaletteSource.Harness, Palette.entries(claude, PaletteContext(true)).last { it.name == "init" }.source)
        // Before a thread exists only the prompt templates are local commands.
        val newThread = Palette.entries(emptyList(), PaletteContext(inThread = false))
        assertEquals(listOf(LocalCommand.Review, LocalCommand.Init), newThread.map { (it.action as PaletteAction.Local).command })
        assertEquals(UiText.of(R.string.command_unpin), Palette.entries(emptyList(), PaletteContext(true, pinned = true)).first { it.name == "pin" }.description)
        // Unknown action types are listed but not runnable.
        val future = Palette.entries(listOf(Command("x", null, CommandSource.App, action = CommandAction.Unknown("wizard", kotlinx.serialization.json.JsonObject(emptyMap())))), PaletteContext(true))
        assertFalse(future.first().runnable)
    }

    @Test
    fun twoHarnessCommandsOfOneNameAreListedOnce() {
        // Codex lists `review` itself and again for a skill named "review".
        val codex = fixture + listOf(
            Command("review", "Review the changes", CommandSource.Harness, action = CommandAction.InsertText("/review ")),
            Command("review", "Skill: review", CommandSource.Harness, action = CommandAction.InsertText("/review ")),
            Command("compact", "Skill: compact", CommandSource.Harness, action = CommandAction.InsertText("/compact ")),
        )
        val entries = Palette.entries(codex, PaletteContext(inThread = true))
        assertEquals(entries.size, entries.map { it.source to it.name }.toSet().size, "unique (source, name): ${entries.map { it.name }}")
        assertEquals(UiText.Plain("Review the changes"), entries.single { it.name == "review" }.description, "the first one is kept")
        assertEquals(UiText.Plain("Compact the conversation"), entries.single { it.name == "compact" }.description)
        assertEquals(listOf("review"), Palette.filter(entries, "rev").map { it.name })
    }

    @Test
    fun resumeIsTheAppsOwnWhileSomeHarnessCanListItsSessions() {
        // The app's /resume in threads and in the new-thread composer, only when it can import.
        val inThread = Palette.entries(fixture, PaletteContext(inThread = true, canImport = true))
        val resume = inThread.single { it.name == "resume" }
        assertEquals(PaletteSource.Local, resume.source)
        assertEquals(PaletteAction.Local(LocalCommand.Resume), resume.action)
        assertEquals(UiText.of(R.string.command_resume), resume.description)
        assertTrue(Palette.entries(emptyList(), PaletteContext(inThread = false, canImport = true)).any { it.name == "resume" })
        assertFalse(Palette.entries(fixture, PaletteContext(inThread = true, canImport = false)).any { it.name == "resume" })
        assertFalse(Palette.entries(emptyList(), PaletteContext(inThread = false, canImport = false)).any { it.name == "resume" })
    }

    @Test
    fun aHarnessCommandNamedResumeIsNeverOffered() {
        // Claude Code and Codex have their own /resume (a session picker in their terminal UI).
        val harnessResume = Command("resume", "Resume a conversation", CommandSource.Harness, action = CommandAction.InsertText("/resume "))
        val entries = Palette.entries(fixture + harnessResume, PaletteContext(inThread = true, canImport = true))
        assertEquals(listOf(PaletteSource.Local), entries.filter { it.name == "resume" }.map { it.source })
        assertEquals(listOf(PaletteAction.Local(LocalCommand.Resume)), Palette.filter(entries, "resu").map { it.action })
        // Without a harness that can import, neither the app's nor the harness's is offered.
        assertTrue(Palette.entries(fixture + harnessResume, PaletteContext(inThread = true, canImport = false)).none { it.name == "resume" })
    }

    @Test
    fun resumeTypedOutIsRecognisedByItsFirstWord() {
        assertTrue(Palette.isResume("/resume"))
        assertTrue(Palette.isResume("  /resume  "))
        assertTrue(Palette.isResume("/resume 019a-session"))
        assertTrue(Palette.isResume("/resume\nmore"))
        assertFalse(Palette.isResume("/resume-queue"))
        assertFalse(Palette.isResume("/resumes"))
        assertFalse(Palette.isResume("please /resume"))
        assertFalse(Palette.isResume("resume"))
        assertFalse(Palette.isResume(""))
    }

    @Test
    fun filteringPutsPrefixMatchesFirst() {
        val entries = Palette.entries(fixture, PaletteContext(inThread = true))
        assertEquals(listOf("init", "pin"), Palette.filter(entries, "in").map { it.name })
        assertEquals(listOf("compact"), Palette.filter(entries, "COMP").map { it.name })
        assertEquals(entries, Palette.filter(entries, ""))
        assertTrue(Palette.filter(entries, "zzz").isEmpty())
    }
}

class SendLogicTest {
    private val idle = Samples.thread("thr_1").copy(lastTurn = Samples.turnSummary("trn_1", 0, TurnStatus.Completed))
    private val running = idle.copy(lastTurn = Samples.turnSummary("trn_2", 1, TurnStatus.Running))
    private val steer = HarnessCapabilities(steer = true)
    private val noSteer = HarnessCapabilities(steer = false)

    private fun state(
        thread: dev.aas.android.protocol.Thread? = idle,
        caps: HarnessCapabilities = steer,
        content: Boolean = true,
        uploading: Boolean = false,
        failed: Boolean = false,
        followUp: FollowUpDelivery = FollowUpDelivery.Queue,
        interrupting: Boolean = false,
        images: Boolean = false,
    ) = SendLogic.state(thread, caps, content, uploading, failed, followUp, interrupting, images)

    @Test
    fun withoutARunningTurnItSends() {
        assertEquals(SendAction.Start, state().primary)
        assertTrue(state().enabled)
        assertEquals(SendBlock.Empty, state(content = false).blocked)
        assertEquals(SendBlock.Uploading, state(uploading = true).blocked)
        assertEquals(SendBlock.UploadFailed, state(failed = true).blocked)
        assertEquals(SendBlock.NotLoaded, state(thread = null).blocked)
        assertEquals(SendBlock.Archived, state(thread = idle.copy(archived = true)).blocked)
        assertEquals(Delivery.Auto, SendLogic.delivery(SendAction.Start))
    }

    @Test
    fun imagesNeedAHarnessThatTakesImages() {
        assertEquals(SendBlock.ImagesUnsupported, state(caps = HarnessCapabilities(images = false), images = true).blocked)
        assertEquals(SendBlock.ImagesUnsupported, state(running, caps = HarnessCapabilities(images = false, steer = true), images = true).blocked)
        assertTrue(state(caps = HarnessCapabilities(images = true), images = true).enabled)
        assertTrue(state(caps = HarnessCapabilities(images = false), images = false).enabled)
        // An upload in progress is said first.
        assertEquals(SendBlock.Uploading, SendLogic.attachmentsBlock(HarnessCapabilities(images = false), uploading = true, uploadFailed = false, hasImages = true))
        assertNull(SendLogic.attachmentsBlock(null, uploading = false, uploadFailed = false, hasImages = true), "an unknown harness decides nothing")
    }

    @Test
    fun whileATurnRunsItStopsQueuesOrSteers() {
        // Empty composer: stop (turn/interrupt), disabled while an interrupt waits for its answer.
        assertEquals(SendAction.Interrupt, state(running, content = false).primary)
        assertTrue(state(running, content = false).enabled)
        assertEquals(SendBlock.Interrupting, state(running, content = false, interrupting = true).blocked)
        // A draft: the preferred follow-up, the other on long press.
        val queue = state(running)
        assertEquals(SendAction.Queue, queue.primary)
        assertEquals(SendAction.Steer, queue.alternate)
        val steerFirst = state(running, followUp = FollowUpDelivery.Steer)
        assertEquals(SendAction.Steer, steerFirst.primary)
        assertEquals(SendAction.Queue, steerFirst.alternate)
        // Without the capability there is only the queue.
        val plain = state(running, caps = noSteer, followUp = FollowUpDelivery.Steer)
        assertEquals(SendAction.Queue, plain.primary)
        assertNull(plain.alternate)
        assertEquals(Delivery.Queue, SendLogic.delivery(SendAction.Queue))
        assertEquals(Delivery.Steer, SendLogic.delivery(SendAction.Steer))
        // A pending upload blocks a follow-up too.
        assertEquals(SendBlock.Uploading, state(running, content = false, uploading = true).blocked)
    }

    @Test
    fun aPausedQueueAsksAndQueuedMessagesCanBeSentNow() {
        val paused = idle.copy(queuePaused = true)
        val queued = listOf(Samples.queued("que_1"))
        assertTrue(SendLogic.needsPausedQueueConfirmation(paused, queued, SendAction.Start))
        assertFalse(SendLogic.needsPausedQueueConfirmation(paused, emptyList(), SendAction.Start))
        assertFalse(SendLogic.needsPausedQueueConfirmation(idle, queued, SendAction.Start))
        assertFalse(SendLogic.needsPausedQueueConfirmation(paused, queued, SendAction.Queue))
        assertTrue(SendLogic.canSendQueuedNow(idle, noSteer))
        assertTrue(SendLogic.canSendQueuedNow(running, steer))
        assertFalse(SendLogic.canSendQueuedNow(running, noSteer))
        assertFalse(SendLogic.canSendQueuedNow(idle.copy(archived = true), steer))
    }
}

class HarnessSettingsTest {
    private val harness = TestEngine.fakeHarness()

    @Test
    fun effectiveValuesComeFromTheThreadThenTheHarness() {
        assertEquals("small", HarnessSettings.model(harness, ThreadSettings())?.id)
        assertEquals("large", HarnessSettings.model(harness, ThreadSettings(model = "large"))?.id)
        assertEquals(listOf("low"), HarnessSettings.effortLevels(harness, "small").map { it.id })
        assertEquals(listOf("low", "high"), HarnessSettings.effortLevels(harness, "large").map { it.id })
        assertNull(HarnessSettings.effort(harness, ThreadSettings()))
        assertEquals("ask", HarnessSettings.permission(harness, ThreadSettings())?.id)
        assertFalse(HarnessSettings.needsConfirmation(harness, harness.permissionModes[0]))
        assertTrue(HarnessSettings.needsConfirmation(harness, harness.permissionModes[1]))
        assertEquals("Fake · Large · High", HarnessSettings.label(harness, ThreadSettings(model = "large", effort = "high")))
    }

    /**
     * Effort levels are the harness's own list: when it lists `ultracode` (Claude Code, for models
     * whose levels include it) the picker offers it like any level, only for those models, and
     * the chip names it. No level is added, hidden or preferred by the app.
     */
    @Test
    fun ultracodeIsOfferedExactlyWhereTheHarnessListsIt() {
        val claude = harness.copy(
            models = listOf(
                dev.aas.android.protocol.Model("opus", "Opus", isDefault = true, effortLevels = listOf("low", "high", "xhigh", "ultracode")),
                dev.aas.android.protocol.Model("haiku", "Haiku"),
                dev.aas.android.protocol.Model("sonnet", "Sonnet", effortLevels = listOf("low", "high")),
            ),
            defaultModel = "opus",
            effortLevels = listOf(
                dev.aas.android.protocol.EffortLevel("low", "Low"),
                dev.aas.android.protocol.EffortLevel("high", "High"),
                dev.aas.android.protocol.EffortLevel("xhigh", "Extra high"),
                dev.aas.android.protocol.EffortLevel("ultracode", "Ultracode"),
            ),
        )
        assertEquals(listOf("low", "high", "xhigh", "ultracode"), HarnessSettings.effortLevels(claude, "opus").map { it.id })
        assertEquals(listOf("low", "high"), HarnessSettings.effortLevels(claude, "sonnet").map { it.id })
        assertEquals("Fake · Opus · Ultracode", HarnessSettings.label(claude, ThreadSettings(effort = "ultracode")))
        // The harness's default applies until one is chosen.
        assertNull(HarnessSettings.effort(claude, ThreadSettings()))
    }

    @Test
    fun newThreadsStartFromTheProjectsLastChoicesForTheSameHarness() {
        val defaults = ProjectDefaults(harnessId = "fake", model = "large", effort = "high", permissionMode = "full")
        assertEquals(ThreadSettings("large", "high", "full"), HarnessSettings.initial(harness, defaults))
        // Another harness's defaults, and values the harness no longer lists, are not used.
        assertEquals(ThreadSettings(), HarnessSettings.initial(harness, defaults.copy(harnessId = "other")))
        assertEquals(ThreadSettings(effort = null, permissionMode = "full"), HarnessSettings.initial(harness, defaults.copy(model = "gone", effort = "high")))
        val unavailable = harness.copy(id = "off", available = false)
        assertEquals("fake", HarnessSettings.initialHarness(listOf(unavailable, harness), ProjectDefaults(harnessId = "off"))?.id)
        assertNull(HarnessSettings.initialHarness(listOf(unavailable), ProjectDefaults()))
    }
}
