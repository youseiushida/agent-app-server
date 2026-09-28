package dev.aas.android.domain.diff

import dev.aas.android.protocol.FileChangeKind

/*
 * Unified diffs as `git diff` writes them (the daemon's `thread/diff` patch) and as harnesses
 * report them per file (`FileChange.diff`, often only the hunks). The patch is split into files
 * so the diff viewer can show one file at a time (docs/ux/codex-desktop.md §8.2).
 */

/** One line of a hunk with its line numbers in the old and the new file. */
data class DiffLine(val kind: Kind, val text: String, val oldNumber: Int?, val newNumber: Int?) {
    enum class Kind { Context, Added, Removed, NoNewline }
}

/** A hunk: its `@@ -a,b +c,d @@ section` header and lines. */
data class DiffHunk(val oldStart: Int, val oldCount: Int, val newStart: Int, val newCount: Int, val section: String?, val lines: List<DiffLine>) {
    val header: String
        get() = "@@ -$oldStart,$oldCount +$newStart,$newCount @@" + (section?.let { " $it" } ?: "")
}

/** One file of a patch. [oldPath] is null for an added file, [newPath] for a deleted one. */
data class PatchFile(
    val oldPath: String?,
    val newPath: String?,
    val kind: FileChangeKind,
    val binary: Boolean,
    val hunks: List<DiffHunk>,
) {
    /** The path shown for the file (the new path, or the old one of a deleted file). */
    val path: String get() = newPath ?: oldPath ?: ""

    val added: Int get() = hunks.sumOf { h -> h.lines.count { it.kind == DiffLine.Kind.Added } }
    val removed: Int get() = hunks.sumOf { h -> h.lines.count { it.kind == DiffLine.Kind.Removed } }

    /** Rendered lines (hunk headers included), for size limits. */
    val lineCount: Int get() = hunks.sumOf { it.lines.size + 1 }
}

object UnifiedDiff {
    private val HUNK = Regex("^@@ -(\\d+)(?:,(\\d+))? \\+(\\d+)(?:,(\\d+))? @@ ?(.*)$")
    private const val DEV_NULL = "/dev/null"

    /** Splits a patch into its files. Text before the first file header is ignored. */
    fun parse(patch: String): List<PatchFile> {
        val lines = splitLines(patch)
        val files = ArrayList<PatchFile>()
        var i = 0
        while (i < lines.size) {
            val line = lines[i]
            when {
                line.startsWith("diff --git ") -> i = gitFile(lines, i, files)
                line.startsWith("--- ") && lines.getOrNull(i + 1)?.startsWith("+++ ") == true -> i = plainFile(lines, i, files)
                else -> i++
            }
        }
        return files
    }

    /**
     * The hunks of one file's diff as a harness reports it: either a complete patch of that file
     * or only its `@@` hunks.
     */
    fun parseHunks(diff: String): List<DiffHunk> {
        val files = parse(diff)
        if (files.isNotEmpty()) return files.flatMap { it.hunks }
        val lines = splitLines(diff)
        val hunks = ArrayList<DiffHunk>()
        var i = 0
        while (i < lines.size) {
            if (HUNK.matches(lines[i])) {
                val (hunk, next) = hunk(lines, i)
                hunks += hunk
                i = next
            } else {
                i++
            }
        }
        return hunks
    }

    private fun splitLines(text: String): List<String> {
        val lines = text.split('\n').map { it.removeSuffix("\r") }
        // A trailing newline does not start another line.
        return if (lines.isNotEmpty() && lines.last().isEmpty()) lines.dropLast(1) else lines
    }

