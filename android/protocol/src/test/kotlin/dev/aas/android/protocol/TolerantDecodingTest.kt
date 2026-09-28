package dev.aas.android.protocol

import kotlinx.serialization.json.JsonObject
import kotlinx.serialization.json.JsonPrimitive
import kotlin.test.Test
import kotlin.test.assertEquals
import kotlin.test.assertIs
import kotlin.test.assertNull

/** A newer server may add fields, event types, item kinds and enum values within v1. */
class TolerantDecodingTest {
    @Test
    fun unknownEventTypeDecodesToUnknown() {
        val env = AasJson.decodeFromString(
            EventEnvelope.serializer(),
            """{"seq":5,"ts":1,"type":"thread/somethingNew","data":{"x":1}}""",
        )
        val unknown = assertIs<Event.Unknown>(env.event)
        assertEquals("thread/somethingNew", unknown.type)
        // Unknown events survive a round trip unchanged.
        assertEquals(
            AasJson.parseToJsonElement("""{"seq":5,"ts":1,"type":"thread/somethingNew","data":{"x":1}}"""),
            AasJson.encodeToJsonElement(EventEnvelope.serializer(), env),
        )
    }

    @Test
    fun unknownItemKindKeepsCommonFields() {
        val item = AasJson.decodeFromString(
            Item.serializer(),
            """{"kind":"hologram","id":"itm_1","threadId":"thr_1","turnId":"trn_1","status":"inProgress","startedAt":7,"beam":"x"}""",
        )
        val unknown = assertIs<Item.Unknown>(item)
        assertEquals("hologram", unknown.kind)
        assertEquals("itm_1", unknown.id)
        assertEquals(ItemStatus.InProgress, unknown.status)
        assertEquals(7, unknown.startedAt)
        assertNull(unknown.completedAt)
    }

    @Test
    fun unknownEnumValuesAndFieldsAreTolerated() {
        val turn = AasJson.decodeFromString(
            Turn.serializer(),
            """{"id":"trn_1","threadId":"thr_1","index":0,"status":"paused","startedAt":1,"brandNewField":true}""",
        )
        assertEquals(TurnStatus.Unknown, turn.status)
    }

    @Test
    fun additionsOfTheProtocolHaveSafeDefaults() {
        // An older server omits the fields added later within v1.
        val thread = AasJson.decodeFromString(
            Thread.serializer(),
            """{"id":"thr_1","projectId":"prj_1","harnessId":"fake","title":"t","cwd":"C:/p","workspace":{"kind":"local"},""" +
                """"status":"idle","createdAt":1,"updatedAt":1,"lastActivityAt":1}""",
        )
        assertEquals(false, thread.pinned)
        assertNull(thread.usage.context)
        val status = AasJson.decodeFromString(ServerStatusResult.serializer(), """{"uptimeMs":1,"runningProcesses":0,"runningTurns":0,"draining":false}""")
        assertEquals(false, status.preventSleepWhileRunning)
        val op = AasJson.decodeFromString(Operation.serializer(), """{"id":"op_1","kind":"gitClone","status":"cancelled","startedAt":1}""")
        assertEquals(OperationStatus.Cancelled, op.status)
        assertEquals(true, op.status.isTerminal)
        assertNull(op.progress)
        // A status added after this client counts as ended, never as running forever.
        val future = AasJson.decodeFromString(Operation.serializer(), """{"id":"op_2","kind":"gitClone","status":"paused","startedAt":1}""")
        assertEquals(OperationStatus.Unknown, future.status)
        assertEquals(true, future.status.isTerminal)
        val steer = AasJson.decodeFromString(QueueSteerResult.serializer(), "{}")
        assertNull(steer.disposition, "an entry that already left the queue")
    }

