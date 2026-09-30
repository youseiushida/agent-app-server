package dev.aas.android.ui.composer

import androidx.compose.foundation.ExperimentalFoundationApi
import androidx.compose.foundation.background
import androidx.compose.foundation.clickable
import androidx.compose.foundation.combinedClickable
import androidx.compose.foundation.horizontalScroll
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.RowScope
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.heightIn
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.foundation.lazy.LazyListState
import androidx.compose.foundation.lazy.items
import androidx.compose.foundation.lazy.rememberLazyListState
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.shape.CircleShape
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.automirrored.filled.Send
import androidx.compose.material.icons.outlined.Close
import androidx.compose.material3.CircularProgressIndicator
import androidx.compose.material3.DropdownMenu
import androidx.compose.material3.DropdownMenuItem
import androidx.compose.material3.HorizontalDivider
import androidx.compose.material3.Icon
import androidx.compose.material3.IconButton
import androidx.compose.material3.IconButtonDefaults
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Surface
import androidx.compose.material3.Text
import androidx.compose.material3.TextField
import androidx.compose.material3.TextFieldDefaults
import androidx.compose.material3.minimumInteractiveComponentSize
import androidx.compose.runtime.Composable
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clip
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.platform.testTag
import androidx.compose.ui.res.stringResource
import androidx.compose.ui.semantics.Role
import androidx.compose.ui.semantics.contentDescription
import androidx.compose.ui.semantics.onLongClick
import androidx.compose.ui.semantics.role
import androidx.compose.ui.semantics.semantics
import androidx.compose.ui.text.input.TextFieldValue
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.unit.dp
import dev.aas.android.R
import dev.aas.android.domain.composer.ComposerTrigger
import dev.aas.android.domain.composer.PaletteEntry
import dev.aas.android.domain.composer.PaletteSource
import dev.aas.android.domain.composer.SendAction
import dev.aas.android.domain.composer.SendBlock
import dev.aas.android.domain.composer.SendState
import dev.aas.android.ui.common.asString
import dev.aas.android.ui.components.BlobImage
import dev.aas.android.ui.components.LocalImage
import dev.aas.android.ui.icons.AddPhotoAlternate
import dev.aas.android.ui.icons.AddToQueue
import dev.aas.android.ui.icons.Bolt
import dev.aas.android.ui.icons.Description
import dev.aas.android.ui.icons.ErrorOutline
import dev.aas.android.ui.icons.Folder
import dev.aas.android.ui.icons.PhotoCamera
import dev.aas.android.ui.icons.PhotoLibrary
import dev.aas.android.ui.icons.Stop
import dev.aas.android.ui.theme.codeStyle
import dev.aas.android.ui.theme.statusColors

/** Test tags of the composer (Compose UI tests). */
object ComposerTags {
    /** The rounded container of the input, its chips and its buttons. */
    const val CONTAINER = "composer-container"
    const val INPUT = "composer-input"
    const val SEND = "composer-send"
    const val PALETTE = "composer-palette"
    const val MENTIONS = "composer-mentions"
}

/**
 * The composer (docs/ux/codex-desktop.md §2, §8.2): the `/` palette or the `@` results right
 * above it (so they stay above the keyboard), then one rounded container (Material 3
 * `surfaceContainerHigh`) holding the attached images, the multi-line input without an outline
 * of its own, and a row of chips ([chips]: model, permission, context) with the image button and
 * the send button (a filled icon button; 停止 in the error container colours). The send button's
 * long press runs [SendState.alternate]. [interruptHint] is said under the composer while the
 * button stops the turn (e.g. that background work goes on).
 */