    private fun gitFile(lines: List<String>, start: Int, out: MutableList<PatchFile>): Int {
        val (headerOld, headerNew) = gitHeaderPaths(lines[start].removePrefix("diff --git "))
        var oldPath: String? = headerOld
        var newPath: String? = headerNew
        var kind = FileChangeKind.Update
        var binary = false
        var i = start + 1
        // Extended header lines until the hunks, the next file or a binary marker.
        while (i < lines.size) {
            val line = lines[i]
            when {
                line.startsWith("diff --git ") -> break
                line.startsWith("new file mode") -> kind = FileChangeKind.Add
                line.startsWith("deleted file mode") -> kind = FileChangeKind.Delete
                line.startsWith("rename from ") -> {
                    oldPath = unquote(line.removePrefix("rename from "))
                    kind = FileChangeKind.Move
                }
                line.startsWith("rename to ") -> {
                    newPath = unquote(line.removePrefix("rename to "))
                    kind = FileChangeKind.Move
                }
                line.startsWith("Binary files ") || line == "GIT binary patch" -> binary = true
                line.startsWith("--- ") -> oldPath = side(line.removePrefix("--- "), "a/") ?: oldPath.takeIf { kind != FileChangeKind.Add }
                line.startsWith("+++ ") -> newPath = side(line.removePrefix("+++ "), "b/") ?: newPath.takeIf { kind != FileChangeKind.Delete }
                HUNK.matches(line) -> break
            }
            i++
        }
        val hunks = ArrayList<DiffHunk>()
        while (i < lines.size && !lines[i].startsWith("diff --git ")) {
            if (HUNK.matches(lines[i])) {
                val (hunk, next) = hunk(lines, i)
                hunks += hunk
                i = next
            } else {
                // Binary patch payload or anything else between hunks.
                i++
            }
        }
        if (kind == FileChangeKind.Add) oldPath = null
        if (kind == FileChangeKind.Delete) newPath = null
        out += PatchFile(oldPath, newPath, kind, binary, hunks)
        return i
    }

    private fun plainFile(lines: List<String>, start: Int, out: MutableList<PatchFile>): Int {
        val oldPath = side(lines[start].removePrefix("--- "), "a/")
        val newPath = side(lines[start + 1].removePrefix("+++ "), "b/")
        var i = start + 2
        val hunks = ArrayList<DiffHunk>()
        while (i < lines.size && HUNK.matches(lines[i])) {
            val (hunk, next) = hunk(lines, i)
            hunks += hunk
            i = next
        }
        val kind = when {
            oldPath == null -> FileChangeKind.Add
            newPath == null -> FileChangeKind.Delete
            oldPath != newPath -> FileChangeKind.Move
            else -> FileChangeKind.Update
        }
        out += PatchFile(oldPath, newPath, kind, binary = false, hunks = hunks)
        return i
    }

    /** Reads the hunk starting at [start]; the header's counts say where it ends. */
    private fun hunk(lines: List<String>, start: Int): Pair<DiffHunk, Int> {
        // Callers only start a hunk at a line the pattern matched.
        val m = HUNK.matchEntire(lines[start]) ?: error("not a hunk header: ${lines[start]}")
        val oldStart = m.groupValues[1].toInt()
        val oldCount = m.groups[2]?.value?.toInt() ?: 1
        val newStart = m.groupValues[3].toInt()
        val newCount = m.groups[4]?.value?.toInt() ?: 1
        val section = m.groupValues[5].takeIf { it.isNotBlank() }
        var oldLeft = oldCount
        var newLeft = newCount
        var oldNo = oldStart
        var newNo = newStart
        val out = ArrayList<DiffLine>()
        var i = start + 1
        while (i < lines.size) {
            val line = lines[i]
            if (line.startsWith("\\")) {
                // "\ No newline at end of file" belongs to the line before it.
                out += DiffLine(DiffLine.Kind.NoNewline, line.drop(1).trim(), null, null)
                i++
                continue
            }
            if (oldLeft <= 0 && newLeft <= 0) break
            val marker = line.firstOrNull()
            val text = if (line.isEmpty()) "" else line.substring(1)
            when (marker) {
                '+' -> {
                    if (newLeft <= 0) break
                    out += DiffLine(DiffLine.Kind.Added, text, null, newNo++)
                    newLeft--
                }
                '-' -> {
                    if (oldLeft <= 0) break
                    out += DiffLine(DiffLine.Kind.Removed, text, oldNo++, null)
                    oldLeft--
                }
                // Tools that strip trailing whitespace turn an empty context line into "".
                ' ', null -> {
                    if (oldLeft <= 0 || newLeft <= 0) break
                    out += DiffLine(DiffLine.Kind.Context, text, oldNo++, newNo++)
                    oldLeft--
                    newLeft--
                }
                else -> break
            }
            i++
        }
        return DiffHunk(oldStart, oldCount, newStart, newCount, section, out) to i
    }

