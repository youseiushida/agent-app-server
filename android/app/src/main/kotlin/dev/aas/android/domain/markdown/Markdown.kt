package dev.aas.android.domain.markdown

/*
 * A Markdown parser for agent messages (docs/ux/codex-desktop.md §3.1: answers are shown as
 * Markdown with code blocks, tables and links). It covers the CommonMark blocks agents write
 * (ATX and setext headings, paragraphs, fenced and indented code, block quotes, nested bullet
 * and ordered lists, thematic breaks) plus the GitHub extensions they rely on (pipe tables,
 * task list items, strikethrough, bare URLs). Raw HTML is shown as text: the app never renders
 * markup from the agent.
 *
 * The parser is total: any input produces blocks (unmatched syntax stays literal text), so a
 * message that is still streaming renders at every step.
 */

/** A block of a Markdown document. */
sealed interface MdBlock {
    data class Heading(val level: Int, val content: List<MdInline>) : MdBlock

    data class Paragraph(val content: List<MdInline>) : MdBlock

    /** A fenced or indented code block; [language] is the first word of the fence's info string. */
    data class Code(val language: String?, val code: String) : MdBlock

    data class Quote(val blocks: List<MdBlock>) : MdBlock

    /** [tight]: no blank lines between or inside the items (rendered with less spacing). */
    data class ListBlock(val ordered: Boolean, val start: Int, val items: List<MdListItem>, val tight: Boolean) : MdBlock

    /** A GFM pipe table; every row has as many cells as [header]. */
    data class Table(val alignments: List<MdAlign>, val header: List<List<MdInline>>, val rows: List<List<List<MdInline>>>) : MdBlock

    data object Rule : MdBlock
}

/** A list item; [checked] is set for task list items (`- [ ]` / `- [x]`). */
data class MdListItem(val checked: Boolean?, val blocks: List<MdBlock>)

enum class MdAlign { None, Start, Center, End }

/** Inline content. */
sealed interface MdInline {
    data class Text(val text: String) : MdInline

    data class Code(val code: String) : MdInline

    data class Emphasis(val children: List<MdInline>) : MdInline

    data class Strong(val children: List<MdInline>) : MdInline

    data class Strike(val children: List<MdInline>) : MdInline

    /** A link (also images, shown as a link with their alt text, and autolinks). */
    data class Link(val url: String, val children: List<MdInline>) : MdInline

    /** A hard line break (two trailing spaces or a backslash before the newline). */
    data object LineBreak : MdInline

    /** A line ending inside a paragraph. The renderer keeps it as a newline (like GitHub comments). */
    data object SoftBreak : MdInline
}

/** Plain text of inline content (for copying and accessibility). */
fun List<MdInline>.plainText(): String = buildString { appendPlain(this@plainText) }

private fun StringBuilder.appendPlain(inlines: List<MdInline>) {
    for (inline in inlines) {
        when (inline) {
            is MdInline.Text -> append(inline.text)
            is MdInline.Code -> append(inline.code)
            is MdInline.Emphasis -> appendPlain(inline.children)
            is MdInline.Strong -> appendPlain(inline.children)
            is MdInline.Strike -> appendPlain(inline.children)
            is MdInline.Link -> appendPlain(inline.children)
            MdInline.LineBreak, MdInline.SoftBreak -> append('\n')
        }
    }
}

object Markdown {
    fun parse(text: String): List<MdBlock> = BlockParser(normalize(text)).parse()

    /** Parses inline content only (table cells, headings). */
    fun parseInline(text: String): List<MdInline> = InlineParser(text).parse()

    /** Line endings to `\n`; leading tabs to spaces (tab stops of [TAB_WIDTH]) so indentation is comparable. */
    private fun normalize(text: String): List<String> =
        text.replace("\r\n", "\n").replace('\r', '\n').split('\n').map(::expandLeadingTabs)

    private fun expandLeadingTabs(line: String): String {
        if (!line.startsWith('\t') && !line.startsWith(' ')) return line
        val sb = StringBuilder()
        var col = 0
        var i = 0
        while (i < line.length && (line[i] == ' ' || line[i] == '\t')) {
            if (line[i] == '\t') {
                val spaces = TAB_WIDTH - col % TAB_WIDTH
                repeat(spaces) { sb.append(' ') }
                col += spaces
            } else {
                sb.append(' ')
                col++
            }
            i++
        }
        return sb.append(line, i, line.length).toString()
    }

