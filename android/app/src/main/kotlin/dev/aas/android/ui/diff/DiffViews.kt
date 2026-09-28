package dev.aas.android.ui.diff

import androidx.compose.foundation.ExperimentalFoundationApi
import androidx.compose.foundation.background
import androidx.compose.foundation.combinedClickable
import androidx.compose.foundation.horizontalScroll
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.rememberScrollState
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.ui.Modifier
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.platform.LocalDensity
import androidx.compose.ui.res.stringResource
import androidx.compose.ui.text.rememberTextMeasurer
import androidx.compose.ui.text.style.TextAlign
import androidx.compose.ui.unit.Dp
import androidx.compose.ui.unit.dp
import dev.aas.android.R
import dev.aas.android.domain.diff.DiffHunk
import dev.aas.android.domain.diff.DiffLine
import dev.aas.android.ui.theme.codeStyle
import dev.aas.android.ui.theme.statusColors

/** Colours of diff lines (tints of the status colours, readable in light and dark). */
data class DiffColors(val added: Color, val removed: Color, val hunk: Color, val addedMarker: Color, val removedMarker: Color)

@Composable
fun diffColors(): DiffColors {
    val status = MaterialTheme.statusColors
    return DiffColors(
        added = status.connected.copy(alpha = LINE_TINT_ALPHA),
        removed = status.error.copy(alpha = LINE_TINT_ALPHA),
        hunk = status.running.copy(alpha = HUNK_TINT_ALPHA),
        addedMarker = status.connected,
        removedMarker = status.error,
    )
}

/** Width of a line-number column holding [digits] digits of the code font. */
@Composable
fun lineNumberWidth(digits: Int): Dp {
    val measurer = rememberTextMeasurer()
    val style = MaterialTheme.codeStyle
    val px = measurer.measure("0".repeat(digits.coerceAtLeast(1)), style).size.width
    return with(LocalDensity.current) { px.toDp() } + NUMBER_PADDING
}

/** Width of [columns] characters of the code font (monospace; wide characters count two). */
@Composable
fun codeTextWidth(columns: Int): Dp {
    val measurer = rememberTextMeasurer()
    val style = MaterialTheme.codeStyle
    // Measured over a run of characters so fractional advances add up as they do in a line.
    val px = measurer.measure("0".repeat(WIDTH_SAMPLE), style).size.width.toFloat() / WIDTH_SAMPLE
    return with(LocalDensity.current) { (px * columns).toDp() }
}

/** Digits of the largest line number of [hunks]. */
fun lineNumberDigits(hunks: List<DiffHunk>): Int =
    hunks.maxOfOrNull { maxOf(it.oldStart + it.oldCount, it.newStart + it.newCount) }?.toString()?.length ?: 1

/** The `@@ … @@` header row of a hunk. */
@Composable
fun HunkHeaderRow(hunk: DiffHunk, modifier: Modifier = Modifier) {
    val colors = diffColors()
    Text(
        hunk.header,
        style = MaterialTheme.codeStyle,
        color = MaterialTheme.colorScheme.onSurfaceVariant,
        softWrap = false,
        modifier = modifier.fillMaxWidth().background(colors.hunk).padding(horizontal = 8.dp, vertical = 2.dp),
    )
}

/**
 * One diff line: old and new line numbers, the `+`/`-` marker and the text on its tint.
 * [wrap] folds long lines; otherwise the row is as wide as its text (the list scrolls sideways).
 */
