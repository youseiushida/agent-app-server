package dev.aas.android.ui.components

import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.outlined.Refresh
import androidx.compose.material3.CircularProgressIndicator
import androidx.compose.material3.Icon
import androidx.compose.material3.IconButton
import androidx.compose.material3.ListItem
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Surface
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.runtime.Composable
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.res.stringResource
import androidx.compose.ui.semantics.contentDescription
import androidx.compose.ui.semantics.semantics
import androidx.compose.ui.unit.dp
import dev.aas.android.R
import dev.aas.android.domain.HarnessState
import dev.aas.android.domain.HarnessWait
import dev.aas.android.protocol.Harness
import dev.aas.android.ui.theme.statusColors

/** 使えます / 使えません: <reason> / 確認しています… */
@Composable
fun HarnessState.label(): String = when (this) {
    HarnessState.Available -> stringResource(R.string.harness_state_available)
    is HarnessState.Unavailable -> reason?.let { stringResource(R.string.harness_state_unavailable, it) } ?: stringResource(R.string.harness_state_unavailable_no_reason)
    HarnessState.Probing -> stringResource(R.string.harness_state_probing)
}

@Composable
fun HarnessState.color(): Color = when (this) {
    HarnessState.Available -> MaterialTheme.statusColors.connected
    is HarnessState.Unavailable -> MaterialTheme.statusColors.error
    HarnessState.Probing -> MaterialTheme.statusColors.working
}

/** The 再確認 button of one harness, or a spinner while its probe runs. */
@Composable
fun HarnessRefreshButton(name: String, probing: Boolean, onRefresh: () -> Unit) {
    if (probing) {
        CircularProgressIndicator(Modifier.padding(12.dp).size(24.dp), strokeWidth = 2.dp)
    } else {
        val description = stringResource(R.string.harness_refresh_one, name)
        IconButton(onClick = onRefresh, modifier = Modifier.semantics { contentDescription = description }) {
            Icon(Icons.Outlined.Refresh, contentDescription = null)
        }
    }
}

/** One harness with its version and state, and 再確認 (settings). */
@Composable
fun HarnessStatusRow(harness: Harness, state: HarnessState, onRefresh: () -> Unit) {
    ListItem(
        headlineContent = { Text(harness.displayName) },
        supportingContent = {
            Column {
                Row(verticalAlignment = Alignment.CenterVertically) {
                    StatusDot(state.color(), size = 8)
                    Text(state.label(), style = MaterialTheme.typography.bodySmall, modifier = Modifier.padding(start = 6.dp))
                }
                harness.version?.let { Text(stringResource(R.string.harness_version, it), style = MaterialTheme.typography.labelSmall, color = MaterialTheme.colorScheme.onSurfaceVariant) }
            }
        },
        trailingContent = { HarnessRefreshButton(harness.displayName, state == HarnessState.Probing, onRefresh) },
    )
}

/**
 * A request held because its harness cannot be used (the server's `harnessUnavailable`): which
 * harness, why, that it is sent by itself once the harness is back, and the ways out: probe
 * again, or withdraw the request.
 */
@Composable
fun HarnessWaitNotice(wait: HarnessWait, probing: Boolean, onRefresh: () -> Unit, onDiscard: () -> Unit, modifier: Modifier = Modifier) {
    Surface(
        color = MaterialTheme.statusColors.needsApproval.copy(alpha = NOTICE_ALPHA),
        shape = RoundedCornerShape(12.dp),
        modifier = modifier.fillMaxWidth(),
    ) {
        Column(Modifier.padding(horizontal = 14.dp, vertical = 10.dp), verticalArrangement = Arrangement.spacedBy(4.dp)) {
            Text(stringResource(R.string.harness_wait_title, wait.harnessName), style = MaterialTheme.typography.titleSmall)
            wait.reason?.let { Text(stringResource(R.string.harness_wait_reason, it), style = MaterialTheme.typography.bodySmall) }
            Text(stringResource(R.string.harness_wait_note), style = MaterialTheme.typography.bodySmall, color = MaterialTheme.colorScheme.onSurfaceVariant)
            Row(Modifier.fillMaxWidth(), horizontalArrangement = Arrangement.End, verticalAlignment = Alignment.CenterVertically) {
                if (probing) {
                    CircularProgressIndicator(Modifier.size(18.dp), strokeWidth = 2.dp)
                    Text(stringResource(R.string.harness_state_probing), style = MaterialTheme.typography.labelMedium, modifier = Modifier.padding(horizontal = 8.dp))
                } else {
                    TextButton(onClick = onRefresh) { Text(stringResource(R.string.harness_refresh)) }
                }
                TextButton(onClick = onDiscard) { Text(stringResource(R.string.pending_discard)) }
            }
        }
    }
}

private const val NOTICE_ALPHA = 0.14f