@Composable
fun ComposerBar(
    value: TextFieldValue,
    state: ComposerUiState,
    send: SendState,
    placeholder: String,
    imagesAllowed: Boolean,
    onValueChange: (TextFieldValue) -> Unit,
    onChoose: (PaletteEntry) -> Unit,
    onChooseMention: (String) -> Unit,
    onPickImages: () -> Unit,
    onTakePhoto: (() -> Unit)?,
    onRemoveAttachment: (String) -> Unit,
    onRetryAttachment: (String) -> Unit,
    onSend: (SendAction) -> Unit,
    modifier: Modifier = Modifier,
    inputEnabled: Boolean = true,
    interruptHint: String? = null,
    chips: @Composable RowScope.() -> Unit = {},
) {
    Column(modifier.fillMaxWidth()) {
        when (val trigger = state.trigger) {
            is ComposerTrigger.Slash -> PalettePopup(state, onChoose)
            is ComposerTrigger.Mention -> MentionPopup(state.mentionSearch, onChooseMention)
            null -> Unit
        }
        Surface(
            shape = RoundedCornerShape(CONTAINER_CORNER),
            color = MaterialTheme.colorScheme.surfaceContainerHigh,
            modifier = Modifier.fillMaxWidth().padding(horizontal = 8.dp, vertical = 6.dp).testTag(ComposerTags.CONTAINER),
        ) {
            Column(Modifier.padding(top = 4.dp, bottom = 4.dp)) {
                if (state.attachments.isNotEmpty()) {
                    Row(
                        Modifier.fillMaxWidth().horizontalScroll(rememberScrollState()).padding(horizontal = 12.dp, vertical = 6.dp),
                        horizontalArrangement = Arrangement.spacedBy(8.dp),
                    ) {
                        state.attachments.forEach { AttachmentThumb(it, onRemove = { onRemoveAttachment(it.id) }, onRetry = { onRetryAttachment(it.id) }) }
                    }
                    state.attachments.firstNotNullOfOrNull { it.state as? Attachment.State.Failed }?.let {
                        Text(it.message.asString(), style = MaterialTheme.typography.bodySmall, color = MaterialTheme.colorScheme.error, modifier = Modifier.padding(horizontal = 16.dp))
                    }
                }
                // The container is the field's outline: the text field draws neither its own
                // container nor an indicator line.
                val transparent = Color.Transparent
                TextField(
                    value = value,
                    onValueChange = onValueChange,
                    enabled = inputEnabled,
                    placeholder = { Text(placeholder, maxLines = 2, overflow = TextOverflow.Ellipsis) },
                    maxLines = INPUT_MAX_LINES,
                    colors = TextFieldDefaults.colors(
                        focusedContainerColor = transparent,
                        unfocusedContainerColor = transparent,
                        disabledContainerColor = transparent,
                        errorContainerColor = transparent,
                        focusedIndicatorColor = transparent,
                        unfocusedIndicatorColor = transparent,
                        disabledIndicatorColor = transparent,
                        errorIndicatorColor = transparent,
                    ),
                    modifier = Modifier.fillMaxWidth().testTag(ComposerTags.INPUT),
                )
                Row(Modifier.fillMaxWidth().padding(start = 12.dp, end = 4.dp), verticalAlignment = Alignment.CenterVertically) {
                    Row(
                        Modifier.weight(1f).horizontalScroll(rememberScrollState()),
                        horizontalArrangement = Arrangement.spacedBy(6.dp),
                        verticalAlignment = Alignment.CenterVertically,
                        content = chips,
                    )
                    if (imagesAllowed) AttachButton(inputEnabled, onPickImages, onTakePhoto)
                    SendButton(send, onSend)
                }
            }
        }
        (sendHint(send) ?: interruptHint?.takeIf { send.primary == SendAction.Interrupt && send.enabled })?.let {
            Text(it, style = MaterialTheme.typography.labelSmall, color = MaterialTheme.colorScheme.onSurfaceVariant, modifier = Modifier.padding(start = 16.dp, end = 16.dp, bottom = 4.dp))
        }
    }
}

/** Images: the photo picker, or (with a camera) a menu of both sources. */
@Composable
private fun AttachButton(enabled: Boolean, onPickImages: () -> Unit, onTakePhoto: (() -> Unit)?) {
    var menu by remember { mutableStateOf(false) }
    Box {
        IconButton(onClick = { if (onTakePhoto == null) onPickImages() else menu = true }, enabled = enabled) {
            Icon(Icons.Outlined.AddPhotoAlternate, contentDescription = stringResource(R.string.composer_attach_image))
        }
        DropdownMenu(expanded = menu, onDismissRequest = { menu = false }) {
            DropdownMenuItem(
                text = { Text(stringResource(R.string.composer_pick_photos)) },
                leadingIcon = { Icon(Icons.Outlined.PhotoLibrary, contentDescription = null) },
                onClick = {
                    menu = false
                    onPickImages()
                },
            )
            if (onTakePhoto != null) {
                DropdownMenuItem(
                    text = { Text(stringResource(R.string.composer_take_photo)) },
                    leadingIcon = { Icon(Icons.Outlined.PhotoCamera, contentDescription = null) },
                    onClick = {
                        menu = false
                        onTakePhoto()
                    },
                )
            }
        }
    }
}

