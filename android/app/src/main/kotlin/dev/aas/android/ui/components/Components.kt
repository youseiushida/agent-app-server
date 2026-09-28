package dev.aas.android.ui.components

import androidx.compose.foundation.background
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.shape.CircleShape
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.material3.AlertDialog
import androidx.compose.material3.Button
import androidx.compose.material3.ExtendedFloatingActionButton
import androidx.compose.material3.Icon
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Surface
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.runtime.Composable
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableLongStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.graphics.vector.ImageVector
import androidx.compose.ui.res.stringResource
import androidx.compose.ui.semantics.contentDescription
import androidx.compose.ui.semantics.semantics
import androidx.compose.ui.text.style.TextAlign
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.unit.dp
import dev.aas.android.R
import dev.aas.android.domain.ThreadActivity
import dev.aas.android.service.ConnectionTexts
import dev.aas.android.ui.common.LocalAppPolicy
import dev.aas.android.ui.theme.statusColors
import kotlinx.coroutines.delay

/**
 * An extended floating action button whose label is also its accessible name. Material 3 clears
 * the semantics of an extended FAB's label (it animates the label in and out), which leaves the
 * button without a name: TalkBack announces an unlabelled button and UI Automator cannot find it.
 * The label is therefore set as the button's content description as well.
 */
@Composable
fun LabeledExtendedFab(text: String, icon: ImageVector, onClick: () -> Unit, modifier: Modifier = Modifier) {
    ExtendedFloatingActionButton(
        onClick = onClick,
        icon = { Icon(icon, contentDescription = null) },
        text = { Text(text) },
        modifier = modifier.semantics { contentDescription = text },
    )
}

/** The colour of a [ThreadActivity] (shared by chips, dots and notifications' accents). */
@Composable
fun ThreadActivity.color(): Color {
    val colors = MaterialTheme.statusColors
    return when (this) {
        ThreadActivity.NeedsApproval -> colors.needsApproval
        ThreadActivity.NeedsInput -> colors.needsInput
        ThreadActivity.Error -> colors.error
        ThreadActivity.Running, ThreadActivity.Background -> colors.running
        ThreadActivity.Idle -> colors.idle
    }
}

/**
 * The words of a [ThreadActivity] (実行中 / 承認が必要 / 入力が必要 / エラー /
 * バックグラウンドで実行中 (N) / 待機中). [backgroundRunning] is N, the running background tasks
 * (`Thread.background.running`, or their sum over a project's threads); it is shown when positive.
 */
@Composable
fun ThreadActivity.label(backgroundRunning: Int = 0): String = when (this) {
    ThreadActivity.NeedsApproval -> stringResource(R.string.activity_needs_approval)
    ThreadActivity.NeedsInput -> stringResource(R.string.activity_needs_input)
    ThreadActivity.Error -> stringResource(R.string.activity_error)
    ThreadActivity.Running -> stringResource(R.string.activity_running)
    ThreadActivity.Background ->
        if (backgroundRunning > 0) stringResource(R.string.activity_background_count, backgroundRunning) else stringResource(R.string.activity_background)
    ThreadActivity.Idle -> stringResource(R.string.activity_idle)
}

/** A small tinted chip with the status words ([backgroundRunning]: see [label]). */
@Composable
fun ThreadActivityChip(activity: ThreadActivity, modifier: Modifier = Modifier, backgroundRunning: Int = 0) {
    val color = activity.color()
    Row(
        modifier = modifier
            .background(color.copy(alpha = CHIP_BACKGROUND_ALPHA), RoundedCornerShape(50))
            .padding(horizontal = 8.dp, vertical = 2.dp),
        verticalAlignment = Alignment.CenterVertically,
    ) {
        StatusDot(color, size = 6)
        Spacer(Modifier.size(4.dp))
        Text(activity.label(backgroundRunning), style = MaterialTheme.typography.labelSmall, color = color, maxLines = 1)
    }
}

/** A filled circle. */
@Composable
fun StatusDot(color: Color, modifier: Modifier = Modifier, size: Int = 8) {
    Box(modifier.size(size.dp).background(color, CircleShape))
}

/** The unread marker of list rows. */
@Composable
fun UnreadDot(unread: Boolean, modifier: Modifier = Modifier) {
    val description = stringResource(R.string.unread)
    Box(modifier.size(10.dp), contentAlignment = Alignment.Center) {
        if (unread) StatusDot(MaterialTheme.statusColors.unread, Modifier.semantics { contentDescription = description }, size = 10)
    }
}

