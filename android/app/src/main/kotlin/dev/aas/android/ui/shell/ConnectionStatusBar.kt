package dev.aas.android.ui.shell

import androidx.compose.foundation.clickable
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.width
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Surface
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.runtime.Composable
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.platform.LocalResources
import androidx.compose.ui.res.stringResource
import androidx.compose.ui.semantics.LiveRegionMode
import androidx.compose.ui.semantics.liveRegion
import androidx.compose.ui.semantics.semantics
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.unit.dp
import dev.aas.android.R
import dev.aas.android.service.ConnectionPresentation
import dev.aas.android.service.ConnectionSummary
import dev.aas.android.service.ConnectionTexts
import dev.aas.android.service.PairingNeed
import dev.aas.android.ui.components.Banner
import dev.aas.android.ui.components.StatusDot
import dev.aas.android.ui.components.rememberNow
import dev.aas.android.ui.theme.statusColors

/** The colour of the connection dot. */
@Composable
fun ConnectionSummary.color(): Color {
    val colors = MaterialTheme.statusColors
    return when (this) {
        is ConnectionSummary.Connected -> colors.connected
        is ConnectionSummary.Connecting, is ConnectionSummary.WaitingToRetry -> colors.working
        ConnectionSummary.PhoneOffline, ConnectionSummary.Stopped -> colors.offline
        ConnectionSummary.ConnectedElsewhere, is ConnectionSummary.NeedsPairing, is ConnectionSummary.Incompatible -> colors.error
        is ConnectionSummary.KeystoreUnavailable -> colors.working
    }
}

/**
 * The one-line connection state under the system bar: a dot, the state, and (when not
 * connected) the last sync time, the requests waiting in the outbox and a 再接続 button. Tapping
 * opens the diagnostics.
 */
@Composable
fun ConnectionStatusBar(
    presentation: ConnectionPresentation,
    onReconnect: () -> Unit,
    onOpenDiagnostics: () -> Unit,
    modifier: Modifier = Modifier,
) {
    val res = LocalResources.current
    val text = ConnectionTexts.describe(res, presentation, rememberNow())
    val summary = presentation.summary
    Surface(color = MaterialTheme.colorScheme.surfaceContainer, modifier = modifier.fillMaxWidth()) {
        Row(
            modifier = Modifier
                .clickable(onClickLabel = stringResource(R.string.open_diagnostics), onClick = onOpenDiagnostics)
                .padding(start = 16.dp, end = 4.dp)
                .semantics { liveRegion = LiveRegionMode.Polite },
            verticalAlignment = Alignment.CenterVertically,
        ) {
            StatusDot(summary.color())
            Spacer(Modifier.width(8.dp))
            Column(Modifier.weight(1f).padding(vertical = 6.dp)) {
                Text(text.title, style = MaterialTheme.typography.labelLarge, maxLines = 1, overflow = TextOverflow.Ellipsis)
                if (text.detail != null) {
                    Text(
                        text.detail,
                        style = MaterialTheme.typography.labelSmall,
                        color = MaterialTheme.colorScheme.onSurfaceVariant,
                        maxLines = 1,
                        overflow = TextOverflow.Ellipsis,
                    )
                }
            }
            val retryable = summary is ConnectionSummary.WaitingToRetry || summary is ConnectionSummary.Connecting
            if (retryable) TextButton(onClick = onReconnect) { Text(stringResource(R.string.action_reconnect)) }
        }
    }
}

/**
 * The banner for states the user must act on: connected elsewhere (4000), pairing needed
 * (4001, token rejected, unreadable token), incompatible server.
 */
@Composable
fun ConnectionAttentionBanner(
    presentation: ConnectionPresentation,
    tokenUnreadable: Boolean,
    onReconnect: () -> Unit,
    onRepair: () -> Unit,
) {
    val errorColor = MaterialTheme.statusColors.error
    if (tokenUnreadable) {
        // The engine has no credentials then (and the service stopped): only pairing again helps.
        Banner(
            text = stringResource(R.string.banner_token_unreadable),
            color = errorColor,
            action = stringResource(R.string.action_pair_again),
            onAction = onRepair,
        )
        return
    }
    when (val summary = presentation.summary) {
        ConnectionSummary.ConnectedElsewhere -> Banner(
            text = stringResource(R.string.banner_connected_elsewhere),
            color = errorColor,
            action = stringResource(R.string.action_connect_here),
            onAction = onReconnect,
        )
        is ConnectionSummary.NeedsPairing -> Banner(
            text = stringResource(
                when (summary.reason) {
                    PairingNeed.Revoked -> R.string.banner_revoked
                    PairingNeed.TokenRejected -> R.string.banner_token_rejected
                    PairingNeed.InvalidServerUrl -> R.string.banner_invalid_url
                    PairingNeed.NotPaired -> R.string.banner_not_paired
                },
            ),
            color = errorColor,
            action = stringResource(R.string.action_pair_again),
            onAction = onRepair,
        )
        is ConnectionSummary.Incompatible -> Banner(
            text = stringResource(R.string.banner_incompatible, summary.message),
            color = errorColor,
            action = stringResource(R.string.action_retry),
            onAction = onReconnect,
        )
        // Transient: the app retries by itself; the button retries at once.
        is ConnectionSummary.KeystoreUnavailable -> Banner(
            text = stringResource(R.string.banner_keystore_unavailable),
            color = MaterialTheme.statusColors.working,
            action = stringResource(R.string.action_retry),
            onAction = onReconnect,
        )
        else -> Unit
    }
}
