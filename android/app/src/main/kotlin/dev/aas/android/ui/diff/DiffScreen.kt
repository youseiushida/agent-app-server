package dev.aas.android.ui.diff

import androidx.compose.foundation.background
import androidx.compose.foundation.clickable
import androidx.compose.foundation.horizontalScroll
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxHeight
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.foundation.lazy.LazyListScope
import androidx.compose.foundation.lazy.LazyListState
import androidx.compose.foundation.lazy.items
import androidx.compose.foundation.lazy.rememberLazyListState
import androidx.compose.foundation.rememberScrollState
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.automirrored.outlined.ArrowBack
import androidx.compose.material3.CircularProgressIndicator
import androidx.compose.material3.ExperimentalMaterial3Api
import androidx.compose.material3.HorizontalDivider
import androidx.compose.material3.Icon
import androidx.compose.material3.IconButton
import androidx.compose.material3.IconToggleButton
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.PrimaryTabRow
import androidx.compose.material3.Scaffold
import androidx.compose.material3.Surface
import androidx.compose.material3.Tab
import androidx.compose.material3.Text
import androidx.compose.material3.TopAppBar
import androidx.compose.runtime.Composable
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.rememberCoroutineScope
import androidx.compose.runtime.saveable.rememberSaveable
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.platform.testTag
import androidx.compose.ui.res.stringResource
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.unit.Dp
import androidx.compose.ui.unit.dp
import androidx.lifecycle.compose.collectAsStateWithLifecycle
import androidx.navigation.NavGraphBuilder
import androidx.navigation.compose.composable
import androidx.navigation.toRoute
import dev.aas.android.R
import dev.aas.android.domain.diff.DiffLine
import dev.aas.android.domain.diff.PatchFile
import dev.aas.android.ui.common.aasViewModel
import dev.aas.android.ui.common.asString
import dev.aas.android.ui.components.EmptyState
import dev.aas.android.ui.components.TextInputDialog
import dev.aas.android.ui.icons.ChevronLeft
import dev.aas.android.ui.icons.ChevronRight
import dev.aas.android.ui.icons.Difference
import dev.aas.android.ui.icons.ExpandLess
import dev.aas.android.ui.icons.ExpandMore
import dev.aas.android.ui.icons.WrapText
import dev.aas.android.ui.navigation.AppNavigator
import dev.aas.android.ui.navigation.DiffRoute
import dev.aas.android.ui.theme.codeStyle
import dev.aas.android.ui.theme.statusColors
import dev.aas.android.ui.thread.fileKindColor
import dev.aas.android.ui.thread.fileKindLabel
import kotlinx.coroutines.launch

fun NavGraphBuilder.diffDestinations(navigator: AppNavigator) {
    composable<DiffRoute> { entry ->
        val route = entry.toRoute<DiffRoute>()
        val vm = aasViewModel(key = "${route.threadId}/${route.turnId}") { c, _ ->
            DiffViewModel(route, c.threadRepository, c.blobRepository, c.policy.diff, c.composerDrafts, c.userMessages, c.engine.status)
        }
        DiffScreen(vm, navigator)
    }
}

/** Test tag of the diff list. */
const val DIFF_LIST_TAG = "diff-list"

/**
 * The 変更 screen (docs/ux/codex-desktop.md §6, §8.2): a turn's or the whole thread's changes
 * (`thread/diff`), the changed files, and each file's unified diff with line numbers. Large
 * diffs show one file at a time; files too large for a phone say so. Long-pressing a line adds a
 * comment about it to the thread's composer.
 */
