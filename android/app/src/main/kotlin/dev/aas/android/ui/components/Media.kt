package dev.aas.android.ui.components

import android.content.ClipData
import android.graphics.Bitmap
import android.util.Size
import androidx.annotation.StringRes
import androidx.compose.foundation.Image
import androidx.compose.foundation.background
import androidx.compose.foundation.clickable
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.material.icons.Icons
import androidx.compose.material3.CircularProgressIndicator
import androidx.compose.material3.Icon
import androidx.compose.material3.IconButton
import androidx.compose.material3.MaterialTheme
import androidx.compose.runtime.Composable
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableIntStateOf
import androidx.compose.runtime.produceState
import androidx.compose.runtime.remember
import androidx.compose.runtime.rememberCoroutineScope
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clip
import androidx.compose.ui.graphics.asImageBitmap
import androidx.compose.ui.layout.ContentScale
import androidx.compose.ui.platform.ClipEntry
import androidx.compose.ui.platform.LocalClipboard
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.res.stringResource
import androidx.compose.ui.semantics.contentDescription
import androidx.compose.ui.semantics.semantics
import androidx.compose.ui.unit.Dp
import androidx.compose.ui.unit.dp
import androidx.core.net.toUri
import dev.aas.android.R
import dev.aas.android.data.BlobException
import dev.aas.android.protocol.BlobId
import dev.aas.android.ui.common.LocalAppContainer
import dev.aas.android.ui.common.UiText
import dev.aas.android.ui.common.asString
import dev.aas.android.ui.icons.BrokenImage
import dev.aas.android.ui.icons.ContentCopy
import kotlinx.coroutines.CancellationException
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.launch
import kotlinx.coroutines.withContext
import java.io.IOException

/** A copy-to-clipboard icon; the snackbar confirms with [confirmation]. */
@Composable
fun CopyIconButton(text: String, @StringRes confirmation: Int, modifier: Modifier = Modifier) {
    val clipboard = LocalClipboard.current
    val scope = rememberCoroutineScope()
    val messages = LocalAppContainer.current.userMessages
    val label = stringResource(R.string.copy)
    IconButton(
        onClick = {
            scope.launch {
                clipboard.setClipEntry(ClipEntry(ClipData.newPlainText(label, text)))
                messages.show(UiText.of(confirmation))
            }
        },
        modifier = modifier,
    ) { Icon(Icons.Outlined.ContentCopy, contentDescription = label) }
}

/** Copies [text] (for menus and long presses; confirms with a snackbar). */
@Composable
fun rememberCopyAction(@StringRes confirmation: Int): (String) -> Unit {
    val clipboard = LocalClipboard.current
    val scope = rememberCoroutineScope()
    val messages = LocalAppContainer.current.userMessages
    val label = stringResource(R.string.copy)
    return remember(clipboard, scope, messages, label) {
        { text ->
            scope.launch {
                clipboard.setClipEntry(ClipEntry(ClipData.newPlainText(label, text)))
                messages.show(UiText.of(confirmation))
            }
        }
    }
}

private sealed interface ImageState {
    data object Loading : ImageState

    data class Loaded(val bitmap: Bitmap) : ImageState

    data class Failed(val message: UiText) : ImageState
}

/**
 * An image blob (an attachment of a message): downloaded once (then from the disk cache, also
 * offline), scaled to [maxEdgePx]. A failure shows a broken image; tapping it retries.
 */
@Composable
fun BlobImage(
    blobId: BlobId,
    maxEdgePx: Int,
    size: Dp,
    modifier: Modifier = Modifier,
    contentScale: ContentScale = ContentScale.Crop,
    onClick: (() -> Unit)? = null,
) {
    val images = LocalAppContainer.current.blobImages
    var attempt by remember(blobId) { mutableIntStateOf(0) }
    val state by produceState<ImageState>(ImageState.Loading, blobId, maxEdgePx, attempt) {
        value = ImageState.Loading
        value = try {
            ImageState.Loaded(images.load(blobId, maxEdgePx))
        } catch (e: CancellationException) {
            throw e
        } catch (e: BlobException) {
            ImageState.Failed(e.describe())
        } catch (e: Exception) {
            ImageState.Failed(UiText.of(R.string.image_unreadable, e.message ?: e.javaClass.simpleName))
        }
    }
    ImageBox(state, size, modifier, contentScale, onClick = onClick, onRetry = { attempt++ })
}

/** A picked image before and while it uploads (composer attachments): the system thumbnail. */
@Composable
fun LocalImage(uri: String, size: Dp, modifier: Modifier = Modifier) {
    val context = LocalContext.current
    val px = with(androidx.compose.ui.platform.LocalDensity.current) { size.roundToPx() }
    var attempt by remember(uri) { mutableIntStateOf(0) }
    val state by produceState<ImageState>(ImageState.Loading, uri, px, attempt) {
        value = try {
            ImageState.Loaded(withContext(Dispatchers.IO) { context.contentResolver.loadThumbnail(uri.toUri(), Size(px, px), null) })
        } catch (e: CancellationException) {
            throw e
        } catch (e: IOException) {
            ImageState.Failed(UiText.of(R.string.image_unreadable, e.message ?: ""))
        } catch (e: SecurityException) {
            ImageState.Failed(UiText.of(R.string.image_unreadable, e.message ?: ""))
        }
    }
    ImageBox(state, size, modifier, ContentScale.Crop, onClick = null, onRetry = { attempt++ })
}

@Composable
private fun ImageBox(state: ImageState, size: Dp, modifier: Modifier, contentScale: ContentScale, onClick: (() -> Unit)?, onRetry: () -> Unit) {
    val shape = RoundedCornerShape(8.dp)
    val description = stringResource(R.string.image_attachment)
    Box(
        modifier
            .size(size)
            .clip(shape)
            .background(MaterialTheme.colorScheme.surfaceContainerHighest)
            .semantics { contentDescription = description },
        contentAlignment = Alignment.Center,
    ) {
        when (state) {
            ImageState.Loading -> CircularProgressIndicator(Modifier.size(20.dp), strokeWidth = 2.dp)
            is ImageState.Loaded -> Image(
                state.bitmap.asImageBitmap(),
                contentDescription = null,
                contentScale = contentScale,
                modifier = Modifier.size(size).then(if (onClick != null) Modifier.clickable(onClick = onClick) else Modifier),
            )
            is ImageState.Failed -> {
                val retry = stringResource(R.string.image_retry)
                val reason = state.message.asString()
                Icon(
                    Icons.Outlined.BrokenImage,
                    contentDescription = "$reason $retry",
                    tint = MaterialTheme.colorScheme.onSurfaceVariant,
                    modifier = Modifier.clickable(onClick = onRetry),
                )
            }
        }
    }
}
