package dev.aas.android.domain

import dev.aas.android.AppPolicy
import dev.aas.android.DisplayPolicy
import dev.aas.android.sync.OutboxEntry
import dev.aas.android.sync.Samples
import kotlinx.serialization.json.JsonObject
import kotlinx.serialization.json.JsonPrimitive
import org.junit.Test
import kotlin.test.assertEquals
import kotlin.test.assertFailsWith
import kotlin.test.assertNull

/** The words for a harness's state, and requests waiting for their harness. */
class HarnessesTest {
    private val codex = Samples.harness("codex").copy(displayName = "Codex")

    @Test
    fun theStateFollowsTheServerAndThisAppsOwnProbe() {
        assertEquals(HarnessState.Available, HarnessState.of(codex, emptySet()))
        val down = codex.copy(available = false, unavailableReason = "not logged in")
        assertEquals(HarnessState.Unavailable("not logged in"), HarnessState.of(down, emptySet()))
        assertEquals(HarnessState.Unavailable(null), HarnessState.of(down.copy(unavailableReason = null), emptySet()))
        // Probing wins while this app's refresh is in flight, whatever the last answer was.
        assertEquals(HarnessState.Probing, HarnessState.of(down, setOf("codex")))
        assertEquals(HarnessState.Probing, HarnessState.of(codex, setOf("codex")))
    }

    @Test
    fun aWaitNamesTheHarnessAndTheFreshestReason() {
        val entry = OutboxEntry(
            "c1", "turn/start", JsonObject(mapOf("clientRequestId" to JsonPrimitive("c1"))), 1,
            failures = 1, lastError = "not logged in", waitingForHarness = "codex",
        )
        // The workspace's reason when it lists the harness as unavailable (it may be newer).
        val down = codex.copy(available = false, unavailableReason = "codex not found on PATH")
        assertEquals(HarnessWait("c1", "turn/start", "codex", "Codex", "codex not found on PATH"), HarnessWait.of(entry, listOf(down)))
        // Otherwise the refusal's reason; an unknown harness is named by its id.
        assertEquals(HarnessWait("c1", "turn/start", "codex", "Codex", "not logged in"), HarnessWait.of(entry, listOf(codex)))
        assertEquals(HarnessWait("c1", "turn/start", "codex", "codex", "not logged in"), HarnessWait.of(entry, emptyList()))
        assertNull(HarnessWait.of(entry.copy(waitingForHarness = null), listOf(down)))
        assertEquals(listOf("c1"), HarnessWait.all(listOf(entry, entry.copy(clientRequestId = "c2", waitingForHarness = null)), listOf(down)).map { it.clientRequestId })
    }

    @Test
    fun displayLimitsMustBePositive() {
        assertEquals(DisplayPolicy(), AppPolicy().display)
        assertFailsWith<IllegalArgumentException> { DisplayPolicy(workingTickMs = 0) }
        assertFailsWith<IllegalArgumentException> { DisplayPolicy(relativeTimeRefreshMs = 0) }
        assertFailsWith<IllegalArgumentException> { DisplayPolicy(outputExpandedLines = 0) }
        assertFailsWith<IllegalArgumentException> { DisplayPolicy(previewFiles = 0) }
    }
}
