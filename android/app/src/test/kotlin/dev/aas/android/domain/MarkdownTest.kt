package dev.aas.android.domain

import dev.aas.android.domain.markdown.Markdown
import dev.aas.android.domain.markdown.MdAlign
import dev.aas.android.domain.markdown.MdBlock
import dev.aas.android.domain.markdown.MdInline
import dev.aas.android.domain.markdown.plainText
import org.junit.Test
import kotlin.test.assertEquals
import kotlin.test.assertIs
import kotlin.test.assertTrue

class MarkdownTest {
    private fun text(s: String) = MdInline.Text(s)

    @Test
    fun headingsParagraphsAndBreaks() {
        val blocks = Markdown.parse("# Title #\n\nFirst line  \nsecond line\nthird\n\nSub\n---\n")
        assertEquals(MdBlock.Heading(1, listOf(text("Title"))), blocks[0])
        assertEquals(
            MdBlock.Paragraph(listOf(text("First line"), MdInline.LineBreak, text("second line"), MdInline.SoftBreak, text("third"))),
            blocks[1],
        )
        // A paragraph followed by "---" is a setext heading, not a rule.
        assertEquals(MdBlock.Heading(2, listOf(text("Sub"))), blocks[2])
        assertEquals(3, blocks.size)
        // "#tag" is not a heading.
        assertIs<MdBlock.Paragraph>(Markdown.parse("#tag").single())
    }

    @Test
    fun emphasisStrongStrikeAndCode() {
        val inlines = Markdown.parseInline("a **bold *both* x** and _it_ ~~gone~~ `co*de*` snake_case_name")
        assertEquals(
            listOf(
                text("a "),
                MdInline.Strong(listOf(text("bold "), MdInline.Emphasis(listOf(text("both"))), text(" x"))),
                text(" and "),
                MdInline.Emphasis(listOf(text("it"))),
                text(" "),
                MdInline.Strike(listOf(text("gone"))),
                text(" "),
                MdInline.Code("co*de*"),
                text(" snake_case_name"),
            ),
            inlines,
        )
        // Unclosed delimiters stay literal.
        assertEquals(listOf(text("2 * 3 and **open")), Markdown.parseInline("2 * 3 and **open"))
        assertEquals(listOf(text("``not code")), Markdown.parseInline("``not code"))
        assertEquals(listOf(MdInline.Code("a ` b")), Markdown.parseInline("`` a ` b ``"))
    }

    @Test
    fun linksAutolinksAndEscapes() {
        assertEquals(
            listOf(text("see "), MdInline.Link("https://x.dev/a_(b)", listOf(text("docs"))), text(".")),
            Markdown.parseInline("see [docs](https://x.dev/a_(b) \"title\")."),
        )
        assertEquals(
            listOf(text("go "), MdInline.Link("https://example.com/path", listOf(text("https://example.com/path"))), text(", now")),
            Markdown.parseInline("go https://example.com/path, now"),
        )
        assertEquals(listOf(MdInline.Link("http://www.a.io", listOf(text("www.a.io")))), Markdown.parseInline("www.a.io"))
        assertEquals(listOf(MdInline.Link("mailto:me@a.io", listOf(text("me@a.io")))), Markdown.parseInline("<me@a.io>"))
        assertEquals(listOf(text("*not* [x]")), Markdown.parseInline("\\*not\\* \\[x]"))
        // Images are shown as links with their alt text; unmatched brackets stay literal.
        assertEquals(listOf(MdInline.Link("a.png", listOf(text("pic")))), Markdown.parseInline("![pic](a.png)"))
        assertEquals(listOf(text("[a] b]")), Markdown.parseInline("[a] b]"))
    }

    @Test
    fun codeBlocksFencedIndentedAndUnterminated() {
        val blocks = Markdown.parse("```kotlin title\nval x = 1\n\n  y()\n```\n\n    indented\n    code\n\n~~~\nopen")
        assertEquals(MdBlock.Code("kotlin", "val x = 1\n\n  y()"), blocks[0])
        assertEquals(MdBlock.Code(null, "indented\ncode"), blocks[1])
        // A fence that is still streaming runs to the end.
        assertEquals(MdBlock.Code(null, "open"), blocks[2])
    }

    @Test
    fun nestedListsTasksAndLooseness() {
        val blocks = Markdown.parse("- a\n  - b\n  - c\n- [x] done\n- [ ] todo\n\n3. three\n4. four\n")
        val bullets = assertIs<MdBlock.ListBlock>(blocks[0])
        assertEquals(false, bullets.ordered)
        assertEquals(3, bullets.items.size)
        assertTrue(bullets.tight)
        val nested = assertIs<MdBlock.ListBlock>(bullets.items[0].blocks[1])
        assertEquals(listOf("b", "c"), nested.items.map { (it.blocks.single() as MdBlock.Paragraph).content.plainText() })
        assertEquals(true, bullets.items[1].checked)
        assertEquals(false, bullets.items[2].checked)
        val ordered = assertIs<MdBlock.ListBlock>(blocks[1])
        assertEquals(true, ordered.ordered)
        assertEquals(3, ordered.start)
        assertEquals(2, ordered.items.size)

        val loose = assertIs<MdBlock.ListBlock>(Markdown.parse("- one\n\n- two\n").single())
        assertEquals(false, loose.tight)
        assertEquals(2, loose.items.size)
        // Lazy continuation lines belong to the item.
        val lazy = assertIs<MdBlock.ListBlock>(Markdown.parse("- first\ncontinued\n").single())
        assertEquals("first\ncontinued", (lazy.items.single().blocks.single() as MdBlock.Paragraph).content.plainText())
    }

    @Test
    fun rulesQuotesAndTables() {
        val blocks = Markdown.parse("***\n> quoted\nlazy\n> > deeper\n\n| a | b:c | r |\n|:--|:-:|--:|\n| 1 | x\\|y | `z` |\n| 2 |\n\nafter")
        assertEquals(MdBlock.Rule, blocks[0])
        val quote = assertIs<MdBlock.Quote>(blocks[1])
        assertEquals("quoted\nlazy", (quote.blocks[0] as MdBlock.Paragraph).content.plainText())
        assertIs<MdBlock.Quote>(quote.blocks[1])
        val table = assertIs<MdBlock.Table>(blocks[2])
        assertEquals(listOf(MdAlign.Start, MdAlign.Center, MdAlign.End), table.alignments)
        assertEquals(listOf("a", "b:c", "r"), table.header.map { it.plainText() })
        assertEquals(listOf("1", "x|y", "z"), table.rows[0].map { it.plainText() })
        // Short rows are padded to the header's width.
        assertEquals(listOf("2", "", ""), table.rows[1].map { it.plainText() })
        assertEquals(MdBlock.Paragraph(listOf(text("after"))), blocks[3])
        // "- - -" is a rule, not a list.
        assertEquals(MdBlock.Rule, Markdown.parse("- - -").single())
    }

    @Test
    fun rawHtmlStaysTextAndCrLfIsNormalised() {
        assertEquals(listOf(text("<b>x</b>")), (Markdown.parse("<b>x</b>").single() as MdBlock.Paragraph).content)
        assertEquals(Markdown.parse("a\nb"), Markdown.parse("a\r\nb"))
    }
}
