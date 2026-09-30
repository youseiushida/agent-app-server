package dev.aas.android.domain

import dev.aas.android.protocol.BackgroundTask
import dev.aas.android.protocol.BackgroundTaskStatus
import dev.aas.android.protocol.BlobId

/**
 * What a background task's output view shows (docs/android.md 30), from the task's explicit
 * fields only (protocol.md §3.1):
 *
 * * While the harness streams it (`BackgroundTask.output`, grown by `backgroundTask/outputDelta`):
 *   the current run's output so far. It stops growing at the daemon's inline limit
 *   (`outputTruncated`); the rest comes with the end.
 * * Once the harness reported the whole output (`result.output`, and the blob with all of it when
 *   longer than the inline limit): that output, which replaces the streamed copy.
 * * A task that ended without reporting its output (the process ended with it) keeps what was
 *   streamed.
 */
data class TaskOutput(
    /** The output as far as it is known here (the beginning, when a blob or the limit cut it). */
    val text: String,
    /** The run still streams into [text]: the view follows its end. */
    val live: Boolean,
    /** The whole output (the harness's final report was longer than the daemon's inline limit). */
    val blobId: BlobId?,
    /** Streaming reached the daemon's inline limit: nothing more arrives before the end. */
    val streamLimitReached: Boolean,
    /** [text] is only the beginning and there is no blob with the rest. */
    val cutWithoutBlob: Boolean,
    /** Bytes at the start of the harness's output file the daemon did not read (it was too large). */
    val omittedBytes: Long?,
) {
    /** Only the beginning is inline: the whole output needs [blobId]. */
    val partial: Boolean get() = blobId != null || cutWithoutBlob || streamLimitReached

    companion object {
        /** The output of [task], `null` when it has none (an agent's summary is not output). */
        fun of(task: BackgroundTask): TaskOutput? {
            val result = task.result
            if (result != null && (result.output != null || result.outputBlobId != null)) {
                return TaskOutput(
                    text = result.output.orEmpty(),
                    live = false,
                    blobId = result.outputBlobId,
                    streamLimitReached = false,
                    cutWithoutBlob = result.outputTruncated && result.outputBlobId == null,
                    omittedBytes = result.outputOmittedBytes?.takeIf { it > 0 },
                )
            }
            val streamed = task.output ?: return null
            return TaskOutput(
                text = streamed,
                live = task.status == BackgroundTaskStatus.Running,
                blobId = null,
                streamLimitReached = task.outputTruncated,
                cutWithoutBlob = false,
                omittedBytes = null,
            )
        }

        /**
         * The lines of an output: a final line break ends the last line (it adds no empty one),
         * and Windows line ends (`\r\n`, which console programs on the PC print) count as one.
         */
        fun lines(text: String): List<String> =
            text.trimEnd('\n', '\r').let { if (it.isEmpty()) emptyList() else it.split('\n').map { line -> line.removeSuffix("\r") } }

        /**
         * The last [count] lines of [text] (as [lines] would end them), and whether there are more.
         * Reads from the end only: a card shows the newest lines of an output that grows many
         * times a second.
         */
        fun lastLines(text: String, count: Int): Excerpt {
            val body = text.trimEnd('\n', '\r')
            if (body.isEmpty() || count <= 0) return Excerpt(emptyList(), more = body.isNotEmpty())
            var start = body.length
            var taken = 0
            while (taken < count) {
                // The line before `start` begins after the line break before it (the text's last
                // line ends at its end; any other at the line break just before `start`).
                val searchFrom = if (start == body.length) body.length - 1 else start - 2
                val lineBreak = if (searchFrom < 0) -1 else body.lastIndexOf('\n', searchFrom)
                start = lineBreak + 1
                taken++
                if (lineBreak < 0) break
            }
            return Excerpt(split(body.substring(start)), more = start > 0)
        }

        /** The first [count] lines of [text] (as [lines] would begin them), and whether there are more. Reads from the start only. */
        fun firstLines(text: String, count: Int): Excerpt {
            val body = text.trimEnd('\n', '\r')
            if (body.isEmpty() || count <= 0) return Excerpt(emptyList(), more = body.isNotEmpty())
            var end = 0
            var taken = 0
            while (taken < count) {
                val lineBreak = body.indexOf('\n', end)
                taken++
                if (lineBreak < 0) {
                    end = body.length
                    break
                }
                end = lineBreak + 1
            }
            // Without the line break that ends the last line taken (an empty last line stays one).
            val excerpt = if (end == body.length) body else body.substring(0, end - 1)
            return Excerpt(split(excerpt), more = end < body.length)
        }

        /** Lines of a text that has no final line break (every piece, empty ones too). */
        private fun split(text: String): List<String> = text.split('\n').map { it.removeSuffix("\r") }
    }

    /** Some lines of an output, and whether the output has more lines than these. */
    data class Excerpt(val lines: List<String>, val more: Boolean)
}