    /** Columns per tab stop in indentation (CommonMark). */
    internal const val TAB_WIDTH = 4
}

// ----- blocks ---------------------------------------------------------------------------------

private val FENCE_OPEN = Regex("^( {0,3})(`{3,}|~{3,})(.*)$")
private val ATX_HEADING = Regex("^ {0,3}(#{1,6})(?:[ \\t]+(.*?))?[ \\t]*$")
private val ATX_CLOSING = Regex("(?:^|[ \\t]+)#+[ \\t]*$")
private val THEMATIC_BREAK = Regex("^ {0,3}(?:(?:\\*[ \\t]*){3,}|(?:-[ \\t]*){3,}|(?:_[ \\t]*){3,})$")
private val QUOTE = Regex("^ {0,3}> ?(.*)$")
private val LIST_ITEM = Regex("^( {0,3})([-+*]|\\d{1,9}[.)])(?:( +)(.*))?$")
private val SETEXT_UNDERLINE = Regex("^ {0,3}(=+|-+)[ \\t]*$")
private val TABLE_DELIMITER = Regex("^ {0,3}\\|?[ \\t]*:?-+:?[ \\t]*(?:\\|[ \\t]*:?-+:?[ \\t]*)*\\|?[ \\t]*$")
private val TASK_MARKER = Regex("^\\[([ xX])\\](?:[ \\t]+(.*)|$)")

/** Columns of indentation that make a line an indented code block. */
private const val CODE_INDENT = 4

/** Spaces after a list marker beyond which the item's content is an indented code block. */
private const val MAX_MARKER_GAP = 4

private fun indentOf(line: String): Int = line.indexOfFirst { it != ' ' }.let { if (it < 0) line.length else it }

private data class ListMarker(val indent: Int, val bullet: Char?, val delimiter: Char?, val number: Int, val contentColumn: Int, val content: String) {
    fun sameType(other: ListMarker) = bullet == other.bullet && delimiter == other.delimiter
}

private fun listMarker(line: String): ListMarker? {
    val m = LIST_ITEM.matchEntire(line) ?: return null
    val indent = m.groupValues[1].length
    val marker = m.groupValues[2]
    val gap = m.groups[3]?.value?.length ?: 0
    val rest = m.groups[4]?.value ?: ""
    // "-foo" is not a list item: the marker must be followed by a space or the end of the line.
    if (m.groups[3] == null && m.groupValues[2].length != line.trimEnd().length - indent) return null
    val markerWidth = marker.length
    val (column, content) = when {
        rest.isEmpty() -> indent + markerWidth + 1 to ""
        gap > MAX_MARKER_GAP -> indent + markerWidth + 1 to " ".repeat(gap - 1) + rest
        else -> indent + markerWidth + gap to rest
    }
    return if (marker[0].isDigit()) {
        ListMarker(indent, null, marker.last(), marker.dropLast(1).toInt(), column, content)
    } else {
        ListMarker(indent, marker[0], null, 0, column, content)
    }
}

private class BlockParser(private val lines: List<String>) {
    private var i = 0
    private val out = ArrayList<MdBlock>()

    fun parse(): List<MdBlock> {
        while (i < lines.size) {
            if (lines[i].isBlank()) {
                i++
                continue
            }
            if (fence() || heading() || rule() || quote() || list() || table() || indentedCode()) continue
            paragraph()
        }
        return out
    }

    private fun fence(): Boolean {
        val m = FENCE_OPEN.matchEntire(lines[i]) ?: return false
        val indent = m.groupValues[1].length
        val marker = m.groupValues[2]
        val info = m.groupValues[3].trim()
        if (marker[0] == '`' && info.contains('`')) return false
        val close = Regex("^ {0,3}${Regex.escape(marker[0].toString())}{${marker.length},}[ \\t]*$")
        val code = ArrayList<String>()
        i++
        while (i < lines.size) {
            val line = lines[i]
            if (close.matches(line)) {
                i++
                break
            }
            code += line.drop(minOf(indent, indentOf(line)))
            i++
        }
        out += MdBlock.Code(info.split(' ', '\t').firstOrNull()?.takeIf { it.isNotEmpty() }, code.joinToString("\n"))
        return true
    }

