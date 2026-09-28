package dev.aas.android.ui.thread

import android.graphics.Bitmap
import androidx.compose.foundation.Image
import androidx.compose.foundation.background
import androidx.compose.foundation.gestures.rememberTransformableState
import androidx.compose.foundation.gestures.transformable
import androidx.compose.foundation.horizontalScroll
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.fillMaxHeight
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.foundation.lazy.itemsIndexed
import androidx.compose.foundation.rememberScrollState
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.automirrored.outlined.ArrowBack
import androidx.compose.material3.CircularProgressIndicator
import androidx.compose.material3.ExperimentalMaterial3Api
import androidx.compose.material3.Icon
import androidx.compose.material3.IconButton
import androidx.compose.material3.IconToggleButton
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Scaffold
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.material3.TopAppBar
import androidx.compose.runtime.Composable
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableFloatStateOf
import androidx.compose.runtime.mutableIntStateOf
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.produceState
import androidx.compose.runtime.remember
import androidx.compose.runtime.saveable.rememberSaveable
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.geometry.Offset
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.graphics.asImageBitmap
import androidx.compose.ui.graphics.graphicsLayer
import androidx.compose.ui.layout.ContentScale
import androidx.compose.ui.res.stringResource
import androidx.compose.ui.text.style.TextAlign
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.unit.dp
import androidx.lifecycle.ViewModel
import androidx.lifecycle.compose.collectAsStateWithLifecycle
import androidx.lifecycle.viewModelScope
import dev.aas.android.AppPolicy
import dev.aas.android.R
import dev.aas.android.data.BlobException
import dev.aas.android.data.BlobRepository
import dev.aas.android.data.ThreadRepository
import dev.aas.android.protocol.Item
import dev.aas.android.sync.ThreadSync
import dev.aas.android.ui.common.LocalAppContainer
import dev.aas.android.ui.common.UiText
import dev.aas.android.ui.common.asString
import dev.aas.android.ui.common.requestFailed
import dev.aas.android.ui.components.CopyIconButton
import dev.aas.android.ui.components.EmptyState
import dev.aas.android.ui.diff.codeTextWidth
import dev.aas.android.ui.diff.displayColumns
import dev.aas.android.ui.diff.displayText
import dev.aas.android.ui.diff.lineNumberWidth
import dev.aas.android.ui.icons.BrokenImage
import dev.aas.android.ui.icons.Terminal
import dev.aas.android.ui.icons.WrapText
import dev.aas.android.ui.navigation.AppNavigator
import dev.aas.android.ui.theme.codeStyle
import kotlinx.coroutines.CancellationException
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.SharingStarted
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.flow.combine
import kotlinx.coroutines.flow.stateIn
import kotlinx.coroutines.launch

/** Whose output [OutputViewModel] shows. */
sealed interface OutputTarget {
    /** A command or tool call. */
    data class OfItem(val itemId: String) : OutputTarget

    /** A finished background task (`BackgroundTask.result`). */
    data class OfTask(val taskId: String) : OutputTarget
}

/** The full output of a command, a tool call or a background task. */
data class OutputUiState(
    val title: String?,
    val lines: List<String>?,
    val loading: Boolean,
    val error: UiText?,
    /** The daemon stopped recording the output (only the beginning exists). */
    val truncated: Boolean,
    val itemMissing: Boolean,
)

/**
 * Loads the output of one item or background task: the inline text, or the blob the daemon
 * stored when the output was larger than its inline limit (`outputBlobId`, protocol.md §5
 * `item/completed`, §3.1 `BackgroundTask.result`).
 */
