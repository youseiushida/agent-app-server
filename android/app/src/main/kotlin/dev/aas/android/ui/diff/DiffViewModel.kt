package dev.aas.android.ui.diff

import androidx.lifecycle.ViewModel
import androidx.lifecycle.viewModelScope
import dev.aas.android.DiffViewPolicy
import dev.aas.android.R
import dev.aas.android.data.BlobException
import dev.aas.android.data.BlobRepository
import dev.aas.android.data.ComposerDrafts
import dev.aas.android.data.ThreadRepository
import dev.aas.android.domain.diff.DiffLine
import dev.aas.android.domain.diff.PatchFile
import dev.aas.android.domain.diff.UnifiedDiff
import dev.aas.android.protocol.DiffScope
import dev.aas.android.protocol.DiffSummary
import dev.aas.android.protocol.FileChangeKind
import dev.aas.android.protocol.RpcException
import dev.aas.android.protocol.ThreadDiffResult
import dev.aas.android.sync.NotConnectedException
import dev.aas.android.sync.SyncStatus
import dev.aas.android.ui.common.UiText
import dev.aas.android.ui.common.UserMessages
import dev.aas.android.ui.common.requestFailed
import dev.aas.android.ui.navigation.DiffRoute
import kotlinx.coroutines.CancellationException
import kotlinx.coroutines.Job
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.flow.asStateFlow
import kotlinx.coroutines.flow.distinctUntilChanged
import kotlinx.coroutines.flow.map
import kotlinx.coroutines.flow.update
import kotlinx.coroutines.launch

enum class DiffScopeChoice { Turn, Thread }

/** Why a file's lines are not shown. */
enum class FileNote { None, Binary, NotInPatch, PatchTooLarge, FileTooLarge, NoHunks }

/** One file of the diff: the daemon's entry (`DiffFile`) with its part of the patch. */
data class DiffFileView(
    val index: Int,
    val path: String,
    val title: String,
    val kind: FileChangeKind,
    val added: Long,
    val removed: Long,
    val binary: Boolean,
    val patch: PatchFile?,
    val note: FileNote,
) {
    val tooLarge: Boolean get() = note == FileNote.FileTooLarge

    /** Rows of this file in the diff list (header, content, end); mirrors the screen's sections. */
    val rows: Int
        get() = 2 + if (note == FileNote.None && patch != null) patch.hunks.sumOf { 1 + it.lines.size } else 1
}

data class LoadedDiff(
    val summary: DiffSummary,
    val files: List<DiffFileView>,
    /** Too many lines for one list: one file at a time (UX §6 大きな差分). */
    val oneFileAtATime: Boolean,
    val selected: Int,
    val notice: UiText?,
) {
    val shownFiles: List<DiffFileView> get() = if (oneFileAtATime) listOfNotNull(files.getOrNull(selected)) else files

    /** The list index of file [index]'s header (all files shown). */
    fun firstRowOf(index: Int): Int = files.take(index).sumOf { it.rows }
}

sealed interface DiffContent {
    data object Loading : DiffContent

    data object Offline : DiffContent

    data class Failed(val message: UiText) : DiffContent

    data class Loaded(val diff: LoadedDiff) : DiffContent
}

data class DiffUiState(val scope: DiffScopeChoice, val turnAvailable: Boolean, val content: DiffContent)

/**
 * `thread/diff` for a turn or the whole thread. The patch comes inline or as a blob
 * (`patchBlobId`, above the daemon's inline limit), is parsed into files and matched with the
 * daemon's file list. Line comments go into the thread's composer draft.
 */
