package dev.aas.android.data

import dev.aas.android.protocol.NativeSession
import dev.aas.android.sync.Samples
import dev.aas.android.sync.SyncLogger
import org.junit.Test
import kotlin.test.assertEquals
import kotlin.test.assertSame
import kotlin.test.assertTrue

/** Server lists made unique by the ids the screens key their rows by. */
class ServerListsTest {
    private val warnings = mutableListOf<Pair<SyncLogger.Level, String>>()
    private val lists = ServerLists { level, message, _ -> warnings += level to message }
    private val latest: Comparator<NativeSession> = compareBy(nullsFirst()) { it.updatedAt }

    @Test
    fun aListWithoutDuplicatesIsReturnedAsItIsAndNothingIsLogged() {
        val sessions = listOf(NativeSession("a", updatedAt = 1), NativeSession("b", updatedAt = 2))
        assertSame(sessions, lists.unique(sessions, "native/list", NativeSession::nativeSessionId, latest))
        assertTrue(warnings.isEmpty())
    }

    /** Codex's `thread/list` returns a thread once per rollout of a thread resumed elsewhere. */
    @Test
    fun theLatestCopyOfASessionIsKeptAtItsFirstPositionWithAWarning() {
        val sessions = listOf(
            NativeSession("019a", title = "Fix the build", updatedAt = 100),
            NativeSession("019b", title = "Other", updatedAt = 50),
            NativeSession("019a", title = "Fix the build", updatedAt = 300),
            NativeSession("019a", title = "Fix the build", updatedAt = 200),
            NativeSession("019c", title = "No time", updatedAt = null),
            NativeSession("019c", title = "No time", updatedAt = 10),
        )
        val unique = lists.unique(sessions, "native/list of codex", NativeSession::nativeSessionId, latest)
        assertEquals(listOf("019a" to 300L, "019b" to 50L, "019c" to 10L), unique.map { it.nativeSessionId to it.updatedAt })
        val (level, message) = warnings.single()
        assertEquals(SyncLogger.Level.Warn, level)
        assertTrue("native/list of codex" in message && "019a" in message && "019c" in message && "3 dropped" in message, message)
    }

    @Test
    fun withoutAPreferenceTheFirstCopyIsKeptAndEqualCopiesKeepTheFirst() {
        val threads = listOf(Samples.thread("thr_1", title = "first"), Samples.thread("thr_2"), Samples.thread("thr_1", title = "second"))
        assertEquals(listOf("first", "t"), lists.unique(threads, "thread/list", { it.id }).map { it.title })
        val sameTime = listOf(NativeSession("a", title = "one", updatedAt = 5), NativeSession("a", title = "two", updatedAt = 5))
        assertEquals("one", lists.unique(sameTime, "native/list", NativeSession::nativeSessionId, latest).single().title)
    }

    @Test
    fun aLongListOfRepeatedIdsIsSummarisedInTheWarning() {
        val ids = (1..8).map { "s$it" }
        val sessions = ids.flatMap { listOf(NativeSession(it), NativeSession(it)) }
        assertEquals(ids, lists.unique(sessions, "native/list", NativeSession::nativeSessionId).map { it.nativeSessionId })
        val message = warnings.single().second
        assertTrue("s1, s2, s3, s4, s5 and 3 more" in message, message)
    }
}
