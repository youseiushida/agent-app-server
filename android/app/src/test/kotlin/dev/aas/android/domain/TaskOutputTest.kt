package dev.aas.android.domain

import dev.aas.android.protocol.BackgroundResult
import dev.aas.android.protocol.BackgroundTaskKind
import dev.aas.android.protocol.BackgroundTaskStatus
import dev.aas.android.sync.Samples
import org.junit.Test
import kotlin.test.assertEquals
import kotlin.test.assertNull

/**
 * What a background task's output view shows (docs/android.md 30): the streamed output while it
 * runs, the reported one after, from the task's explicit fields only; and the excerpts a card
 * shows, read from one end of an output that grows many times a second.
 */
class TaskOutputTest {
    private val shell = Samples.backgroundTask("bgt_1", kind = BackgroundTaskKind.Shell)

    @Test
    fun theStreamedOutputWhileItRunsTheReportedOneAfter() {
        assertNull(TaskOutput.of(shell), "nothing streamed, nothing reported")
        val streaming = shell.copy(output = "a\nb\n")
        assertEquals(TaskOutput("a\nb\n", live = true, blobId = null, streamLimitReached = false, cutWithoutBlob = false, omittedBytes = null), TaskOutput.of(streaming))
        val limited = TaskOutput.of(streaming.copy(outputTruncated = true))!!
        assertEquals(true, limited.streamLimitReached)
        assertEquals(true, limited.partial)
        // Ended without its output reported (the process went with it): what was streamed, no longer live.
        val lost = TaskOutput.of(streaming.copy(status = BackgroundTaskStatus.Lost))!!
        assertEquals("a\nb\n", lost.text)
        assertEquals(false, lost.live)
        // The harness's whole output replaces the streamed one.
        val reported = streaming.copy(
            status = BackgroundTaskStatus.Completed,
            output = null,
            result = BackgroundResult(output = "start of it", outputTruncated = true, outputBlobId = "blb_1", outputOmittedBytes = 9),
        )
        assertEquals(TaskOutput("start of it", live = false, blobId = "blb_1", streamLimitReached = false, cutWithoutBlob = false, omittedBytes = 9), TaskOutput.of(reported))
        val cut = TaskOutput.of(reported.copy(result = BackgroundResult(output = "x", outputTruncated = true)))!!
        assertEquals(true, cut.cutWithoutBlob)
        // An agent's summary is not output.
        assertNull(TaskOutput.of(shell.copy(kind = BackgroundTaskKind.Agent, result = BackgroundResult(summary = "done"))))
    }

    @Test
    fun linesEndWithTheTextAndWindowsLineEndsCountAsOne() {
        assertEquals(listOf("a", "", "b"), TaskOutput.lines("a\r\n\r\nb\r\n"))
        assertEquals(emptyList(), TaskOutput.lines("\n"))
        assertEquals(emptyList(), TaskOutput.lines(""))
    }

    @Test
    fun excerptsAreTheLinesAtEitherEnd() {
        val text = "1\n2\n\n4\r\n5\n"
        assertEquals(TaskOutput.Excerpt(listOf("", "4", "5"), more = true), TaskOutput.lastLines(text, 3))
        assertEquals(TaskOutput.Excerpt(listOf("1", "2", ""), more = true), TaskOutput.firstLines(text, 3))
        assertEquals(TaskOutput.Excerpt(TaskOutput.lines(text), more = false), TaskOutput.lastLines(text, 5))
        assertEquals(TaskOutput.Excerpt(TaskOutput.lines(text), more = false), TaskOutput.firstLines(text, 9))
        assertEquals(TaskOutput.Excerpt(listOf("only"), more = false), TaskOutput.lastLines("only", 6))
        assertEquals(TaskOutput.Excerpt(emptyList(), more = false), TaskOutput.lastLines("\n\n", 6))
        // Each excerpt equals the same end of all the lines, for every count.
        val many = (1..40).joinToString("") { if (it % 7 == 0) "\n" else "line $it\n" }
        val all = TaskOutput.lines(many)
        for (count in 1..45) {
            assertEquals(TaskOutput.Excerpt(all.takeLast(count), more = count < all.size), TaskOutput.lastLines(many, count), "last $count")
            assertEquals(TaskOutput.Excerpt(all.take(count), more = count < all.size), TaskOutput.firstLines(many, count), "first $count")
        }
    }
}