class DiffViewModel(
    private val route: DiffRoute,
    private val threads: ThreadRepository,
    private val blobs: BlobRepository,
    private val policy: DiffViewPolicy,
    private val drafts: ComposerDrafts,
    private val messages: UserMessages,
    status: StateFlow<SyncStatus>,
) : ViewModel() {
    private val _state = MutableStateFlow(
        DiffUiState(if (route.turnId != null) DiffScopeChoice.Turn else DiffScopeChoice.Thread, route.turnId != null, DiffContent.Loading),
    )
    val state: StateFlow<DiffUiState> = _state.asStateFlow()
    private var job: Job? = null

    init {
        reload()
        // Offline when opened: load as soon as the connection is back.
        viewModelScope.launch {
            status.map { it.isOnline }.distinctUntilChanged().collect { online ->
                if (online && _state.value.content == DiffContent.Offline) reload()
            }
        }
    }

    fun setScope(scope: DiffScopeChoice) {
        if (scope == _state.value.scope) return
        _state.update { it.copy(scope = scope) }
        reload()
    }

    fun select(index: Int) {
        _state.update { ui ->
            val content = ui.content as? DiffContent.Loaded ?: return@update ui
            ui.copy(content = DiffContent.Loaded(content.diff.copy(selected = index.coerceIn(0, content.diff.files.lastIndex))))
        }
    }

    fun reload() {
        job?.cancel()
        val scope = _state.value.scope
        _state.update { it.copy(content = DiffContent.Loading) }
        job = viewModelScope.launch {
            val content = try {
                val diffScope = if (scope == DiffScopeChoice.Turn && route.turnId != null) DiffScope.Turn(route.turnId) else DiffScope.Thread
                DiffContent.Loaded(load(threads.diff(route.threadId, diffScope)))
            } catch (e: CancellationException) {
                throw e
            } catch (e: NotConnectedException) {
                DiffContent.Offline
            } catch (e: RpcException) {
                DiffContent.Failed(UiText.of(R.string.diff_failed, e.error.message))
            } catch (e: Exception) {
                DiffContent.Failed(requestFailed(e))
            }
            _state.update { it.copy(content = content) }
        }
    }

    private suspend fun load(result: ThreadDiffResult): LoadedDiff {
        var notice: UiText? = null
        val inline = result.patch
        val blobId = result.patchBlobId
        val patchText: String? = when {
            inline != null -> inline
            blobId != null -> try {
                blobs.text(blobId, policy.maxPatchBytes)
            } catch (e: BlobException.TooLarge) {
                notice = UiText.of(R.string.diff_patch_too_large)
                null
            }
            else -> null
        }
        return build(result, patchText?.let { UnifiedDiff.parse(it) }, notice, policy)
    }

    /** Adds a comment about [line] of [file] to the thread's composer (sent with the next message). */
    fun comment(file: PatchFile, line: DiffLine, text: String) {
        val trimmed = text.trim()
        if (trimmed.isEmpty()) return
        drafts.appendParagraph(ComposerDrafts.threadKey(route.threadId), commentText(file, line, trimmed, policy.commentQuoteChars))
        messages.show(UiText.of(R.string.diff_comment_added))
    }

    companion object {
        /**
         * Joins the daemon's file list with the parsed patch. [patch] is `null` when the patch
         * was not available (too large); then every file says so.
         */
        fun build(result: ThreadDiffResult, patch: List<PatchFile>?, notice: UiText?, policy: DiffViewPolicy): LoadedDiff {
            val used = HashSet<Int>()
            val files = result.files.mapIndexed { index, file ->
                val found = patch?.withIndex()?.firstOrNull { (i, p) -> i !in used && (p.newPath == file.path || p.oldPath == file.path) }
                found?.let { used += it.index }
                val parsed = found?.value
                val title = if (parsed != null && parsed.kind == FileChangeKind.Move && parsed.oldPath != null && parsed.newPath != null) "${parsed.oldPath} → ${parsed.newPath}" else file.path
                val note = when {
                    file.binary || parsed?.binary == true -> FileNote.Binary
                    patch == null -> FileNote.PatchTooLarge
                    parsed == null -> FileNote.NotInPatch
                    parsed.lineCount > policy.maxFileLines -> FileNote.FileTooLarge
                    parsed.hunks.isEmpty() -> FileNote.NoHunks
                    else -> FileNote.None
                }
                DiffFileView(index, file.path, title, file.kind, file.added, file.removed, file.binary, parsed, note)
            }
            val shownLines = files.filter { it.note == FileNote.None }.sumOf { it.patch?.lineCount ?: 0 }
            return LoadedDiff(result.summary, files, oneFileAtATime = shownLines > policy.oneFileAtATimeLines, selected = 0, notice = notice ?: if (shownLines > policy.oneFileAtATimeLines) UiText.of(R.string.diff_one_file_at_a_time) else null)
        }

        /**
         * The comment as it goes into the message: the file and line, the line itself (quoted,
         * shortened), then the comment.
         */
        fun commentText(file: PatchFile, line: DiffLine, comment: String, quoteChars: Int): String {
            val number = line.newNumber ?: line.oldNumber
            val side = if (line.kind == DiffLine.Kind.Removed) " (-)" else ""
            val quoted = line.text.trim().let { if (it.length > quoteChars) it.take(quoteChars) + "…" else it }
            return "> `${file.path}:$number`$side\n> `$quoted`\n$comment"
        }
    }
}
