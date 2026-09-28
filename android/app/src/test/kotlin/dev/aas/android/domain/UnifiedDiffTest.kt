package dev.aas.android.domain

import dev.aas.android.domain.diff.DiffLine
import dev.aas.android.domain.diff.UnifiedDiff
import dev.aas.android.protocol.FileChangeKind
import dev.aas.android.protocol.Item
import dev.aas.android.testing.Fixtures
import dev.aas.android.testing.item
import org.junit.Test
import kotlin.test.assertEquals
import kotlin.test.assertNull
import kotlin.test.assertTrue

class UnifiedDiffTest {
    private val patch = """
        diff --git a/src/main.rs b/src/main.rs
        index 1111111..2222222 100644
        --- a/src/main.rs
        +++ b/src/main.rs
        @@ -1,4 +1,5 @@ fn main() {
         line one
        -old two
        +new two
        +added three
         line four
        --- not a header
        @@ -10,2 +11,2 @@
         ten
        -eleven
        +eleven!
        \ No newline at end of file
        diff --git a/new.txt b/new.txt
        new file mode 100644
        index 0000000..3333333
        --- /dev/null
        +++ b/new.txt
        @@ -0,0 +1,2 @@
        +hello
        +world
        diff --git a/gone.txt b/gone.txt
        deleted file mode 100644
        --- a/gone.txt
        +++ /dev/null
        @@ -1 +0,0 @@
        -bye
        diff --git a/old name.txt b/new name.txt
        similarity index 90%
        rename from old name.txt
        rename to new name.txt
        diff --git a/logo.png b/logo.png
        index 4444444..5555555 100644
        Binary files a/logo.png and b/logo.png differ
        diff --git "a/\346\227\245\346\234\254.md" "b/\346\227\245\346\234\254.md"
        --- "a/\346\227\245\346\234\254.md"
        +++ "b/\346\227\245\346\234\254.md"
        @@ -1 +1 @@
        -a
        +b
    """.trimIndent() + "\n"

    @Test
    fun aGitPatchSplitsIntoFilesWithKindsAndLineNumbers() {
        val files = UnifiedDiff.parse(patch)
        assertEquals(listOf("src/main.rs", "new.txt", "gone.txt", "new name.txt", "logo.png", "日本.md"), files.map { it.path })
        assertEquals(listOf(FileChangeKind.Update, FileChangeKind.Add, FileChangeKind.Delete, FileChangeKind.Move, FileChangeKind.Update, FileChangeKind.Update), files.map { it.kind })

        val main = files[0]
        assertEquals(2, main.hunks.size)
        assertEquals("fn main() {", main.hunks[0].section)
        // A removed line that looks like a file header stays in its hunk (the counts decide).
        val first = main.hunks[0].lines
        assertEquals(
            listOf(
                DiffLine(DiffLine.Kind.Context, "line one", 1, 1),
                DiffLine(DiffLine.Kind.Removed, "old two", 2, null),
                DiffLine(DiffLine.Kind.Added, "new two", null, 2),
                DiffLine(DiffLine.Kind.Added, "added three", null, 3),
                DiffLine(DiffLine.Kind.Context, "line four", 3, 4),
                DiffLine(DiffLine.Kind.Removed, "-- not a header", 4, null),
            ),
            first,
        )
        val second = main.hunks[1].lines
        assertEquals(DiffLine(DiffLine.Kind.Context, "ten", 10, 11), second[0])
        assertEquals(DiffLine(DiffLine.Kind.Added, "eleven!", null, 12), second[2])
        assertEquals(DiffLine.Kind.NoNewline, second[3].kind)
        assertEquals(3, main.added)
        assertEquals(3, main.removed)

        assertNull(files[1].oldPath)
        assertEquals(listOf("hello", "world"), files[1].hunks.single().lines.map { it.text })
        assertNull(files[2].newPath)
        assertEquals("old name.txt", files[3].oldPath)
        assertTrue(files[3].hunks.isEmpty())
        assertTrue(files[4].binary)
        assertEquals(1, files[5].added)
    }

    @Test
    fun hunksWithoutHeadersAndPlainUnifiedDiffs() {
        // The fixture's file change carries only a hunk (as harnesses report it).
        val change = Fixtures.threadRead.item<Item.FileChangeItem>().changes.single()
        val hunks = UnifiedDiff.parseHunks(change.diff!!)
        assertEquals(1, hunks.size)
        assertEquals(listOf(DiffLine.Kind.Removed, DiffLine.Kind.Added), hunks.single().lines.map { it.kind })
        assertEquals("let t = 2;", hunks.single().lines[1].text)

        val plain = UnifiedDiff.parse("--- a/x.txt\t2026-01-01\n+++ b/y.txt\n@@ -1 +1 @@\n-x\n+y\n")
        assertEquals("y.txt", plain.single().path)
        assertEquals("x.txt", plain.single().oldPath)
        assertEquals(FileChangeKind.Move, plain.single().kind)
        // Text before the first file and CRLF line endings.
        val crlf = UnifiedDiff.parse("preamble\r\ndiff --git a/a b/a\r\n--- a/a\r\n+++ b/a\r\n@@ -1 +1 @@\r\n-1\r\n+2\r\n")
        assertEquals(listOf("1", "2"), crlf.single().hunks.single().lines.map { it.text })
    }

    @Test
    fun emptyAndGarbageInputHaveNoFiles() {
        assertTrue(UnifiedDiff.parse("").isEmpty())
        assertTrue(UnifiedDiff.parse("not a diff\n").isEmpty())
        assertTrue(UnifiedDiff.parseHunks("nothing").isEmpty())
    }
}