    private fun heading(): Boolean {
        val m = ATX_HEADING.matchEntire(lines[i]) ?: return false
        val content = (m.groups[2]?.value ?: "").replace(ATX_CLOSING, "").trim()
        out += MdBlock.Heading(m.groupValues[1].length, InlineParser(content).parse())
        i++
        return true
    }

    private fun rule(): Boolean {
        if (!THEMATIC_BREAK.matches(lines[i])) return false
        out += MdBlock.Rule
        i++
        return true
    }

    private fun quote(): Boolean {
        if (QUOTE.matchEntire(lines[i]) == null) return false
        val content = ArrayList<String>()
        while (i < lines.size) {
            val line = lines[i]
            val m = QUOTE.matchEntire(line)
            when {
                m != null -> content += m.groupValues[1]
                // Lazy continuation: paragraph text right after quoted paragraph text.
                line.isNotBlank() && content.lastOrNull()?.isNotBlank() == true && !startsBlock(line) -> content += line
                else -> break
            }
            i++
        }
        out += MdBlock.Quote(BlockParser(content).parse())
        return true
    }

    private fun list(): Boolean {
        val first = listMarker(lines[i]) ?: return false
        val items = ArrayList<MdListItem>()
        var loose = false
        while (i < lines.size) {
            val marker = listMarker(lines[i]) ?: break
            if (!marker.sameType(first) || marker.indent >= first.contentColumn) break
            val itemLines = arrayListOf(marker.content)
            i++
            while (i < lines.size) {
                val line = lines[i]
                if (line.isBlank()) {
                    itemLines += ""
                    i++
                    continue
                }
                if (indentOf(line) >= marker.contentColumn) {
                    itemLines += line.drop(marker.contentColumn)
                    i++
                    continue
                }
                val lazy = itemLines.last().isNotBlank() && !startsBlock(line) && !isContinuationBreaker(itemLines)
                if (lazy) {
                    itemLines += line.trimStart()
                    i++
                    continue
                }
                break
            }
            // Blank lines at the end of an item separate it from what follows.
            var trailingBlank = false
            while (itemLines.size > 1 && itemLines.last().isBlank()) {
                itemLines.removeAt(itemLines.lastIndex)
                trailingBlank = true
            }
            if (itemLines.dropWhile { it.isBlank() }.any { it.isBlank() }) loose = true
            val next = lines.getOrNull(i)?.let(::listMarker)
            val continues = next != null && next.sameType(first) && next.indent < first.contentColumn
            if (trailingBlank && continues) loose = true
            if (trailingBlank && !continues) {
                // The blank lines end the list; step back so they are consumed as separators.
                items += item(itemLines)
                break
            }
            items += item(itemLines)
        }
        out += MdBlock.ListBlock(ordered = first.bullet == null, start = first.number, items = items, tight = !loose)
        return true
    }

    /** The last line of the item so far opened a fenced code block that is still open: no lazy lines. */
    private fun isContinuationBreaker(itemLines: List<String>): Boolean {
        var open: String? = null
        for (line in itemLines) {
            val m = FENCE_OPEN.matchEntire(line)
            if (open == null && m != null) {
                open = m.groupValues[2]
            } else if (open != null && line.trim().startsWith(open) && line.trim().all { it == open[0] }) {
                open = null
            }
        }
        return open != null
    }

    private fun item(itemLines: List<String>): MdListItem {
        val firstLine = itemLines.first()
        val task = TASK_MARKER.matchEntire(firstLine)
        return if (task != null) {
            val rest = listOf(task.groups[2]?.value ?: "") + itemLines.drop(1)
            MdListItem(checked = task.groupValues[1] != " ", blocks = BlockParser(rest).parse())
        } else {
            MdListItem(checked = null, blocks = BlockParser(itemLines).parse())
        }
    }

