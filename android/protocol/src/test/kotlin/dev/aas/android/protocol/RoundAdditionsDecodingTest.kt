package dev.aas.android.protocol

import kotlinx.serialization.json.jsonObject
import kotlin.test.Test
import kotlin.test.assertEquals
import kotlin.test.assertFalse
import kotlin.test.assertIs
import kotlin.test.assertNull

/**
 * The additions of background output, per-model permission modes and another thread's workspace
 * (protocol.md §3.1, §4 `thread/create`): an older server leaves them out, and this client
 * writes them the way the server does.
 */
class RoundAdditionsDecodingTest {
    @Test
    fun anOlderServersTaskHasNoOutputAndItsModelsAllowEveryMode() {
        val task = AasJson.decodeFromString(
            BackgroundTask.serializer(),
            """{"id":"bgt_1","threadId":"thr_1","nativeId":"n","kind":"shell","title":"x","status":"running","startedAt":1,"stoppable":true,""" +
                """"result":{"outputTruncated":false}}""",
        )
        assertNull(task.output)
        assertFalse(task.outputTruncated)
        assertNull(task.result?.outputOmittedBytes)
        val json = AasJson.encodeToJsonElement(BackgroundTask.serializer(), task).jsonObject
        assertFalse("output" in json || "outputTruncated" in json, "absent fields stay absent: $json")
        val model = AasJson.decodeFromString(Model.serializer(), """{"id":"haiku","displayName":"Haiku"}""")
        assertNull(model.permissionModes)
    }

    @Test
    fun theOutputDeltaNamesItsTask() {
        val env = AasJson.decodeFromString(
            EventEnvelope.serializer(),
            """{"seq":12,"seqFrom":9,"ts":1,"type":"backgroundTask/outputDelta","data":{"taskId":"bgt_1","text":"TICK 1\r\nTICK 2\r\n"}}""",
        )
        assertEquals(Event.BackgroundTaskOutputDelta("bgt_1", "TICK 1\r\nTICK 2\r\n"), env.event)
        assertEquals(9L, env.seqFrom)
        assertEquals("backgroundTask/outputDelta", Event.typeOf(env.event))
    }

    @Test
    fun anotherThreadsWorkspaceIsWrittenLikeTheServerReadsIt() {
        val spec: WorkspaceSpec = WorkspaceSpec.Thread("thr_1")
        val json = AasJson.encodeToJsonElement(WorkspaceSpec.serializer(), spec)
        assertEquals(AasJson.parseToJsonElement("""{"threadId":"thr_1","kind":"thread"}"""), json)
        assertIs<WorkspaceSpec.Thread>(AasJson.decodeFromJsonElement(WorkspaceSpec.serializer(), json))
    }
}
