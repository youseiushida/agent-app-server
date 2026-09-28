package dev.aas.android.ui.components

import androidx.compose.foundation.border
import androidx.compose.foundation.horizontalScroll
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.IntrinsicSize
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxHeight
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.layout.widthIn
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.material.icons.Icons
import androidx.compose.material3.HorizontalDivider
import androidx.compose.material3.Icon
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Surface
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.remember
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.layout.Layout
import androidx.compose.ui.res.stringResource
import androidx.compose.ui.text.AnnotatedString
import androidx.compose.ui.text.LinkAnnotation
import androidx.compose.ui.text.SpanStyle
import androidx.compose.ui.text.TextLinkStyles
import androidx.compose.ui.text.TextStyle
import androidx.compose.ui.text.buildAnnotatedString
import androidx.compose.ui.text.font.FontFamily
import androidx.compose.ui.text.font.FontStyle
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.text.style.TextAlign
import androidx.compose.ui.text.style.TextDecoration
import androidx.compose.ui.text.withLink
import androidx.compose.ui.text.withStyle
import androidx.compose.ui.unit.Constraints
import androidx.compose.ui.unit.dp
import dev.aas.android.R
import dev.aas.android.domain.markdown.Markdown
import dev.aas.android.domain.markdown.MdAlign
import dev.aas.android.domain.markdown.MdBlock
import dev.aas.android.domain.markdown.MdInline
import dev.aas.android.ui.icons.CheckBox
import dev.aas.android.ui.icons.CheckBoxOutlineBlank
import dev.aas.android.ui.theme.codeStyle

/**
 * An agent message as Markdown (docs/ux/codex-desktop.md §3.2: the answer in full, with code
 * blocks, tables and links). The text is parsed again when it changes (streaming), which is
 * linear in its length.
 */
@Composable
fun MarkdownText(text: String, modifier: Modifier = Modifier, style: TextStyle = MaterialTheme.typography.bodyLarge) {
    val blocks = remember(text) { Markdown.parse(text) }
    MarkdownBlocks(blocks, modifier, style)
}

@Composable
fun MarkdownBlocks(blocks: List<MdBlock>, modifier: Modifier = Modifier, style: TextStyle = MaterialTheme.typography.bodyLarge) {
    Column(modifier, verticalArrangement = Arrangement.spacedBy(8.dp)) {
        for (block in blocks) Block(block, style)
    }
}

@Composable
private fun Block(block: MdBlock, style: TextStyle) {
    when (block) {
        is MdBlock.Heading -> Text(
            inlineText(block.content),
            style = when (block.level) {
                1 -> MaterialTheme.typography.titleLarge
                2 -> MaterialTheme.typography.titleMedium
                else -> MaterialTheme.typography.titleSmall
            }.copy(fontWeight = FontWeight.SemiBold),
        )
        is MdBlock.Paragraph -> Text(inlineText(block.content), style = style)
        is MdBlock.Code -> CodeCard(block.code, block.language)
        is MdBlock.Quote -> Row(Modifier.height(IntrinsicSize.Min)) {
            Surface(color = MaterialTheme.colorScheme.outlineVariant, modifier = Modifier.width(3.dp).fillMaxHeight()) {}
            Spacer(Modifier.width(10.dp))
            MarkdownBlocks(block.blocks, Modifier.weight(1f), style.copy(color = MaterialTheme.colorScheme.onSurfaceVariant))
        }
        is MdBlock.ListBlock -> Column(verticalArrangement = Arrangement.spacedBy(if (block.tight) 2.dp else 8.dp)) {
            block.items.forEachIndexed { index, item ->
                Row {
                    val marker = when {
                        item.checked != null -> null
                        block.ordered -> "${block.start + index}."
                        else -> "•"
                    }
                    Box(Modifier.widthIn(min = LIST_MARKER_WIDTH), contentAlignment = Alignment.TopStart) {
                        if (marker != null) {
                            Text(marker, style = style)
                        } else {
                            Icon(
                                if (item.checked == true) Icons.Outlined.CheckBox else Icons.Outlined.CheckBoxOutlineBlank,
                                contentDescription = stringResource(if (item.checked == true) R.string.markdown_task_done else R.string.markdown_task_open),
                                modifier = Modifier.padding(top = 2.dp),
                            )
                        }
                    }
                    MarkdownBlocks(item.blocks, Modifier.weight(1f), style)
                }
            }
        }
        is MdBlock.Table -> TableView(block, style)
        MdBlock.Rule -> HorizontalDivider(Modifier.padding(vertical = 4.dp))
    }
}