    private fun table(): Boolean {
        val header = lines[i]
        val delimiter = lines.getOrNull(i + 1) ?: return false
        if (!header.contains('|') || !TABLE_DELIMITER.matches(delimiter)) return false
        val headerCells = splitRow(header)
        val delimiterCells = splitRow(delimiter)
        if (headerCells.size != delimiterCells.size || headerCells.isEmpty()) return false
        val alignments = delimiterCells.map { cell ->
            val c = cell.trim()
            when {
                c.startsWith(':') && c.endsWith(':') -> MdAlign.Center
                c.endsWith(':') -> MdAlign.End
                c.startsWith(':') -> MdAlign.Start
                else -> MdAlign.None
            }
        }
        i += 2
        val rows = ArrayList<List<List<MdInline>>>()
        while (i < lines.size) {
            val line = lines[i]
            if (line.isBlank() || (startsBlock(line) && !line.contains('|'))) break
            val cells = splitRow(line)
            rows += List(headerCells.size) { index -> InlineParser(cells.getOrElse(index) { "" }.trim()).parse() }
            i++
        }
        out += MdBlock.Table(alignments, headerCells.map { InlineParser(it.trim()).parse() }, rows)
        return true
    }

    private fun indentedCode(): Boolean {
        if (indentOf(lines[i]) < CODE_INDENT) return false
        val code = ArrayList<String>()
        while (i < lines.size && (lines[i].isBlank() || indentOf(lines[i]) >= CODE_INDENT)) {
            code += lines[i].drop(minOf(CODE_INDENT, indentOf(lines[i])))
            i++
        }
        while (code.isNotEmpty() && code.last().isBlank()) code.removeAt(code.lastIndex)
        out += MdBlock.Code(null, code.joinToString("\n"))
        return true
    }

    private fun paragraph() {
        val text = arrayListOf(lines[i].trimStart())
        i++
        while (i < lines.size) {
            val line = lines[i]
            if (line.isBlank()) break
            val setext = SETEXT_UNDERLINE.matchEntire(line)
            if (setext != null) {
                val level = if (setext.groupValues[1][0] == '=') 1 else 2
                out += MdBlock.Heading(level, InlineParser(text.joinToString("\n").trimEnd()).parse())
                i++
                return
            }
            if (interruptsParagraph(line)) break
            text += line.trimStart()
            i++
        }
        out += MdBlock.Paragraph(InlineParser(text.joinToString("\n").trimEnd()).parse())
    }

    private fun interruptsParagraph(line: String): Boolean {
        if (FENCE_OPEN.matches(line) || ATX_HEADING.matches(line) || THEMATIC_BREAK.matches(line) || QUOTE.matches(line)) return true
        val marker = listMarker(line) ?: return startsTable(line)
        // An empty item or an ordered list not starting at 1 does not interrupt a paragraph (CommonMark).
        return marker.content.isNotBlank() && (marker.bullet != null || marker.number == 1)
    }

    private fun startsTable(line: String): Boolean {
        val next = lines.getOrNull(i + 1) ?: return false
        return line.contains('|') && TABLE_DELIMITER.matches(next) && splitRow(line).size == splitRow(next).size
    }

    private fun startsBlock(line: String): Boolean =
        FENCE_OPEN.matches(line) || ATX_HEADING.matches(line) || THEMATIC_BREAK.matches(line) || QUOTE.matches(line) || listMarker(line) != null
}

/** Cells of a table row: outer pipes removed, split on unescaped pipes, `\|` kept as `|`. */
private fun splitRow(line: String): List<String> {
    var s = line.trim()
    if (s.startsWith('|')) s = s.drop(1)
    if (s.endsWith('|') && !s.endsWith("\\|")) s = s.dropLast(1)
    val cells = ArrayList<String>()
    val cell = StringBuilder()
    var k = 0
    while (k < s.length) {
        val c = s[k]
        if (c == '\\' && k + 1 < s.length && s[k + 1] == '|') {
            cell.append('|')
            k += 2
            continue
        }
        if (c == '|') {
            cells += cell.toString()
            cell.clear()
        } else {
            cell.append(c)
        }
        k++
    }
    cells += cell.toString()
    return cells
}

// ----- inlines --------------------------------------------------------------------------------

private const val ASCII_PUNCTUATION = "!\"#$%&'()*+,-./:;<=>?@[\\]^_`{|}~"
private val AUTOLINK = Regex("<([a-zA-Z][a-zA-Z0-9+.-]{1,31}:[^<>\\s]*)>")
private val EMAIL_AUTOLINK = Regex("<([a-zA-Z0-9.!#$%&'*+/=?^_`{|}~-]+@[a-zA-Z0-9](?:[a-zA-Z0-9-]{0,61}[a-zA-Z0-9])?(?:\\.[a-zA-Z0-9](?:[a-zA-Z0-9-]{0,61}[a-zA-Z0-9])?)*)>")