class OutputViewModel(
    threadId: String,
    private val target: OutputTarget,
    threads: ThreadRepository,
    private val blobs: BlobRepository,
    private val policy: AppPolicy,
) : ViewModel() {
    private val blobText = MutableStateFlow<Pair<String, String>?>(null)
    private val loading = MutableStateFlow(false)
    private val error = MutableStateFlow<UiText?>(null)
    private var requestedBlob: String? = null

    val state: StateFlow<OutputUiState> = combine(threads.observe(threadId), blobText, loading, error) { thread, blob, busy, failure ->
        val (found, output) = when (target) {
            is OutputTarget.OfItem -> {
                val item = thread.items.firstOrNull { it.id == target.itemId }
                item to when (item) {
                    is Item.CommandExecution -> Quad("$ ${item.command}", item.output, item.outputBlobId, item.outputTruncated)
                    is Item.ToolCall -> Quad(item.title.ifEmpty { item.name }, item.output.orEmpty(), item.outputBlobId, item.outputTruncated)
                    else -> Quad(null, null, null, false)
                }
            }
            is OutputTarget.OfTask -> {
                val task = thread.backgroundTasks.firstOrNull { it.id == target.taskId }
                val result = task?.result
                task to Quad(task?.title, result?.output.orEmpty().takeIf { task != null }, result?.outputBlobId, result?.outputTruncated ?: false)
            }
        }
        val (title, inline, blobId, truncated) = output
        val item = found
        if (blobId != null && requestedBlob != blobId) load(blobId)
        val text = if (blobId != null) blob?.takeIf { it.first == blobId }?.second else inline
        OutputUiState(
            title = title,
            lines = text?.let { splitLines(it) },
            loading = busy || (blobId != null && text == null && failure == null),
            error = failure,
            truncated = truncated && blobId == null,
            itemMissing = item == null && thread.sync != ThreadSync.Loading && thread.thread != null,
        )
    }.stateIn(viewModelScope, SharingStarted.WhileSubscribed(policy.uiStopTimeoutMs), OutputUiState(null, null, true, null, false, false))

    fun retry() {
        val blobId = requestedBlob ?: return
        requestedBlob = null
        error.value = null
        load(blobId)
    }

    private fun load(blobId: String) {
        requestedBlob = blobId
        loading.value = true
        viewModelScope.launch {
            try {
                blobText.value = blobId to blobs.text(blobId, policy.maxOutputDownloadBytes)
            } catch (e: CancellationException) {
                throw e
            } catch (e: BlobException) {
                error.value = e.describe()
            } catch (e: Exception) {
                error.value = requestFailed(e)
            } finally {
                loading.value = false
            }
        }
    }

    private data class Quad(val title: String?, val inline: String?, val blobId: String?, val truncated: Boolean)

    private fun splitLines(text: String): List<String> = text.trimEnd('\n').let { if (it.isEmpty()) emptyList() else it.split('\n') }
}