/** A code block with its language and a copy button, scrolling sideways. */
@Composable
fun CodeCard(code: String, language: String?, modifier: Modifier = Modifier) {
    Surface(color = MaterialTheme.colorScheme.surfaceContainerHighest, shape = RoundedCornerShape(8.dp), modifier = modifier.fillMaxWidth()) {
        Column {
            Row(Modifier.fillMaxWidth().padding(start = 12.dp, end = 4.dp), verticalAlignment = Alignment.CenterVertically) {
                Text(
                    language ?: stringResource(R.string.markdown_code),
                    style = MaterialTheme.typography.labelSmall,
                    color = MaterialTheme.colorScheme.onSurfaceVariant,
                    modifier = Modifier.weight(1f),
                )
                CopyIconButton(code, R.string.copied_code)
            }
            Text(
                code,
                style = MaterialTheme.codeStyle,
                softWrap = false,
                modifier = Modifier.horizontalScroll(rememberScrollState()).padding(start = 12.dp, end = 12.dp, bottom = 10.dp),
            )
        }
    }
}

@Composable
private fun TableView(table: MdBlock.Table, style: TextStyle) {
    val border = MaterialTheme.colorScheme.outlineVariant
    val header = MaterialTheme.colorScheme.surfaceContainerHigh
    val cells = ArrayList<@Composable () -> Unit>()
    val columns = table.header.size
    val rows = listOf(table.header) + table.rows
    rows.forEachIndexed { rowIndex, row ->
        row.forEachIndexed { column, content ->
            cells += {
                Surface(color = if (rowIndex == 0) header else Color.Transparent) {
                    Text(
                        inlineText(content),
                        style = if (rowIndex == 0) style.copy(fontWeight = FontWeight.SemiBold) else style,
                        textAlign = when (table.alignments.getOrNull(column)) {
                            MdAlign.Center -> TextAlign.Center
                            MdAlign.End -> TextAlign.End
                            else -> TextAlign.Start
                        },
                        modifier = Modifier.border(0.5.dp, border).padding(horizontal = 8.dp, vertical = 6.dp),
                    )
                }
            }
        }
    }
    Box(Modifier.horizontalScroll(rememberScrollState())) {
        TableLayout(columns, cells)
    }
}

/**
 * A grid: each column as wide as its widest cell (up to [MAX_CELL_WIDTH]), each row as tall as
 * its tallest cell. The table scrolls sideways when wider than the screen.
 */
@Composable
private fun TableLayout(columns: Int, cells: List<@Composable () -> Unit>) {
    Layout(content = { cells.forEach { it() } }) { measurables, _ ->
        val rows = (measurables.size + columns - 1) / columns
        val maxCell = MAX_CELL_WIDTH.roundToPx()
        val widths = IntArray(columns)
        measurables.forEachIndexed { index, m -> widths[index % columns] = maxOf(widths[index % columns], minOf(m.maxIntrinsicWidth(Constraints.Infinity), maxCell)) }
        // Row heights from the intrinsic heights at the column widths, so every cell of a row is
        // measured once at the row's height and the borders line up.
        val heights = IntArray(rows)
        measurables.forEachIndexed { index, m -> heights[index / columns] = maxOf(heights[index / columns], m.maxIntrinsicHeight(widths[index % columns])) }
        val stretched = measurables.mapIndexed { index, m -> m.measure(Constraints.fixed(widths[index % columns], heights[index / columns])) }
        layout(widths.sum(), heights.sum()) {
            var y = 0
            for (row in 0 until rows) {
                var x = 0
                for (column in 0 until columns) {
                    stretched.getOrNull(row * columns + column)?.place(x, y)
                    x += widths[column]
                }
                y += heights[row]
            }
        }
    }
}

/** Inline Markdown as styled text with clickable links. */
@Composable
fun inlineText(inlines: List<MdInline>): AnnotatedString {
    val code = SpanStyle(fontFamily = FontFamily.Monospace, background = MaterialTheme.colorScheme.surfaceContainerHighest)
    val link = TextLinkStyles(SpanStyle(color = MaterialTheme.colorScheme.primary, textDecoration = TextDecoration.Underline))
    return remember(inlines, code, link) { buildAnnotatedString { appendInlines(inlines, code, link) } }
}

private fun AnnotatedString.Builder.appendInlines(inlines: List<MdInline>, code: SpanStyle, link: TextLinkStyles) {
    for (inline in inlines) {
        when (inline) {
            is MdInline.Text -> append(inline.text)
            is MdInline.Code -> withStyle(code) { append(inline.code) }
            is MdInline.Emphasis -> withStyle(SpanStyle(fontStyle = FontStyle.Italic)) { appendInlines(inline.children, code, link) }
            is MdInline.Strong -> withStyle(SpanStyle(fontWeight = FontWeight.Bold)) { appendInlines(inline.children, code, link) }
            is MdInline.Strike -> withStyle(SpanStyle(textDecoration = TextDecoration.LineThrough)) { appendInlines(inline.children, code, link) }
            is MdInline.Link -> withLink(LinkAnnotation.Url(inline.url, link)) { appendInlines(inline.children, code, link) }
            MdInline.LineBreak, MdInline.SoftBreak -> append('\n')
        }
    }
}

/** Room for "10." before list items. */
private val LIST_MARKER_WIDTH = 24.dp

/** Widest a table column grows before its text wraps. */
private val MAX_CELL_WIDTH = 280.dp