/** A centred message for empty lists (UX §7.3). */
@Composable
fun EmptyState(
    icon: ImageVector,
    title: String,
    modifier: Modifier = Modifier,
    body: String? = null,
    action: String? = null,
    onAction: (() -> Unit)? = null,
) {
    Column(
        modifier = modifier.fillMaxSize().padding(32.dp),
        verticalArrangement = Arrangement.Center,
        horizontalAlignment = Alignment.CenterHorizontally,
    ) {
        Icon(icon, contentDescription = null, modifier = Modifier.size(48.dp), tint = MaterialTheme.colorScheme.onSurfaceVariant)
        Spacer(Modifier.height(16.dp))
        Text(title, style = MaterialTheme.typography.titleMedium, textAlign = TextAlign.Center)
        if (body != null) {
            Spacer(Modifier.height(8.dp))
            Text(body, style = MaterialTheme.typography.bodyMedium, color = MaterialTheme.colorScheme.onSurfaceVariant, textAlign = TextAlign.Center)
        }
        if (action != null && onAction != null) {
            Spacer(Modifier.height(16.dp))
            Button(onClick = onAction) { Text(action) }
        }
    }
}

/** A section title in lists. */
@Composable
fun SectionHeader(text: String, modifier: Modifier = Modifier, trailing: (@Composable () -> Unit)? = null) {
    Row(
        modifier = modifier.fillMaxWidth().padding(start = 16.dp, end = 8.dp, top = 16.dp, bottom = 4.dp),
        verticalAlignment = Alignment.CenterVertically,
    ) {
        Text(
            text,
            style = MaterialTheme.typography.titleSmall,
            color = MaterialTheme.colorScheme.primary,
            modifier = Modifier.weight(1f),
            maxLines = 1,
            overflow = TextOverflow.Ellipsis,
        )
        trailing?.invoke()
    }
}

/** A tinted box for a message that needs the user (connection problems, pairing). */
@Composable
fun Banner(
    text: String,
    color: Color,
    modifier: Modifier = Modifier,
    action: String? = null,
    onAction: (() -> Unit)? = null,
) {
    Surface(color = color.copy(alpha = BANNER_BACKGROUND_ALPHA), modifier = modifier.fillMaxWidth()) {
        Row(Modifier.padding(horizontal = 16.dp, vertical = 8.dp), verticalAlignment = Alignment.CenterVertically) {
            Text(text, style = MaterialTheme.typography.bodyMedium, modifier = Modifier.weight(1f))
            if (action != null && onAction != null) TextButton(onClick = onAction) { Text(action) }
        }
    }
}

/** A yes/no dialog; [extra] adds content under the text (e.g. the background work a stop takes along). */
@Composable
fun ConfirmDialog(
    title: String,
    text: String,
    confirm: String,
    onConfirm: () -> Unit,
    onDismiss: () -> Unit,
    dismiss: String = stringResource(R.string.cancel),
    extra: (@Composable () -> Unit)? = null,
) {
    AlertDialog(
        onDismissRequest = onDismiss,
        title = { Text(title) },
        text = {
            if (extra == null) {
                Text(text)
            } else {
                Column {
                    Text(text)
                    Spacer(Modifier.size(8.dp))
                    extra()
                }
            }
        },
        confirmButton = { TextButton(onClick = onConfirm) { Text(confirm) } },
        dismissButton = { TextButton(onClick = onDismiss) { Text(dismiss) } },
    )
}

/**
 * The current time, refreshed every [intervalMs] (by default the policy's interval for relative
 * times such as "3 分前", `DisplayPolicy.relativeTimeRefreshMs`).
 */
@Composable
fun rememberNow(intervalMs: Long = LocalAppPolicy.current.display.relativeTimeRefreshMs): Long {
    var now by remember { mutableLongStateOf(System.currentTimeMillis()) }
    LaunchedEffect(intervalMs) {
        while (true) {
            delay(intervalMs)
            now = System.currentTimeMillis()
        }
    }
    return now
}

/** "3 分前" for [atMs]. */
@Composable
fun relativeTime(atMs: Long): String = ConnectionTexts.relative(atMs, rememberNow())

private const val CHIP_BACKGROUND_ALPHA = 0.14f
private const val BANNER_BACKGROUND_ALPHA = 0.16f