    /** A `---` / `+++` path: `/dev/null` is none; the `a/` / `b/` prefix and a timestamp are dropped. */
    private fun side(raw: String, prefix: String): String? {
        val value = raw.substringBefore('\t').trimEnd()
        if (value == DEV_NULL) return null
        return unquote(value).removePrefix(prefix)
    }

    /** The two paths of `diff --git a/x b/y` (quoted when they contain special characters). */
    private fun gitHeaderPaths(rest: String): Pair<String?, String?> {
        if (rest.startsWith('"')) {
            val (first, after) = quoted(rest)
            val second = after.trimStart()
            val newPath = if (second.startsWith('"')) quoted(second).first else second
            return first.removePrefix("a/") to newPath.removePrefix("b/")
        }
        if (rest.endsWith('"')) {
            val open = rest.lastIndexOf(" \"")
            if (open >= 0) {
                return rest.substring(0, open).removePrefix("a/") to quoted(rest.substring(open + 1)).first.removePrefix("b/")
            }
        }
        // Unquoted: "a/<p> b/<p>"; the two halves are equal for anything but a rename, whose
        // real paths come from the "rename from/to" lines.
        val half = (rest.length - 1) / 2
        if (rest.length % 2 == 1 && rest[half] == ' ' && rest.substring(0, half).removePrefix("a/") == rest.substring(half + 1).removePrefix("b/")) {
            val path = rest.substring(0, half).removePrefix("a/")
            return path to path
        }
        val split = rest.indexOf(" b/")
        return if (split >= 0) rest.substring(0, split).removePrefix("a/") to rest.substring(split + 1).removePrefix("b/") else rest to rest
    }

    private fun unquote(value: String): String = if (value.startsWith('"')) quoted(value).first else value

    /**
     * A C-style quoted path as git writes it (`core.quotePath`): escapes and octal bytes, which
     * form UTF-8. Returns the path and the rest of the input after the closing quote.
     */
    private fun quoted(value: String): Pair<String, String> {
        val bytes = java.io.ByteArrayOutputStream()
        var i = 1
        while (i < value.length && value[i] != '"') {
            val c = value[i]
            if (c == '\\' && i + 1 < value.length) {
                val n = value[i + 1]
                when {
                    n in '0'..'7' -> {
                        var end = i + 1
                        while (end < value.length && end < i + 4 && value[end] in '0'..'7') end++
                        bytes.write(value.substring(i + 1, end).toInt(8))
                        i = end
                        continue
                    }
                    n == 'n' -> bytes.write('\n'.code)
                    n == 't' -> bytes.write('\t'.code)
                    n == 'r' -> bytes.write('\r'.code)
                    n == 'a' -> bytes.write(7)
                    n == 'b' -> bytes.write(8)
                    n == 'f' -> bytes.write(12)
                    n == 'v' -> bytes.write(11)
                    else -> bytes.write(n.toString().toByteArray(Charsets.UTF_8))
                }
                i += 2
                continue
            }
            bytes.write(c.toString().toByteArray(Charsets.UTF_8))
            i++
        }
        val rest = if (i < value.length) value.substring(i + 1) else ""
        return bytes.toString(Charsets.UTF_8.name()) to rest
    }
}
