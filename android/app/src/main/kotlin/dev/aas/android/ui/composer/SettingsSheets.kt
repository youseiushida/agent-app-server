package dev.aas.android.ui.composer

import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.ExperimentalLayoutApi
import androidx.compose.foundation.layout.FlowRow
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.navigationBarsPadding
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.selection.selectable
import androidx.compose.foundation.verticalScroll
import androidx.compose.material3.Button
import androidx.compose.material3.ExperimentalMaterial3Api
import androidx.compose.material3.FilterChip
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.ModalBottomSheet
import androidx.compose.material3.RadioButton
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.material3.rememberModalBottomSheetState
import androidx.compose.runtime.Composable
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.saveable.rememberSaveable
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.res.stringResource
import androidx.compose.ui.semantics.Role
import androidx.compose.ui.unit.dp
import dev.aas.android.R
import dev.aas.android.domain.composer.HarnessSettings
import dev.aas.android.protocol.Harness
import dev.aas.android.protocol.PermissionMode
import dev.aas.android.protocol.ThreadSettings
import dev.aas.android.ui.components.ConfirmDialog
import dev.aas.android.ui.components.SectionHeader

/**
 * The model and reasoning-effort picker (docs/ux/codex-desktop.md §2.4 モデルと推論, §8.1): the
 * harness's `models`, and the `effortLevels` the chosen model allows. [onApply] receives the
 * chosen model and effort (`null` effort: the harness default).
 *
 * [allowDefaultEffort]: "既定" can be chosen. `thread/update` only sets fields, it cannot go back
 * to the default, so an existing thread with an effort must pick one.
 */
@OptIn(ExperimentalMaterial3Api::class, ExperimentalLayoutApi::class)
@Composable
fun ModelSheet(
    harness: Harness,
    settings: ThreadSettings,
    allowDefaultEffort: Boolean,
    onApply: (model: String?, effort: String?) -> Unit,
    onDismiss: () -> Unit,
) {
    val current = HarnessSettings.model(harness, settings)
    var model by rememberSaveable { mutableStateOf(current?.id) }
    var effort by rememberSaveable { mutableStateOf(settings.effort) }
    val levels = HarnessSettings.effortLevels(harness, model)
    ModalBottomSheet(onDismissRequest = onDismiss, sheetState = rememberModalBottomSheetState(skipPartiallyExpanded = true)) {
        Column(Modifier.fillMaxWidth().navigationBarsPadding().verticalScroll(rememberScrollState())) {
            Text(stringResource(R.string.picker_model_title, harness.displayName), style = MaterialTheme.typography.titleLarge, modifier = Modifier.padding(horizontal = 24.dp))
            if (harness.models.isNotEmpty()) {
                SectionHeader(stringResource(R.string.picker_model))
                for (m in harness.models) {
                    Row(
                        Modifier.fillMaxWidth().selectable(selected = model == m.id, role = Role.RadioButton) {
                            model = m.id
                            // An effort the new model does not offer is dropped (the harness default applies).
                            if (effort != null && HarnessSettings.effortLevels(harness, m.id).none { it.id == effort }) effort = null
                        }.padding(horizontal = 16.dp, vertical = 8.dp),
                        verticalAlignment = Alignment.CenterVertically,
                    ) {
                        RadioButton(selected = model == m.id, onClick = null)
                        Column(Modifier.padding(start = 12.dp)) {
                            Text(if (m.isDefault) stringResource(R.string.picker_default_suffix, m.displayName) else m.displayName, style = MaterialTheme.typography.bodyLarge)
                            m.description?.let { Text(it, style = MaterialTheme.typography.bodySmall, color = MaterialTheme.colorScheme.onSurfaceVariant) }
                        }
                    }
                }
            }
            if (levels.isNotEmpty()) {
                SectionHeader(stringResource(R.string.picker_effort))
                FlowRow(Modifier.padding(horizontal = 16.dp), horizontalArrangement = Arrangement.spacedBy(8.dp)) {
                    if (allowDefaultEffort) FilterChip(selected = effort == null, onClick = { effort = null }, label = { Text(stringResource(R.string.picker_effort_default)) })
                    for (level in levels) {
                        FilterChip(selected = effort == level.id, onClick = { effort = level.id }, label = { Text(level.label) })
                    }
                }
            }
            val effortMissing = levels.isNotEmpty() && effort == null && !allowDefaultEffort
            if (effortMissing) {
                Text(stringResource(R.string.picker_effort_required), style = MaterialTheme.typography.bodySmall, color = MaterialTheme.colorScheme.error, modifier = Modifier.padding(horizontal = 16.dp, vertical = 4.dp))
            }
            Spacer(Modifier.height(16.dp))
            Row(Modifier.fillMaxWidth().padding(horizontal = 16.dp), horizontalArrangement = Arrangement.End) {
                TextButton(onClick = onDismiss) { Text(stringResource(R.string.cancel)) }
                Button(
                    onClick = {
                        onApply(model, effort)
                        onDismiss()
                    },
                    enabled = (model != current?.id || effort != settings.effort) && !effortMissing,
                ) { Text(stringResource(R.string.picker_apply)) }
            }
            Spacer(Modifier.height(16.dp))
        }
    }
}