@OptIn(ExperimentalFoundationApi::class)
@Composable
fun DiffLineRow(line: DiffLine, numberWidth: Dp, wrap: Boolean, modifier: Modifier = Modifier, onLongPress: (() -> Unit)? = null) {
    val colors = diffColors()
    val (background, marker, markerColor) = when (line.kind) {
        DiffLine.Kind.Added -> Triple(colors.added, "+", colors.addedMarker)
        DiffLine.Kind.Removed -> Triple(colors.removed, "-", colors.removedMarker)
        DiffLine.Kind.Context -> Triple(Color.Transparent, " ", MaterialTheme.colorScheme.onSurfaceVariant)
        DiffLine.Kind.NoNewline -> Triple(Color.Transparent, "\\", MaterialTheme.colorScheme.onSurfaceVariant)
    }
    val numberColor = MaterialTheme.colorScheme.onSurfaceVariant
    Row(
        modifier
            .then(if (wrap) Modifier.fillMaxWidth() else Modifier)
            .background(background)
            .then(if (onLongPress != null) Modifier.combinedClickable(onClick = {}, onLongClick = onLongPress) else Modifier),
    ) {
        Text(line.oldNumber?.toString() ?: "", style = MaterialTheme.codeStyle, color = numberColor, textAlign = TextAlign.End, modifier = Modifier.width(numberWidth).padding(end = 4.dp))
        Text(line.newNumber?.toString() ?: "", style = MaterialTheme.codeStyle, color = numberColor, textAlign = TextAlign.End, modifier = Modifier.width(numberWidth).padding(end = 4.dp))
        Text(marker, style = MaterialTheme.codeStyle, color = markerColor, modifier = Modifier.width(MARKER_WIDTH))
        val text = if (line.kind == DiffLine.Kind.NoNewline) stringResource(R.string.diff_no_newline) else displayText(line.text)
        Text(
            text,
            style = MaterialTheme.codeStyle,
            color = if (line.kind == DiffLine.Kind.NoNewline) numberColor else MaterialTheme.colorScheme.onSurface,
            softWrap = wrap,
            modifier = if (wrap) Modifier.weight(1f) else Modifier.padding(end = 12.dp),
        )
    }
}

/**
 * Hunks as a column (a file change inside the conversation): at most [maxLines] lines, then
 * "ほか n 行". The block scrolls sideways as a whole.
 */
@Composable
fun DiffHunkLines(hunks: List<DiffHunk>, maxLines: Int, modifier: Modifier = Modifier) {
    val numberWidth = lineNumberWidth(lineNumberDigits(hunks))
    var budget = maxLines
    val total = hunks.sumOf { it.lines.size }
    Column(modifier.fillMaxWidth()) {
        Column(Modifier.horizontalScroll(rememberScrollState())) {
            for (hunk in hunks) {
                if (budget <= 0) break
                HunkHeaderRow(hunk)
                for (line in hunk.lines) {
                    if (budget <= 0) break
                    DiffLineRow(line, numberWidth, wrap = false)
                    budget--
                }
            }
        }
        val hidden = total - maxLines
        if (hidden > 0) Text(stringResource(R.string.diff_more_lines, hidden), style = MaterialTheme.typography.labelSmall, color = MaterialTheme.colorScheme.onSurfaceVariant, modifier = Modifier.padding(8.dp))
    }
}

/** Tabs as spaces (a monospace font draws a tab as one narrow glyph). */
fun displayText(text: String): String = if ('\t' in text) text.replace("\t", "    ") else text

/**
 * Columns a line takes in a monospace font: East Asian wide and full-width characters take two
 * (Unicode's East Asian Width W/F ranges), everything else one. Sizes the sideways-scrolling
 * diff list.
 */
fun displayColumns(text: String): Int {
    var columns = 0
    var i = 0
    val s = displayText(text)
    while (i < s.length) {
        val cp = s.codePointAt(i)
        columns += if (isWide(cp)) 2 else 1
        i += Character.charCount(cp)
    }
    return columns
}

private fun isWide(cp: Int): Boolean =
    cp in 0x1100..0x115F || cp in 0x2E80..0x303E || cp in 0x3041..0x33FF || cp in 0x3400..0x4DBF ||
        cp in 0x4E00..0x9FFF || cp in 0xA000..0xA4CF || cp in 0xAC00..0xD7A3 || cp in 0xF900..0xFAFF ||
        cp in 0xFE30..0xFE4F || cp in 0xFF00..0xFF60 || cp in 0xFFE0..0xFFE6 || cp in 0x1F300..0x1F64F ||
        cp in 0x1F900..0x1F9FF || cp in 0x20000..0x3FFFD

private const val LINE_TINT_ALPHA = 0.16f
private const val WIDTH_SAMPLE = 100
private const val HUNK_TINT_ALPHA = 0.10f
private val NUMBER_PADDING = 8.dp
private val MARKER_WIDTH = 14.dp