/** Characters trimmed from the end of a bare URL (GFM autolink extension). */
private const val URL_TRAILING_PUNCTUATION = "?!.,:*_~'\";"

private fun isPunctuation(c: Char): Boolean = c in ASCII_PUNCTUATION || Character.getType(c).let {
    it == Character.CONNECTOR_PUNCTUATION.toInt() || it == Character.DASH_PUNCTUATION.toInt() ||
        it == Character.START_PUNCTUATION.toInt() || it == Character.END_PUNCTUATION.toInt() ||
        it == Character.INITIAL_QUOTE_PUNCTUATION.toInt() || it == Character.FINAL_QUOTE_PUNCTUATION.toInt() ||
        it == Character.OTHER_PUNCTUATION.toInt()
}

private class InlineParser(private val src: String) {
    private sealed interface Node

    private class TextNode(val text: StringBuilder) : Node

    private class InlineNode(val inline: MdInline) : Node

    private class DelimNode(val char: Char, var count: Int, val canOpen: Boolean, val canClose: Boolean, val originalCount: Int) : Node

    private class BracketNode(val image: Boolean) : Node {
        var active = true
    }

    private val nodes = ArrayList<Node>()
    private var pos = 0

    fun parse(): List<MdInline> {
        while (pos < src.length) {
            val c = src[pos]
            when {
                c == '\\' -> escape()
                c == '`' -> codeSpan()
                c == '<' -> angle()
                c == '!' && src.getOrNull(pos + 1) == '[' -> {
                    nodes += BracketNode(image = true)
                    pos += 2
                }
                c == '[' -> {
                    nodes += BracketNode(image = false)
                    pos++
                }
                c == ']' -> closeBracket()
                c == '*' || c == '_' || c == '~' -> delimiterRun()
                c == '\n' -> lineEnd()
                (c == 'h' || c == 'w') && atWordStart() && bareUrl() -> Unit
                else -> {
                    text(c.toString())
                    pos++
                }
            }
        }
        processEmphasis(0)
        return toInlines(nodes)
    }

    private fun text(s: String) {
        val last = nodes.lastOrNull()
        if (last is TextNode) last.text.append(s) else nodes += TextNode(StringBuilder(s))
    }

    private fun escape() {
        val next = src.getOrNull(pos + 1)
        when {
            next == '\n' -> {
                nodes += InlineNode(MdInline.LineBreak)
                pos += 2
            }
            next != null && next in ASCII_PUNCTUATION -> {
                text(next.toString())
                pos += 2
            }
            else -> {
                text("\\")
                pos++
            }
        }
    }

    private fun codeSpan() {
        var run = 0
        while (pos + run < src.length && src[pos + run] == '`') run++
        var search = pos + run
        while (search < src.length) {
            val start = src.indexOf('`', search)
            if (start < 0) break
            var end = start
            while (end < src.length && src[end] == '`') end++
            if (end - start == run) {
                var code = src.substring(pos + run, start).replace('\n', ' ')
                if (code.length >= 2 && code.startsWith(' ') && code.endsWith(' ') && code.isNotBlank()) code = code.substring(1, code.length - 1)
                nodes += InlineNode(MdInline.Code(code))
                pos = end
                return
            }
            search = end
        }
        // No closing run: the backticks are literal.
        text("`".repeat(run))
        pos += run
    }

    /** Matches [regex] at [pos] without copying the rest of the text. */
    private fun lookingAt(regex: Regex): java.util.regex.Matcher? {
        val matcher = regex.toPattern().matcher(src).region(pos, src.length)
        return if (matcher.lookingAt()) matcher else null
    }

    private fun angle() {
        val auto = lookingAt(AUTOLINK)
        if (auto != null) {
            val url = auto.group(1).orEmpty()
            nodes += InlineNode(MdInline.Link(url, listOf(MdInline.Text(url))))
            pos = auto.end()
            return
        }
        val email = lookingAt(EMAIL_AUTOLINK)
        if (email != null) {
            val address = email.group(1).orEmpty()
            nodes += InlineNode(MdInline.Link("mailto:$address", listOf(MdInline.Text(address))))
            pos = email.end()
            return
        }
        text("<")
        pos++
    }