    @Test
    fun backgroundWorkAdditionsHaveSafeDefaults() {
        // An older server has no background work: nothing runs, nothing ended, nothing to stop.
        val thread = AasJson.decodeFromString(
            Thread.serializer(),
            """{"id":"thr_1","projectId":"prj_1","harnessId":"fake","title":"t","cwd":"C:/p","workspace":{"kind":"local"},""" +
                """"status":"ready","createdAt":1,"updatedAt":1,"lastActivityAt":1,"head":3}""",
        )
        assertEquals(ThreadBackground(running = 0, lastEnded = null), thread.background)
        val caps = AasJson.decodeFromString(HarnessCapabilities.serializer(), """{"interrupt":true}""")
        assertEquals(false, caps.backgroundTasks)
        assertEquals(false, caps.backgroundStop)
        val status = AasJson.decodeFromString(ServerStatusResult.serializer(), """{"uptimeMs":1,"runningProcesses":0,"runningTurns":0,"draining":false}""")
        assertEquals(0, status.runningBackgroundTasks)
        val read = AasJson.decodeFromString(
            ThreadReadResult.serializer(),
            """{"thread":${AasJson.encodeToString(Thread.serializer(), thread)},"turns":[],"items":[],"interactions":[],"queued":[],"head":3,"hasMoreBefore":false}""",
        )
        assertEquals(emptyList(), read.backgroundTasks)
        val turn = AasJson.decodeFromString(Turn.serializer(), """{"id":"trn_1","threadId":"thr_1","index":0,"status":"completed","startedAt":1}""")
        assertNull(turn.trigger)
        val item = AasJson.decodeFromString(
            Item.serializer(),
            """{"kind":"toolCall","id":"itm_1","threadId":"thr_1","turnId":"trn_1","status":"completed","startedAt":1,"category":"subagent","name":"Agent","title":"x"}""",
        )
        assertNull(item.backgroundTaskId)
    }

    @Test
    fun backgroundTaskValuesAddedLaterAreTolerated() {
        val task = AasJson.decodeFromString(
            BackgroundTask.serializer(),
            """{"id":"bgt_1","threadId":"thr_1","nativeId":"n","kind":"hologram","title":"x","status":"paused","ambient":false,"runs":1,""" +
                """"startedAt":1,"endReason":"meteor","stoppable":true,"progress":{"workflow":[{"label":"a","state":"thinking"}]}}""",
        )
        assertEquals(BackgroundTaskKind.Unknown, task.kind)
        assertEquals(BackgroundTaskStatus.Unknown, task.status)
        // A status added after this client counts as ended, never as running forever.
        assertEquals(true, task.status.isTerminal)
        assertEquals(false, BackgroundTaskStatus.Running.isTerminal)
        assertEquals(BackgroundEndReason.Unknown, task.endReason)
        assertEquals(WorkflowAgentState.Unknown, task.progress?.workflow?.single()?.state)
        val turn = AasJson.decodeFromString(Turn.serializer(), """{"id":"trn_1","threadId":"thr_1","index":0,"status":"completed","startedAt":1,"trigger":"cron"}""")
        assertEquals(TurnTrigger.Unknown, turn.trigger)
        // An item kind this client does not know still says which task it launched.
        val unknown = AasJson.decodeFromString(
            Item.serializer(),
            """{"kind":"hologram","id":"itm_1","threadId":"thr_1","turnId":"trn_1","status":"backgrounded","startedAt":7,"backgroundTaskId":"bgt_1"}""",
        )
        assertEquals(ItemStatus.Backgrounded, unknown.status)
        assertEquals("bgt_1", unknown.backgroundTaskId)
        val interaction = AasJson.decodeFromString(
            Interaction.serializer(),
            """{"id":"int_1","threadId":"thr_1","status":"expired","createdAt":1,"expireReason":"taskEnded","backgroundTaskId":"bgt_1",""" +
                """"request":{"kind":"question","title":"q","questions":[]}}""",
        )
        assertEquals(ExpireReason.TaskEnded, interaction.expireReason)
        assertEquals("bgt_1", interaction.backgroundTaskId)
        assertNull(interaction.turnId)
    }

    @Test
    fun unknownUnionTagsAreKept() {
        val action = AasJson.decodeFromString(CommandAction.serializer(), """{"type":"teleport","to":"mars"}""")
        val unknown = assertIs<CommandAction.Unknown>(action)
        assertEquals(JsonPrimitive("mars"), unknown.raw["to"])
        val subject = AasJson.decodeFromString(Subject.serializer(), """{"type":"network","host":"example.com"}""")
        assertIs<Subject.Unknown>(subject)
    }