/**
 * The permission-mode picker (§2.4 権限): the harness's `permissionModes` with their
 * descriptions. Choosing a mode other than the harness default asks first (the protocol gives
 * no risk level, so every non-default mode is confirmed with its description).
 */
@OptIn(ExperimentalMaterial3Api::class)
@Composable
fun PermissionSheet(harness: Harness, settings: ThreadSettings, onApply: (PermissionMode) -> Unit, onDismiss: () -> Unit) {
    val current = HarnessSettings.permission(harness, settings)
    var confirm by rememberSaveable { mutableStateOf<String?>(null) }
    ModalBottomSheet(onDismissRequest = onDismiss, sheetState = rememberModalBottomSheetState(skipPartiallyExpanded = true)) {
        Column(Modifier.fillMaxWidth().navigationBarsPadding().verticalScroll(rememberScrollState())) {
            Text(stringResource(R.string.picker_permission_title), style = MaterialTheme.typography.titleLarge, modifier = Modifier.padding(horizontal = 24.dp))
            Spacer(Modifier.height(8.dp))
            for (mode in harness.permissionModes) {
                Row(
                    Modifier.fillMaxWidth().selectable(selected = current?.id == mode.id, role = Role.RadioButton) {
                        if (mode.id == current?.id) {
                            onDismiss()
                        } else if (HarnessSettings.needsConfirmation(harness, mode)) {
                            confirm = mode.id
                        } else {
                            onApply(mode)
                            onDismiss()
                        }
                    }.padding(horizontal = 16.dp, vertical = 10.dp),
                    verticalAlignment = Alignment.CenterVertically,
                ) {
                    RadioButton(selected = current?.id == mode.id, onClick = null)
                    Column(Modifier.padding(start = 12.dp)) {
                        Text(if (mode.isDefault) stringResource(R.string.picker_default_suffix, mode.label) else mode.label, style = MaterialTheme.typography.bodyLarge)
                        mode.description?.let { Text(it, style = MaterialTheme.typography.bodySmall, color = MaterialTheme.colorScheme.onSurfaceVariant) }
                    }
                }
            }
            Spacer(Modifier.height(24.dp))
        }
    }
    val pending = confirm?.let { id -> harness.permissionModes.firstOrNull { it.id == id } }
    if (pending != null) {
        ConfirmDialog(
            title = stringResource(R.string.picker_permission_confirm_title, pending.label),
            text = pending.description ?: stringResource(R.string.picker_permission_confirm_body),
            confirm = stringResource(R.string.picker_permission_confirm),
            onConfirm = {
                confirm = null
                onApply(pending)
                onDismiss()
            },
            onDismiss = { confirm = null },
        )
    }
}