@OptIn(ExperimentalMaterial3Api::class)
@Composable
fun DiffScreen(vm: DiffViewModel, navigator: AppNavigator) {
    val ui by vm.state.collectAsStateWithLifecycle()
    var wrap by rememberSaveable { mutableStateOf(true) }
    var commentOn by remember { mutableStateOf<Pair<PatchFile, DiffLine>?>(null) }
    Scaffold(
        topBar = {
            TopAppBar(
                title = {
                    Column {
                        Text(stringResource(R.string.diff_title))
                        Text(
                            stringResource(if (ui.scope == DiffScopeChoice.Turn) R.string.diff_scope_turn else R.string.diff_scope_thread),
                            style = MaterialTheme.typography.labelSmall,
                            color = MaterialTheme.colorScheme.onSurfaceVariant,
                        )
                    }
                },
                navigationIcon = { IconButton(onClick = navigator::back) { Icon(Icons.AutoMirrored.Outlined.ArrowBack, contentDescription = stringResource(R.string.back)) } },
                actions = {
                    IconToggleButton(checked = wrap, onCheckedChange = { wrap = it }) { Icon(Icons.AutoMirrored.Outlined.WrapText, contentDescription = stringResource(R.string.wrap_lines)) }
                },
            )
        },
    ) { padding ->
        Column(Modifier.fillMaxSize().padding(padding)) {
            if (ui.turnAvailable) {
                PrimaryTabRow(selectedTabIndex = if (ui.scope == DiffScopeChoice.Turn) 0 else 1) {
                    Tab(selected = ui.scope == DiffScopeChoice.Turn, onClick = { vm.setScope(DiffScopeChoice.Turn) }, text = { Text(stringResource(R.string.diff_scope_turn)) })
                    Tab(selected = ui.scope == DiffScopeChoice.Thread, onClick = { vm.setScope(DiffScopeChoice.Thread) }, text = { Text(stringResource(R.string.diff_scope_thread)) })
                }
            }
            when (val content = ui.content) {
                DiffContent.Loading -> Box(Modifier.fillMaxSize(), contentAlignment = Alignment.Center) { CircularProgressIndicator() }
                DiffContent.Offline -> EmptyState(Icons.Outlined.Difference, stringResource(R.string.diff_offline), action = stringResource(R.string.action_retry), onAction = vm::reload)
                is DiffContent.Failed -> EmptyState(Icons.Outlined.Difference, content.message.asString(), action = stringResource(R.string.action_retry), onAction = vm::reload)
                is DiffContent.Loaded -> LoadedDiffView(content.diff, wrap, onSelect = vm::select, onComment = { file, line -> commentOn = file to line })
            }
        }
    }
    commentOn?.let { (file, line) ->
        TextInputDialog(
            title = stringResource(R.string.diff_comment_title, file.path, line.newNumber ?: line.oldNumber ?: 0),
            initial = "",
            confirm = stringResource(R.string.diff_comment_add),
            singleLine = false,
            label = stringResource(R.string.diff_comment_label),
            onConfirm = { text ->
                commentOn = null
                vm.comment(file, line, text)
            },
            onDismiss = { commentOn = null },
        )
    }
}

/**
 * A loaded diff: summary, the file index (tap to jump, or to select in one-file mode), and the
 * files' lines. [onSelect] chooses the file shown one at a time.
 */
@Composable
fun LoadedDiffView(diff: LoadedDiff, wrap: Boolean, onSelect: (Int) -> Unit, onComment: (PatchFile, DiffLine) -> Unit) {
    var filesOpen by rememberSaveable { mutableStateOf(false) }
    val listState = rememberLazyListState()
    val scope = rememberCoroutineScope()
    if (diff.files.isEmpty()) {
        EmptyState(Icons.Outlined.Difference, stringResource(R.string.diff_empty))
        return
    }
    Column(Modifier.fillMaxSize()) {
        Summary(diff, filesOpen, onToggle = { filesOpen = !filesOpen })
        if (filesOpen) {
            FileIndex(diff) { index ->
                filesOpen = false
                if (diff.oneFileAtATime) {
                    onSelect(index)
                } else {
                    scope.launch { listState.scrollToItem(diff.firstRowOf(index)) }
                }
            }
        }
        diff.notice?.let { Text(it.asString(), style = MaterialTheme.typography.bodySmall, color = MaterialTheme.colorScheme.onSurfaceVariant, modifier = Modifier.padding(horizontal = 16.dp, vertical = 4.dp)) }
        if (diff.oneFileAtATime) OneFileBar(diff, onPrevious = { onSelect(diff.selected - 1) }, onNext = { onSelect(diff.selected + 1) })
        DiffList(diff, wrap, listState, onComment)
    }
}

@Composable
private fun Summary(diff: LoadedDiff, filesOpen: Boolean, onToggle: () -> Unit) {
    Row(Modifier.fillMaxWidth().clickable(onClick = onToggle).padding(horizontal = 16.dp, vertical = 10.dp), verticalAlignment = Alignment.CenterVertically) {
        Text(stringResource(R.string.diff_summary, diff.summary.files), style = MaterialTheme.typography.titleSmall, modifier = Modifier.weight(1f))
        Text("+${diff.summary.insertions}", style = MaterialTheme.typography.labelLarge, color = MaterialTheme.statusColors.connected)
        Spacer(Modifier.width(6.dp))
        Text("−${diff.summary.deletions}", style = MaterialTheme.typography.labelLarge, color = MaterialTheme.statusColors.error)
        Icon(if (filesOpen) Icons.Outlined.ExpandLess else Icons.Outlined.ExpandMore, contentDescription = stringResource(R.string.diff_files))
    }
    HorizontalDivider()
}

@Composable
private fun FileIndex(diff: LoadedDiff, onOpen: (Int) -> Unit) {
    Column(Modifier.fillMaxWidth()) {
        diff.files.forEachIndexed { index, file ->
            Row(Modifier.fillMaxWidth().clickable { onOpen(index) }.padding(horizontal = 16.dp, vertical = 8.dp), verticalAlignment = Alignment.CenterVertically) {
                Text(fileKindLabel(file.kind), style = MaterialTheme.typography.labelSmall, color = fileKindColor(file.kind), modifier = Modifier.width(40.dp))
                Text(file.path, style = MaterialTheme.codeStyle, maxLines = 1, overflow = TextOverflow.Ellipsis, modifier = Modifier.weight(1f))
                if (file.binary) {
                    Text(stringResource(R.string.diff_binary), style = MaterialTheme.typography.labelSmall)
                } else {
                    Text("+${file.added}", style = MaterialTheme.typography.labelSmall, color = MaterialTheme.statusColors.connected)
                    Spacer(Modifier.width(4.dp))
                    Text("−${file.removed}", style = MaterialTheme.typography.labelSmall, color = MaterialTheme.statusColors.error)
                }
            }
        }
        HorizontalDivider()
    }
}