    @Test
    fun deltasAppendToTheRightField() {
        val msg = Item.AgentMessage("itm", "thr", "trn", ItemStatus.InProgress, 0, null, "Hel")
        assertEquals("Hello", (msg.appendDelta(DeltaField.Text, "lo") as Item.AgentMessage).text)
        assertEquals(msg, msg.appendDelta(DeltaField.Output, "x"), "agent messages have no output field")
        val cmd = Item.CommandExecution("itm", "thr", "trn", ItemStatus.InProgress, 0, null, "ls", null, "a")
        assertEquals("ab", (cmd.appendDelta(DeltaField.Output, "b") as Item.CommandExecution).output)
        val unknown = Item.Unknown("x", JsonObject(mapOf("text" to JsonPrimitive("a"))))
        assertEquals(JsonPrimitive("ab"), (unknown.appendDelta(DeltaField.Text, "b") as Item.Unknown).raw["text"])
    }

    @Test
    fun errorKindsFallBackToCodes() {
        val err = RpcError(code = -32002, message = "gone")
        assertEquals(ErrorKind.NotFound, err.kind)
        assertEquals(ErrorKind.Unknown, RpcError(code = 123, message = "?").kind)
        // An unknown kind may be one the server stored for the request: it is not retried.
        assertEquals(true, ErrorKind.Unknown.definitive)
        // An unknown kind with a known code is that code's kind.
        val coded = AasJson.decodeFromString(RpcError.serializer(), """{"code":-32013,"message":"m","data":{"kind":"somethingNew"}}""")
        assertEquals(ErrorKind.Draining, coded.kind)
    }

    @Test
    fun shutdownReasonsIncludeTheStorageFailStop() {
        val notice = AasJson.decodeFromString(ServerShuttingDown.serializer(), """{"reason":"storageFailure","restartExpected":true}""")
        assertEquals(ShutdownReason.StorageFailure, notice.reason)
        assertEquals(
            AasJson.parseToJsonElement("""{"reason":"storageFailure","restartExpected":true}"""),
            AasJson.encodeToJsonElement(ServerShuttingDown.serializer(), notice),
        )
        assertEquals(ShutdownReason.Unknown, AasJson.decodeFromString(ServerShuttingDown.serializer(), """{"reason":"meteor","restartExpected":false}""").reason)
        // A plain stop (not under the watchdog, or `stop`/drain): the server may stay down.
        val stop = AasJson.decodeFromString(ServerShuttingDown.serializer(), """{"reason":"shutdown","restartExpected":false}""")
        assertEquals(ShutdownReason.Shutdown, stop.reason)
        assertEquals(false, stop.restartExpected)
    }

    @Test
    fun errorDetailsAreReadOnlyWhenTheyAreStrings() {
        val error = AasJson.decodeFromString(
            RpcError.serializer(),
            """{"code":-32005,"message":"m","data":{"kind":"harnessUnavailable","harnessId":7,"reason":"not logged in","future":{"x":1}}}""",
        )
        assertEquals(ErrorKind.HarnessUnavailable, error.kind)
        assertNull(error.harnessId, "a number is not a harness id")
        assertEquals("not logged in", error.reason)
        assertNull(RpcError(-32005, "m", JsonPrimitive("not an object")).reason)
    }

    @Test
    fun pairingLinksAndEndpoints() {
        val link = PairingLink.parse("aas://pair?u=wss%3A%2F%2Fpc.tail-x.ts.net%2Fv1%2Fws&c=ABCD-1234&n=home%20pc")!!
        assertEquals("wss://pc.tail-x.ts.net/v1/ws", link.wsUrl)
        assertEquals("ABCD-1234", link.code)
        assertEquals("home pc", link.serverName)
        assertNull(PairingLink.parse("https://example.com"))
        assertNull(PairingLink.parse("aas://pair?u=http%3A%2F%2Fx&c=1"))
        val ep = ServerEndpoints(link.wsUrl)
        assertEquals("https://pc.tail-x.ts.net", ep.httpBase)
        assertEquals("https://pc.tail-x.ts.net/v1/blobs/blb_x", ep.blob("blb_x"))
        assertEquals("http://127.0.0.1:7878/v1/pair", ServerEndpoints("ws://127.0.0.1:7878/v1/ws").pair)
    }
}