    private fun atWordStart(): Boolean {
        val prev = src.getOrNull(pos - 1) ?: return true
        return prev.isWhitespace() || prev == '(' || prev == '*' || prev == '_' || prev == '~'
    }

    private fun bareUrl(): Boolean {
        if (!src.startsWith("http://", pos) && !src.startsWith("https://", pos) && !src.startsWith("www.", pos)) return false
        var end = pos
        while (end < src.length && !src[end].isWhitespace() && src[end] != '<') end++
        var url = src.substring(pos, end)
        // Trailing punctuation and unbalanced closing parentheses are not part of the URL.
        while (url.isNotEmpty()) {
            val last = url.last()
            if (last in URL_TRAILING_PUNCTUATION) {
                url = url.dropLast(1)
            } else if (last == ')' && url.count { it == ')' } > url.count { it == '(' }) {
                url = url.dropLast(1)
            } else {
                break
            }
        }
        if (url.startsWith("www.") && url.length <= "www.".length) return false
        if (url.endsWith("://")) return false
        val target = if (url.startsWith("www.")) "http://$url" else url
        nodes += InlineNode(MdInline.Link(target, listOf(MdInline.Text(url))))
        pos += url.length
        return true
    }

    private fun lineEnd() {
        val last = nodes.lastOrNull()
        if (last is TextNode) {
            val trailing = last.text.length - last.text.trimEnd(' ').length
            last.text.setLength(last.text.length - trailing)
            nodes += InlineNode(if (trailing >= 2) MdInline.LineBreak else MdInline.SoftBreak)
        } else {
            nodes += InlineNode(MdInline.SoftBreak)
        }
        pos++
        while (pos < src.length && src[pos] == ' ') pos++
    }

    private fun delimiterRun() {
        val c = src[pos]
        var end = pos
        while (end < src.length && src[end] == c) end++
        val count = end - pos
        val before = src.getOrNull(pos - 1)
        val after = src.getOrNull(end)
        val beforeSpace = before == null || before.isWhitespace()
        val afterSpace = after == null || after.isWhitespace()
        val beforePunct = before != null && isPunctuation(before)
        val afterPunct = after != null && isPunctuation(after)
        val leftFlanking = !afterSpace && (!afterPunct || beforeSpace || beforePunct)
        val rightFlanking = !beforeSpace && (!beforePunct || afterSpace || afterPunct)
        val (canOpen, canClose) = if (c == '_') {
            (leftFlanking && (!rightFlanking || beforePunct)) to (rightFlanking && (!leftFlanking || afterPunct))
        } else {
            leftFlanking to rightFlanking
        }
        if (c == '~' && count > 2) {
            text(src.substring(pos, end))
        } else {
            nodes += DelimNode(c, count, canOpen, canClose, count)
        }
        pos = end
    }

    private fun closeBracket() {
        val openerIndex = nodes.indexOfLast { it is BracketNode }
        val opener = nodes.getOrNull(openerIndex) as? BracketNode
        if (opener == null || !opener.active) {
            if (opener != null) nodes.removeAt(openerIndex).also { nodes.add(openerIndex, TextNode(StringBuilder(if (opener.image) "![" else "["))) }
            text("]")
            pos++
            return
        }
        val destination = linkDestination(pos + 1)
        if (destination == null) {
            // Not a link: the brackets are literal.
            nodes[openerIndex] = TextNode(StringBuilder(if (opener.image) "![" else "["))
            text("]")
            pos++
            return
        }
        processEmphasis(openerIndex + 1)
        val children = toInlines(nodes.subList(openerIndex + 1, nodes.size).toList())
        while (nodes.size > openerIndex) nodes.removeAt(nodes.lastIndex)
        nodes += InlineNode(MdInline.Link(destination.first, children.ifEmpty { listOf(MdInline.Text(destination.first)) }))
        // Links do not nest: earlier openers become literal.
        if (!opener.image) nodes.filterIsInstance<BracketNode>().filter { !it.image }.forEach { it.active = false }
        pos = destination.second
    }