/** The line under the composer that says what sending does (and what long press does). */
@Composable
private fun sendHint(send: SendState): String? = when {
    send.blocked == SendBlock.Uploading -> stringResource(R.string.composer_blocked_uploading)
    send.blocked == SendBlock.UploadFailed -> stringResource(R.string.composer_blocked_upload_failed)
    send.blocked == SendBlock.ImagesUnsupported -> stringResource(R.string.composer_blocked_images_unsupported)
    send.blocked == SendBlock.Archived -> stringResource(R.string.composer_blocked_archived)
    send.blocked == SendBlock.PermissionUnavailable -> stringResource(R.string.composer_blocked_permission_unavailable)
    send.blocked == SendBlock.Interrupting -> stringResource(R.string.composer_interrupting)
    send.primary == SendAction.Queue && send.alternate == SendAction.Steer -> stringResource(R.string.composer_hint_queue_or_steer)
    send.primary == SendAction.Steer && send.alternate == SendAction.Queue -> stringResource(R.string.composer_hint_steer_or_queue)
    send.primary == SendAction.Queue -> stringResource(R.string.composer_hint_queue)
    else -> null
}

@Composable
fun sendActionLabel(action: SendAction): String = stringResource(
    when (action) {
        SendAction.Start -> R.string.composer_send
        SendAction.Queue -> R.string.composer_queue
        SendAction.Steer -> R.string.composer_steer
        SendAction.Interrupt -> R.string.composer_stop
    },
)

/**
 * The send button: a Material 3 filled icon button (its colours, 40 dp, a 48 dp touch target)
 * drawn here because the M3 `FilledIconButton` has no long press, which runs the other delivery
 * ([SendState.alternate]). 停止 uses the error container colours; a disabled button the M3
 * disabled colours.
 */
@OptIn(ExperimentalFoundationApi::class)
@Composable
private fun SendButton(send: SendState, onSend: (SendAction) -> Unit) {
    val label = sendActionLabel(send.primary)
    val alternate = send.alternate
    val alternateLabel = alternate?.let { sendActionLabel(it) }
    val colors = if (send.primary == SendAction.Interrupt) {
        IconButtonDefaults.filledIconButtonColors(
            containerColor = MaterialTheme.colorScheme.errorContainer,
            contentColor = MaterialTheme.colorScheme.onErrorContainer,
        )
    } else {
        IconButtonDefaults.filledIconButtonColors()
    }
    val container = if (send.enabled) colors.containerColor else colors.disabledContainerColor
    val content = if (send.enabled) colors.contentColor else colors.disabledContentColor
    val icon = when (send.primary) {
        SendAction.Start -> Icons.AutoMirrored.Filled.Send
        SendAction.Queue -> Icons.Filled.AddToQueue
        SendAction.Steer -> Icons.Filled.Bolt
        SendAction.Interrupt -> Icons.Filled.Stop
    }
    Box(
        Modifier
            .minimumInteractiveComponentSize()
            .size(SEND_BUTTON_SIZE)
            .clip(CircleShape)
            .background(container)
            .combinedClickable(
                enabled = send.enabled,
                onClick = { onSend(send.primary) },
                onLongClick = alternate?.let { alt -> { onSend(alt) } },
            )
            .semantics {
                contentDescription = label
                role = Role.Button
                if (alternate != null && alternateLabel != null) {
                    onLongClick(alternateLabel) {
                        onSend(alternate)
                        true
                    }
                }
            }
            .testTag(ComposerTags.SEND),
        contentAlignment = Alignment.Center,
    ) {
        if (send.blocked == SendBlock.Interrupting || send.blocked == SendBlock.Uploading) {
            CircularProgressIndicator(Modifier.size(20.dp), color = content, strokeWidth = 2.dp)
        } else {
            Icon(icon, contentDescription = null, tint = content)
        }
    }
}

