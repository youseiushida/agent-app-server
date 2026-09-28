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

/** Which harnesses 「PC のセッションを取り込む」 and `/resume` offer, and which one they list first. */
class NativeSessionHarnessesTest {
    private val sessions = dev.aas.android.protocol.HarnessCapabilities(nativeSessions = true)
    private val codex = Samples.harness("codex", capabilities = sessions)
    private val claude = Samples.harness("claude", capabilities = sessions)
    private val acp = Samples.harness("acp")
    private val piDown = Samples.harness("pi", available = false, reason = "pi not found")
    private val all = listOf(acp, codex, claude, piDown)

    @Test
    fun onlyAvailableHarnessesWithTheCapabilityCanImport() {
        assertEquals(listOf("codex", "claude"), NativeSessionHarnesses.importable(all).map { it.id })
        kotlin.test.assertTrue(NativeSessionHarnesses.canImport(all))
        // An unavailable harness's capabilities are unknown: it does not make the import offered.
        kotlin.test.assertFalse(NativeSessionHarnesses.canImport(listOf(acp, piDown)))
        kotlin.test.assertFalse(NativeSessionHarnesses.canImport(emptyList()))
    }

    @Test
    fun theChoicesKeepAnUnavailableHarnessThatWasAskedFor() {
        fun choices(selected: String?, requested: String?, unable: Set<String> = emptySet()) =
            NativeSessionHarnesses.choices(all, selected, requested, unable).map { it.id }
        assertEquals(listOf("codex", "claude"), choices(selected = null, requested = null))
        assertEquals(listOf("codex", "claude", "pi"), choices(selected = null, requested = "pi"))
        assertEquals(listOf("codex", "claude", "pi"), choices(selected = "codex", requested = "pi"), "kept after switching away")
        assertEquals(listOf("codex", "claude", "pi"), choices(selected = "pi", requested = null))
        // A thread's harness that is available without the capability is not offered.
        assertEquals(listOf("codex", "claude"), choices(selected = "codex", requested = "acp"))
        // The server said it cannot list its sessions (the workspace still shows it unavailable).
        assertEquals(listOf("codex", "claude"), choices(selected = "codex", requested = "pi", unable = setOf("pi")))
    }

    /**
     * The selected harness stays among the choices whatever its state: an unavailable harness
     * whose probe (before `native/list`) shows it lacks the capability must not leave the screen
     * showing its failure with no chip to switch to the one harness that can list.
     */
    @Test
    fun theSelectedHarnessIsAlwaysAChoice() {
        assertEquals(listOf("acp", "codex", "claude"), NativeSessionHarnesses.choices(all, selected = "acp", requested = "acp").map { it.id })
        assertEquals(listOf("acp", "codex"), NativeSessionHarnesses.choices(listOf(acp, codex), selected = "acp", requested = "acp", unable = setOf("acp")).map { it.id })
    }

    @Test
    fun theThreadsHarnessThenTheProjectDefaultThenTheFirstImportable() {
        assertEquals("claude", NativeSessionHarnesses.preselect(all, requested = "claude", projectDefault = "codex"))
        assertEquals("codex", NativeSessionHarnesses.preselect(all, requested = null, projectDefault = "codex"))
        assertEquals("claude", NativeSessionHarnesses.preselect(all, requested = null, projectDefault = "claude"))
        assertEquals("codex", NativeSessionHarnesses.preselect(all, requested = null, projectDefault = null))
        // A thread of a harness that cannot list sessions: the project default, else the first.
        assertEquals("claude", NativeSessionHarnesses.preselect(all, requested = "acp", projectDefault = "claude"))
        assertEquals("codex", NativeSessionHarnesses.preselect(all, requested = "acp", projectDefault = "acp"))
        assertEquals("codex", NativeSessionHarnesses.preselect(all, requested = "gone", projectDefault = "gone"))
        // A thread of an unavailable harness: listed first, so the screen says why in place.
        assertEquals("pi", NativeSessionHarnesses.preselect(all, requested = "pi", projectDefault = "codex"))
        assertNull(NativeSessionHarnesses.preselect(listOf(acp), requested = "acp", projectDefault = null))
    }

    @Test
    fun aHarnessTheServerSaidCannotListIsNeverPickedAgain() {
        // Unavailable in the workspace, but the server answered capabilityUnsupported.
        assertEquals("claude", NativeSessionHarnesses.preselect(all, requested = "pi", projectDefault = "claude", unable = setOf("pi")))
        assertEquals("codex", NativeSessionHarnesses.preselect(all, requested = "pi", projectDefault = "pi", unable = setOf("pi")))
        assertNull(NativeSessionHarnesses.preselect(listOf(acp, piDown), requested = "pi", projectDefault = null, unable = setOf("pi")))
        // Importable in the workspace (a stale entry, or a newer CLI): still not picked by itself,
        // so two refusals never make the screen list back and forth...
        assertEquals("claude", NativeSessionHarnesses.preselect(all, requested = "codex", projectDefault = null, unable = setOf("codex")))
        assertNull(NativeSessionHarnesses.preselect(all, requested = "codex", projectDefault = "claude", unable = setOf("codex", "claude")))
        // ...but it stays a choice the user can pick.
        assertEquals(listOf("codex", "claude"), NativeSessionHarnesses.choices(all, selected = "claude", requested = "codex", unable = setOf("codex")).map { it.id })
    }
}
