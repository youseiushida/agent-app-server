package dev.aas.android.protocol

import kotlinx.serialization.json.JsonObject
import kotlin.test.Test
import kotlin.test.assertEquals
import kotlin.test.assertFalse
import kotlin.test.assertIs
import kotlin.test.assertNull
import kotlin.test.assertTrue

/**
 * The additions for the harnesses' own features (protocol.md §3.1 「ハーネスの機能」): fields the
 * server leaves out while they are off decode to off and are left out again, and the new
 * variants, events and errors carry what the app reads.
 */
class HarnessFeaturesDecodingTest {
    @Test
    fun anOlderServersHarnessThreadTurnAndItemHaveEverythingOff() {
        val harness = AasJson.decodeFromString(
            Harness.serializer(),
            """{"id":"fake","kind":"fake","displayName":"Fake","available":true}""",
        )
        assertEquals(HarnessFeatures(), harness.features)
        assertNull(harness.features.planMode)
        val thread = AasJson.decodeFromString(
            Thread.serializer(),
            """{"id":"thr_1","projectId":"prj_1","harnessId":"fake","title":"t","cwd":"C:/p","workspace":{"kind":"local"},""" +
                """"status":"idle","createdAt":1,"updatedAt":1,"lastActivityAt":1}""",
        )
        assertEquals(ThreadModes(plan = false, fast = false), thread.modes)
        assertNull(thread.fastModeState)
        val turn = AasJson.decodeFromString(Turn.serializer(), """{"id":"trn_1","threadId":"thr_1","index":0,"status":"completed","startedAt":1}""")
        assertFalse(turn.forkable)
        val project = AasJson.decodeFromString(Project.serializer(), """{"id":"prj_1","name":"p","path":"C:/p","createdAt":1,"updatedAt":1}""")
        assertTrue(project.harnessTrust.isEmpty())
    }

    @Test
    fun fieldsThatAreOffAreLeftOutLikeTheServerWritesThem() {
        val features = AasJson.encodeToJsonElement(HarnessFeatures.serializer(), HarnessFeatures(rename = true)) as JsonObject
        assertEquals(setOf("rename"), features.keys)
        val turn = AasJson.encodeToJsonElement(Turn.serializer(), Turn("trn_1", "thr_1", 0, TurnStatus.Completed, 1)) as JsonObject
        assertFalse("forkable" in turn)
        val fork = AasJson.encodeToJsonElement(ThreadForkParams.serializer(), ThreadForkParams("c", "thr_1", "trn_1")) as JsonObject
        assertFalse("before" in fork)
        val before = AasJson.encodeToJsonElement(ThreadForkParams.serializer(), ThreadForkParams("c", "thr_1", "trn_1", before = true)) as JsonObject
        assertEquals("true", before["before"].toString())
        val item = AasJson.encodeToJsonElement(Item.serializer(), Item.AgentMessage("i", "t", "u", ItemStatus.InProgress, 1, text = "x")) as JsonObject
        assertFalse("backgroundable" in item)
        // Modes are always written, both of them (the server writes them so).
        val modes = AasJson.encodeToJsonElement(ThreadModes.serializer(), ThreadModes()) as JsonObject
        assertEquals(setOf("plan", "fast"), modes.keys)
    }

    @Test
    fun aProposedPlanStreamsItsText() {
        val plan = Item.ProposedPlan("itm_1", "thr_1", "trn_1", ItemStatus.InProgress, 1, text = "1. Look")
        val grown = assertIs<Item.ProposedPlan>(plan.appendDelta(DeltaField.Text, " into it"))
        assertEquals("1. Look into it", grown.text)
        val decoded = AasJson.decodeFromString(
            Item.serializer(),
            """{"kind":"proposedPlan","id":"itm_1","threadId":"thr_1","turnId":"trn_1","status":"completed","startedAt":1,"text":"# Plan"}""",
        )
        assertEquals("# Plan", assertIs<Item.ProposedPlan>(decoded).text)
    }

    @Test
    fun anUnknownItemKindStillSaysWhetherItCanMoveToTheBackground() {
        val item = AasJson.decodeFromString(
            Item.serializer(),
            """{"kind":"hologram","id":"itm_1","threadId":"thr_1","turnId":"trn_1","status":"inProgress","startedAt":7,"backgroundable":true}""",
        )
        assertTrue(item.backgroundable)
    }

    @Test
    fun theNewEventsDecode() {
        val changed = AasJson.decodeFromString(
            EventEnvelope.serializer(),
            """{"seq":3,"ts":1,"type":"thread/nativeSessionChanged","data":{"previousNativeSessionId":"a","nativeSessionId":"b"}}""",
        )
        assertEquals(Event.NativeSessionChanged("a", "b"), changed.event)
        val insert = AasJson.decodeFromString(EventEnvelope.serializer(), """{"seq":4,"ts":1,"type":"composer/insert","data":{"text":"hi"}}""")
        assertEquals(Event.ComposerInsert("hi"), insert.event)
    }

    @Test
    fun theErrorsCarryTheHarnessesOwnWordsAndTheRefusedCommand() {
        val switching = RpcError(
            -32014,
            "clear switches the session",
            AasJson.parseToJsonElement("""{"kind":"sessionSwitchingCommand","command":"clear","harnessId":"claude"}"""),
        )
        assertEquals(ErrorKind.SessionSwitchingCommand, switching.kind)
        assertTrue(switching.kind.definitive)
        assertEquals("clear", switching.command)
        assertEquals("claude", switching.harnessId)
        val adapter = RpcError(
            -32011,
            "the harness reported an error: not logged in",
            AasJson.parseToJsonElement("""{"kind":"adapterError","detail":"not logged in","harnessId":"codex"}"""),
        )
        assertEquals("not logged in", adapter.detail)
        // An older server without `detail`: nothing is made up.
        assertNull(RpcError(-32011, "the harness reported an error: x").detail)
    }

    @Test
    fun anUnknownRenameStatusIsTolerated() {
        val result = AasJson.decodeFromString(
            NativeRename.serializer(),
            """{"status":"queuedSomewhere","message":"m"}""",
        )
        assertEquals(NativeRenameStatus.Unknown, result.status)
    }
}