@Composable
private fun PalettePopup(state: ComposerUiState, onChoose: (PaletteEntry) -> Unit) {
    PopupSurface(ComposerTags.PALETTE) {
        val note = when (val commands = state.commands) {
            CommandsState.Loading -> stringResource(R.string.palette_loading)
            CommandsState.Offline -> stringResource(R.string.palette_offline)
            is CommandsState.Failed -> stringResource(R.string.palette_failed, commands.message.asString())
            else -> null
        }
        val listState = rememberListShowingTopOf(state.palette)
        LazyColumn(Modifier.heightIn(max = POPUP_MAX_HEIGHT), state = listState) {
            if (note != null) {
                item(key = "note") { PopupNote(note) }
            }
            if (state.palette.isEmpty() && state.commands !is CommandsState.Loading) {
                item(key = "empty") { PopupNote(stringResource(R.string.palette_empty)) }
            }
            items(state.palette, key = { "${it.source}/${it.name}" }) { entry ->
                Row(
                    Modifier.fillMaxWidth().clickable(enabled = entry.runnable) { onChoose(entry) }.padding(horizontal = 16.dp, vertical = 8.dp),
                    verticalAlignment = Alignment.CenterVertically,
                ) {
                    Column(Modifier.weight(1f)) {
                        Row(verticalAlignment = Alignment.CenterVertically) {
                            Text("/${entry.name}", style = MaterialTheme.codeStyle, color = if (entry.runnable) MaterialTheme.colorScheme.onSurface else MaterialTheme.colorScheme.onSurfaceVariant)
                            entry.argumentHint?.let {
                                Spacer(Modifier.width(6.dp))
                                Text(it.asString(), style = MaterialTheme.codeStyle, color = MaterialTheme.colorScheme.onSurfaceVariant)
                            }
                        }
                        val description = if (entry.runnable) entry.description?.asString() else stringResource(R.string.palette_unsupported)
                        description?.let { Text(it, style = MaterialTheme.typography.bodySmall, color = MaterialTheme.colorScheme.onSurfaceVariant, maxLines = 2, overflow = TextOverflow.Ellipsis) }
                    }
                    if (entry.source == PaletteSource.Harness) {
                        Text(stringResource(R.string.palette_source_harness), style = MaterialTheme.typography.labelSmall, color = MaterialTheme.colorScheme.onSurfaceVariant)
                    }
                }
            }
        }
    }
}

@Composable
private fun MentionPopup(search: MentionSearch, onChoose: (String) -> Unit) {
    PopupSurface(ComposerTags.MENTIONS) {
        when (search) {
            MentionSearch.Prompt -> PopupNote(stringResource(R.string.mention_prompt))
            MentionSearch.Loading -> PopupNote(stringResource(R.string.mention_loading))
            MentionSearch.Offline -> PopupNote(stringResource(R.string.mention_offline))
            is MentionSearch.Failed -> PopupNote(search.message.asString())
            is MentionSearch.Results -> if (search.results.isEmpty()) {
                PopupNote(stringResource(R.string.mention_empty))
            } else {
                val listState = rememberListShowingTopOf(search.results)
                LazyColumn(Modifier.heightIn(max = POPUP_MAX_HEIGHT), state = listState) {
                    items(search.results, key = { it.path }) { result ->
                        Row(
                            Modifier.fillMaxWidth().clickable { onChoose(result.path) }.padding(horizontal = 16.dp, vertical = 10.dp),
                            verticalAlignment = Alignment.CenterVertically,
                        ) {
                            Icon(
                                if (result.isDir) Icons.Outlined.Folder else Icons.Outlined.Description,
                                contentDescription = stringResource(if (result.isDir) R.string.mention_folder else R.string.mention_file),
                                tint = MaterialTheme.colorScheme.onSurfaceVariant,
                                modifier = Modifier.size(18.dp),
                            )
                            Spacer(Modifier.width(10.dp))
                            Text(result.path, style = MaterialTheme.codeStyle, maxLines = 1, overflow = TextOverflow.Ellipsis)
                        }
                    }
                }
            }
        }
    }
}

/**
 * The state of a ranked popup list (palette entries, mention results) that shows the top of
 * [entries] whenever they change. A keyed LazyColumn otherwise keeps its first visible item in
 * view: the `/` palette opened with the app's own commands (`/new` first) while `command/list`
 * loaded, and the daemon's commands, listed before them, ended up scrolled out of view above; a
 * narrower query could likewise hide its best match above the previous first row.
 */