@Composable
private fun OneFileBar(diff: LoadedDiff, onPrevious: () -> Unit, onNext: () -> Unit) {
    Surface(color = MaterialTheme.colorScheme.surfaceContainerHigh) {
        Row(Modifier.fillMaxWidth().padding(horizontal = 4.dp), verticalAlignment = Alignment.CenterVertically) {
            IconButton(onClick = onPrevious, enabled = diff.selected > 0) { Icon(Icons.Outlined.ChevronLeft, contentDescription = stringResource(R.string.diff_previous_file)) }
            Text(
                stringResource(R.string.diff_file_position, diff.selected + 1, diff.files.size),
                style = MaterialTheme.typography.labelLarge,
                modifier = Modifier.weight(1f),
            )
            IconButton(onClick = onNext, enabled = diff.selected < diff.files.lastIndex) { Icon(Icons.Outlined.ChevronRight, contentDescription = stringResource(R.string.diff_next_file)) }
        }
    }
}

@Composable
private fun DiffList(diff: LoadedDiff, wrap: Boolean, listState: LazyListState, onComment: (PatchFile, DiffLine) -> Unit) {
    val shown = diff.shownFiles
    val digits = shown.mapNotNull { it.patch }.maxOfOrNull { lineNumberDigits(it.hunks) } ?: 1
    val numberWidth = lineNumberWidth(digits)
    val columns = remember(shown) { shown.mapNotNull { it.patch }.flatMap { p -> p.hunks.flatMap { h -> h.lines } }.maxOfOrNull { displayColumns(it.text) } ?: 0 }
    val width: Dp = numberWidth * 2 + MARKER_AND_PADDING + codeTextWidth(columns)
    val horizontal = rememberScrollState()
    Box(Modifier.fillMaxSize().then(if (wrap) Modifier else Modifier.horizontalScroll(horizontal))) {
        LazyColumn(
            state = listState,
            modifier = Modifier.fillMaxHeight().then(if (wrap) Modifier.fillMaxWidth() else Modifier.width(width)).testTag(DIFF_LIST_TAG),
        ) {
            shown.forEach { file -> fileSection(file, numberWidth, wrap, onComment) }
        }
    }
}

private fun LazyListScope.fileSection(file: DiffFileView, numberWidth: Dp, wrap: Boolean, onComment: (PatchFile, DiffLine) -> Unit) {
    stickyHeader(key = "file-${file.index}") {
        Surface(color = MaterialTheme.colorScheme.surfaceContainer, modifier = Modifier.fillMaxWidth()) {
            Row(Modifier.padding(horizontal = 12.dp, vertical = 8.dp), verticalAlignment = Alignment.CenterVertically) {
                Text(fileKindLabel(file.kind), style = MaterialTheme.typography.labelSmall, color = fileKindColor(file.kind), modifier = Modifier.width(40.dp))
                Text(file.title, style = MaterialTheme.codeStyle, maxLines = 2, overflow = TextOverflow.Ellipsis)
            }
        }
    }
    val patch = file.patch
    if (file.note != FileNote.None || patch == null) {
        item(key = "note-${file.index}") {
            Note(
                when (file.note) {
                    FileNote.Binary -> stringResource(R.string.diff_binary_file)
                    FileNote.PatchTooLarge -> stringResource(R.string.diff_patch_too_large_file)
                    FileNote.FileTooLarge -> stringResource(R.string.diff_file_too_large, patch?.lineCount ?: 0)
                    FileNote.NoHunks -> stringResource(R.string.diff_no_hunks)
                    FileNote.NotInPatch, FileNote.None -> stringResource(R.string.diff_file_not_in_patch)
                },
            )
        }
    } else {
        patch.hunks.forEachIndexed { h, hunk ->
            item(key = "hunk-${file.index}-$h") { HunkHeaderRow(hunk) }
            items(hunk.lines.size, key = { l -> "line-${file.index}-$h-$l" }) { l ->
                val line = hunk.lines[l]
                DiffLineRow(line, numberWidth, wrap, onLongPress = if (line.kind == DiffLine.Kind.NoNewline) null else ({ onComment(patch, line) }))
            }
        }
    }
    item(key = "end-${file.index}") { HorizontalDivider() }
}

@Composable
private fun Note(text: String) {
    Text(text, style = MaterialTheme.typography.bodyMedium, color = MaterialTheme.colorScheme.onSurfaceVariant, modifier = Modifier.fillMaxWidth().background(MaterialTheme.colorScheme.surface).padding(16.dp))
}

/** Room for the `+`/`-` marker and the end padding of a line. */
private val MARKER_AND_PADDING = 32.dp