@OptIn(ExperimentalMaterial3Api::class)
@Composable
fun OutputScreen(vm: OutputViewModel, navigator: AppNavigator) {
    val ui by vm.state.collectAsStateWithLifecycle()
    var wrap by rememberSaveable { mutableStateOf(false) }
    Scaffold(
        topBar = {
            TopAppBar(
                title = { Text(ui.title ?: stringResource(R.string.output_title), maxLines = 1, overflow = TextOverflow.Ellipsis, style = MaterialTheme.codeStyle) },
                navigationIcon = { IconButton(onClick = navigator::back) { Icon(Icons.AutoMirrored.Outlined.ArrowBack, contentDescription = stringResource(R.string.back)) } },
                actions = {
                    IconToggleButton(checked = wrap, onCheckedChange = { wrap = it }) { Icon(Icons.AutoMirrored.Outlined.WrapText, contentDescription = stringResource(R.string.wrap_lines)) }
                    ui.lines?.let { lines -> CopyIconButton(lines.joinToString("\n"), R.string.copied_output) }
                },
            )
        },
    ) { padding ->
        Box(Modifier.fillMaxSize().padding(padding)) {
            val lines = ui.lines
            val error = ui.error
            when {
                ui.itemMissing -> EmptyState(Icons.Outlined.Terminal, stringResource(R.string.output_missing))
                error != null -> EmptyState(Icons.Outlined.Terminal, error.asString(), action = stringResource(R.string.action_retry), onAction = vm::retry)
                lines == null || ui.loading -> CircularProgressIndicator(Modifier.align(Alignment.Center))
                lines.isEmpty() -> EmptyState(Icons.Outlined.Terminal, stringResource(R.string.item_no_output))
                else -> {
                    val numberWidth = lineNumberWidth(lines.size.toString().length)
                    val columns = remember(lines) { lines.maxOf { displayColumns(it) } }
                    val width = numberWidth + codeTextWidth(columns) + LINE_END_PADDING
                    val horizontal = rememberScrollState()
                    Column(Modifier.fillMaxSize()) {
                        if (ui.truncated) Text(stringResource(R.string.item_output_truncated), style = MaterialTheme.typography.labelSmall, modifier = Modifier.padding(horizontal = 16.dp, vertical = 4.dp))
                        Box(Modifier.fillMaxSize().then(if (wrap) Modifier else Modifier.horizontalScroll(horizontal))) {
                            LazyColumn(Modifier.fillMaxHeight().then(if (wrap) Modifier.fillMaxWidth() else Modifier.width(width))) {
                                itemsIndexed(lines) { index, line ->
                                    Row(Modifier.fillMaxWidth()) {
                                        Text((index + 1).toString(), style = MaterialTheme.codeStyle, color = MaterialTheme.colorScheme.onSurfaceVariant, textAlign = TextAlign.End, modifier = Modifier.width(numberWidth).padding(end = 8.dp))
                                        Text(displayText(line), style = MaterialTheme.codeStyle, softWrap = wrap, modifier = Modifier.weight(1f))
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}

private sealed interface FullImage {
    data object Loading : FullImage

    data class Loaded(val bitmap: Bitmap) : FullImage

    data class Failed(val message: UiText) : FullImage
}

/** An attached image in full, with pinch to zoom. */
@OptIn(ExperimentalMaterial3Api::class)
@Composable
fun ImageScreen(blobId: String, navigator: AppNavigator) {
    val images = LocalAppContainer.current.blobImages
    var attempt by remember { mutableIntStateOf(0) }
    val image by produceState<FullImage>(FullImage.Loading, blobId, attempt) {
        value = FullImage.Loading
        value = try {
            FullImage.Loaded(images.load(blobId, FULL_IMAGE_EDGE_PX))
        } catch (e: CancellationException) {
            throw e
        } catch (e: BlobException) {
            FullImage.Failed(e.describe())
        } catch (e: Exception) {
            FullImage.Failed(requestFailed(e))
        }
    }
    var scale by remember { mutableFloatStateOf(1f) }
    var offset by remember { mutableStateOf(Offset.Zero) }
    val transform = rememberTransformableState { _, zoom, pan, _ ->
        scale = (scale * zoom).coerceIn(MIN_ZOOM, MAX_ZOOM)
        offset += pan
    }
    Scaffold(
        topBar = {
            TopAppBar(
                title = { Text(stringResource(R.string.image_title)) },
                navigationIcon = { IconButton(onClick = navigator::back) { Icon(Icons.AutoMirrored.Outlined.ArrowBack, contentDescription = stringResource(R.string.back)) } },
            )
        },
    ) { padding ->
        Box(Modifier.fillMaxSize().padding(padding).background(Color.Black), contentAlignment = Alignment.Center) {
            when (val state = image) {
                FullImage.Loading -> CircularProgressIndicator()
                is FullImage.Loaded -> Image(
                    state.bitmap.asImageBitmap(),
                    contentDescription = stringResource(R.string.image_attachment),
                    contentScale = ContentScale.Fit,
                    modifier = Modifier.fillMaxSize().transformable(transform).graphicsLayer {
                        scaleX = scale
                        scaleY = scale
                        translationX = offset.x
                        translationY = offset.y
                    },
                )
                is FullImage.Failed -> Column(horizontalAlignment = Alignment.CenterHorizontally) {
                    Icon(Icons.Outlined.BrokenImage, null, tint = Color.White)
                    Text(state.message.asString(), color = Color.White, textAlign = TextAlign.Center, modifier = Modifier.padding(16.dp))
                    TextButton(onClick = { attempt++ }) { Text(stringResource(R.string.action_retry)) }
                }
            }
        }
    }
}

/** Room after the longest line when lines do not wrap. */
private val LINE_END_PADDING = 24.dp

/** Decoded size of a full-screen image: sharp on a 1440 px wide phone screen. */
private const val FULL_IMAGE_EDGE_PX = 2048
private const val MIN_ZOOM = 1f
private const val MAX_ZOOM = 6f