@Composable
private fun rememberListShowingTopOf(entries: List<Any>): LazyListState {
    val state = rememberLazyListState()
    LaunchedEffect(entries) { state.scrollToItem(0) }
    return state
}

@Composable
private fun PopupSurface(tag: String, content: @Composable () -> Unit) {
    Surface(
        tonalElevation = 3.dp,
        shadowElevation = 2.dp,
        shape = RoundedCornerShape(12.dp),
        modifier = Modifier.fillMaxWidth().padding(horizontal = 8.dp, vertical = 4.dp).testTag(tag),
    ) {
        Column {
            content()
            HorizontalDivider(color = Color.Transparent)
        }
    }
}

@Composable
private fun PopupNote(text: String) {
    Text(text, style = MaterialTheme.typography.bodyMedium, color = MaterialTheme.colorScheme.onSurfaceVariant, modifier = Modifier.padding(horizontal = 16.dp, vertical = 12.dp))
}

@Composable
private fun AttachmentThumb(attachment: Attachment, onRemove: () -> Unit, onRetry: () -> Unit) {
    Box {
        val uploaded = attachment.state as? Attachment.State.Uploaded
        when {
            attachment.localUri != null -> LocalImage(attachment.localUri, THUMB_SIZE)
            // An image of a sent message put back: only the daemon has it.
            uploaded != null -> BlobImage(uploaded.image.blobId, THUMB_EDGE_PX, THUMB_SIZE)
        }
        when (attachment.state) {
            Attachment.State.Uploading -> Box(Modifier.size(THUMB_SIZE).background(Color.Black.copy(alpha = SCRIM_ALPHA)), contentAlignment = Alignment.Center) {
                CircularProgressIndicator(Modifier.size(20.dp), color = Color.White, strokeWidth = 2.dp)
            }
            is Attachment.State.Failed -> Box(
                Modifier.size(THUMB_SIZE).background(Color.Black.copy(alpha = SCRIM_ALPHA)).clickable(onClick = onRetry),
                contentAlignment = Alignment.Center,
            ) {
                Icon(Icons.Outlined.ErrorOutline, contentDescription = stringResource(R.string.upload_retry), tint = Color.White)
            }
            is Attachment.State.Uploaded -> Unit
        }
        Box(
            Modifier.align(Alignment.TopEnd).padding(2.dp).size(20.dp).clip(CircleShape).background(Color.Black.copy(alpha = SCRIM_ALPHA)).clickable(onClick = onRemove),
            contentAlignment = Alignment.Center,
        ) {
            Icon(Icons.Outlined.Close, contentDescription = stringResource(R.string.composer_remove_image), tint = Color.White, modifier = Modifier.size(14.dp))
        }
    }
}

/** A tappable chip of the composer's settings row. */
@Composable
fun ComposerChip(text: String, onClick: (() -> Unit)?, modifier: Modifier = Modifier, emphasized: Boolean = false) {
    Surface(
        shape = RoundedCornerShape(50),
        color = if (emphasized) MaterialTheme.statusColors.needsApproval.copy(alpha = CHIP_EMPHASIS_ALPHA) else MaterialTheme.colorScheme.surfaceContainerHighest,
        modifier = modifier.then(if (onClick != null) Modifier.clip(RoundedCornerShape(50)).clickable(onClick = onClick) else Modifier),
    ) {
        Text(text, style = MaterialTheme.typography.labelMedium, maxLines = 1, modifier = Modifier.padding(horizontal = 10.dp, vertical = 6.dp))
    }
}

/** Lines the input grows to before it scrolls. */
private const val INPUT_MAX_LINES = 8

/** Height of the palette and mention lists: about five rows, leaving the conversation visible. */
private val POPUP_MAX_HEIGHT = 260.dp

/** The Material 3 filled icon button's size. */
private val SEND_BUTTON_SIZE = 40.dp

/** Corners of the composer's container: the rounded look of a chat input (Material 3's large-to-extra-large range). */
private val CONTAINER_CORNER = 26.dp
private val THUMB_SIZE = 64.dp

/** Decoded size of a thumbnail from a blob: 64 dp at up to 3x density. */
private const val THUMB_EDGE_PX = 192
private const val SCRIM_ALPHA = 0.45f
private const val CHIP_EMPHASIS_ALPHA = 0.22f