    /** `(dest "title")` starting at [start]; returns the destination and the index after `)`. */
    private fun linkDestination(start: Int): Pair<String, Int>? {
        if (src.getOrNull(start) != '(') return null
        var k = start + 1
        while (k < src.length && src[k].isWhitespace()) k++
        val dest = StringBuilder()
        if (src.getOrNull(k) == '<') {
            k++
            while (k < src.length && src[k] != '>' && src[k] != '\n') dest.append(src[k++])
            if (src.getOrNull(k) != '>') return null
            k++
        } else {
            var depth = 0
            while (k < src.length && !src[k].isWhitespace()) {
                val ch = src[k]
                if (ch == '\\' && k + 1 < src.length && src[k + 1] in ASCII_PUNCTUATION) {
                    dest.append(src[k + 1])
                    k += 2
                    continue
                }
                if (ch == '(') depth++
                if (ch == ')') {
                    if (depth == 0) break
                    depth--
                }
                dest.append(ch)
                k++
            }
        }
        while (k < src.length && src[k].isWhitespace()) k++
        val quote = src.getOrNull(k)
        if (quote == '"' || quote == '\'' || quote == '(') {
            val close = if (quote == '(') ')' else quote
            k++
            while (k < src.length && src[k] != close) {
                if (src[k] == '\\') k++
                k++
            }
            if (k >= src.length) return null
            k++
            while (k < src.length && src[k].isWhitespace()) k++
        }
        if (src.getOrNull(k) != ')') return null
        return dest.toString() to k + 1
    }

    /** CommonMark's "process emphasis" over the delimiters from [bottom] on. */
    private fun processEmphasis(bottom: Int) {
        var closerIndex = bottom
        while (closerIndex < nodes.size) {
            val closer = nodes[closerIndex] as? DelimNode
            if (closer == null || !closer.canClose) {
                closerIndex++
                continue
            }
            var openerIndex = closerIndex - 1
            var opener: DelimNode? = null
            while (openerIndex >= bottom) {
                val candidate = nodes[openerIndex] as? DelimNode
                if (candidate != null && candidate.char == closer.char && candidate.canOpen && matches(candidate, closer)) {
                    opener = candidate
                    break
                }
                openerIndex--
            }
            if (opener == null) {
                closerIndex++
                continue
            }
            val use = when {
                closer.char == '~' -> closer.count
                closer.count >= 2 && opener.count >= 2 -> 2
                else -> 1
            }
            val inner = toInlines(nodes.subList(openerIndex + 1, closerIndex).toList())
            val wrapped = when {
                closer.char == '~' -> MdInline.Strike(inner)
                use == 2 -> MdInline.Strong(inner)
                else -> MdInline.Emphasis(inner)
            }
            for (k in closerIndex - 1 downTo openerIndex + 1) nodes.removeAt(k)
            nodes.add(openerIndex + 1, InlineNode(wrapped))
            opener.count -= use
            closer.count -= use
            closerIndex = openerIndex + 2
            if (closer.count == 0) nodes.removeAt(closerIndex)
            if (opener.count == 0) {
                nodes.removeAt(openerIndex)
                closerIndex--
            }
        }
    }

    private fun matches(opener: DelimNode, closer: DelimNode): Boolean {
        if (closer.char == '~') return opener.count == closer.count
        // The "rule of 3" of CommonMark for runs that can both open and close.
        if ((opener.canClose || closer.canOpen) && (opener.originalCount + closer.originalCount) % 3 == 0) {
            return opener.originalCount % 3 == 0 && closer.originalCount % 3 == 0
        }
        return true
    }

    private fun toInlines(list: List<Node>): List<MdInline> {
        val out = ArrayList<MdInline>()
        val pending = StringBuilder()
        fun flush() {
            if (pending.isNotEmpty()) {
                out += MdInline.Text(pending.toString())
                pending.clear()
            }
        }
        for (node in list) {
            when (node) {
                is TextNode -> pending.append(node.text)
                is DelimNode -> pending.append(node.char.toString().repeat(node.count))
                is BracketNode -> pending.append(if (node.image) "![" else "[")
                is InlineNode -> {
                    val inline = node.inline
                    if (inline is MdInline.Text) {
                        pending.append(inline.text)
                    } else {
                        flush()
                        out += inline
                    }
                }
            }
        }
        flush()
        return out
    }
}
