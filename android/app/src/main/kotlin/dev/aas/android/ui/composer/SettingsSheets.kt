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
import androidx.compose.foundation.selection.toggleable
import androidx.compose.foundation.verticalScroll
import androidx.compose.material3.Button
import androidx.compose.material3.ExperimentalMaterial3Api
import androidx.compose.material3.FilterChip
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.ModalBottomSheet
import androidx.compose.material3.RadioButton
import androidx.compose.material3.Switch
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
 * chosen model and effort (`null` effort: the harness default) and the fast-mode switch (`null`:
 * not offered for the chosen model, or no switch at all).
 *
 * [allowDefaultEffort]: "既定" can be chosen. `thread/update` only sets fields, it cannot go back
 * to the default, so an existing thread with an effort must pick one.
 *
 * [fastMode]: a thread's fast mode (`modes.fast`; `null`: no switch, e.g. before the thread
 * exists). The switch shows only for a model the harness lists in `features.fastModeModels`;
 * [fastModeState] is what the harness last reported about it, verbatim.
 *
 * [initialModel]: the model chosen when the sheet opens (`/model <id>` whose model does not offer
 * the effort in effect, or does not run in the permission mode in effect), as if the user had
 * chosen it: an effort it does not offer is dropped.
 *
 * A chosen model that does not run in the permission mode in effect (`Model.permissionModes`,
 * protocol.md §3.1; Claude Code's Haiku has no auto mode) needs one it runs in: the sheet lists
 * that model's modes and applies only once one is chosen, with the model in the same request
 * ([ModelChoice.permissionMode]; the daemon refuses a model alone that the mode does not fit).
 * Modes other than the harness default ask first, as in [PermissionSheet].
 */
@OptIn(ExperimentalMaterial3Api::class, ExperimentalLayoutApi::class)
@Composable
fun ModelSheet(
    harness: Harness,
    settings: ThreadSettings,
    allowDefaultEffort: Boolean,
    onApply: (ModelChoice) -> Unit,
    onDismiss: () -> Unit,
    fastMode: Boolean? = null,
    fastModeState: String? = null,
    initialModel: String? = null,
) {
    val current = HarnessSettings.model(harness, settings)
    var model by rememberSaveable { mutableStateOf(initialModel ?: current?.id) }
    // An effort the chosen model does not offer is dropped (the harness default applies).
    var effort by rememberSaveable { mutableStateOf(settings.effort?.takeIf { e -> initialModel == null || HarnessSettings.effortLevels(harness, initialModel).any { it.id == e } }) }
    var fast by rememberSaveable { mutableStateOf(fastMode ?: false) }
    // The mode chosen for a model that does not run in the one in effect.
    var permission by rememberSaveable { mutableStateOf<String?>(null) }
    var confirmPermission by rememberSaveable { mutableStateOf<String?>(null) }
    val fastOffered = fastMode != null && model != null && model in harness.features.fastModeModels
    val levels = HarnessSettings.effortLevels(harness, model)
    val modeInEffect = HarnessSettings.permission(harness, settings)
    val needsPermission = !HarnessSettings.offersPermission(harness, model, settings)
    val modelModes = HarnessSettings.permissionModes(harness, model)
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
                            // A mode chosen for another model may not fit this one.
                            if (permission != null && HarnessSettings.permissionModes(harness, m.id).none { it.id == permission }) permission = null
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
            if (fastOffered) {
                Row(
                    Modifier.fillMaxWidth().toggleable(value = fast, role = Role.Switch) { fast = it }.padding(horizontal = 16.dp, vertical = 8.dp),
                    verticalAlignment = Alignment.CenterVertically,
                ) {
                    Column(Modifier.weight(1f)) {
                        Text(stringResource(R.string.picker_fast), style = MaterialTheme.typography.bodyLarge)
                        Text(stringResource(R.string.picker_fast_body), style = MaterialTheme.typography.bodySmall, color = MaterialTheme.colorScheme.onSurfaceVariant)
                        fastModeState?.let { Text(stringResource(R.string.picker_fast_state, it), style = MaterialTheme.typography.labelSmall, color = MaterialTheme.colorScheme.onSurfaceVariant) }
                    }
                    Switch(checked = fast, onCheckedChange = null)
                }
            }
            if (needsPermission) {
                SectionHeader(stringResource(R.string.picker_permission))
                val modelName = harness.models.firstOrNull { it.id == model }?.displayName ?: model.orEmpty()
                Text(
                    stringResource(R.string.picker_permission_unavailable_for_model, modelName, modeInEffect?.label ?: settings.permissionMode.orEmpty()),
                    style = MaterialTheme.typography.bodySmall,
                    color = MaterialTheme.colorScheme.onSurfaceVariant,
                    modifier = Modifier.padding(horizontal = 16.dp, vertical = 4.dp),
                )
                for (mode in modelModes) {
                    Row(
                        Modifier.fillMaxWidth().selectable(selected = permission == mode.id, role = Role.RadioButton) {
                            if (HarnessSettings.needsConfirmation(harness, mode)) confirmPermission = mode.id else permission = mode.id
                        }.padding(horizontal = 16.dp, vertical = 8.dp),
                        verticalAlignment = Alignment.CenterVertically,
                    ) {
                        RadioButton(selected = permission == mode.id, onClick = null)
                        Column(Modifier.padding(start = 12.dp)) {
                            Text(if (mode.isDefault) stringResource(R.string.picker_default_suffix, mode.label) else mode.label, style = MaterialTheme.typography.bodyLarge)
                            mode.description?.let { Text(it, style = MaterialTheme.typography.bodySmall, color = MaterialTheme.colorScheme.onSurfaceVariant) }
                        }
                    }
                }
            }
            val effortMissing = levels.isNotEmpty() && effort == null && !allowDefaultEffort
            if (effortMissing) {
                Text(stringResource(R.string.picker_effort_required), style = MaterialTheme.typography.bodySmall, color = MaterialTheme.colorScheme.error, modifier = Modifier.padding(horizontal = 16.dp, vertical = 4.dp))
            }
            val permissionMissing = needsPermission && permission == null
            if (permissionMissing) {
                Text(stringResource(R.string.picker_permission_required), style = MaterialTheme.typography.bodySmall, color = MaterialTheme.colorScheme.error, modifier = Modifier.padding(horizontal = 16.dp, vertical = 4.dp))
            }
            Spacer(Modifier.height(16.dp))
            Row(Modifier.fillMaxWidth().padding(horizontal = 16.dp), horizontalArrangement = Arrangement.End) {
                TextButton(onClick = onDismiss) { Text(stringResource(R.string.cancel)) }
                val fastChoice = if (fastOffered) fast else null
                Button(
                    onClick = {
                        onApply(ModelChoice(model, effort, fastChoice, permission.takeIf { needsPermission }))
                        onDismiss()
                    },
                    enabled = (model != current?.id || effort != settings.effort || (fastChoice != null && fastChoice != fastMode) || needsPermission) &&
                        !effortMissing && !permissionMissing,
                ) { Text(stringResource(R.string.picker_apply)) }
            }
            Spacer(Modifier.height(16.dp))
        }
    }
    val pending = confirmPermission?.let { id -> modelModes.firstOrNull { it.id == id } }
    if (pending != null) {
        ConfirmDialog(
            title = stringResource(R.string.picker_permission_confirm_title, pending.label),
            text = pending.description ?: stringResource(R.string.picker_permission_confirm_body),
            confirm = stringResource(R.string.picker_permission_confirm),
            onConfirm = {
                confirmPermission = null
                permission = pending.id
            },
            onDismiss = { confirmPermission = null },
        )
    }
}

/**
 * What [ModelSheet] applies: the model, its effort (`null`: the harness default), the fast-mode
 * switch (`null`: not offered) and, only when the model does not run in the permission mode in
 * effect, the mode chosen for it (sent with the model in one request).
 */
data class ModelChoice(val model: String?, val effort: String?, val fast: Boolean?, val permissionMode: String?)

/**
 * The permission-mode picker (§2.4 権限): the `permissionModes` the model in effect runs in
 * (`Model.permissionModes`; all of the harness's without such a list), with their descriptions.
 * The ones the model does not run in are named, not offered (the daemon would refuse them).
 * Choosing a mode other than the harness default asks first (the protocol gives no risk level,
 * so every non-default mode is confirmed with its description).
 */
@OptIn(ExperimentalMaterial3Api::class)
@Composable
fun PermissionSheet(harness: Harness, settings: ThreadSettings, onApply: (PermissionMode) -> Unit, onDismiss: () -> Unit) {
    val current = HarnessSettings.permission(harness, settings)
    val model = HarnessSettings.model(harness, settings)
    val modes = HarnessSettings.permissionModes(harness, model?.id)
    val unavailable = harness.permissionModes.filter { mode -> modes.none { it.id == mode.id } }
    var confirm by rememberSaveable { mutableStateOf<String?>(null) }
    ModalBottomSheet(onDismissRequest = onDismiss, sheetState = rememberModalBottomSheetState(skipPartiallyExpanded = true)) {
        Column(Modifier.fillMaxWidth().navigationBarsPadding().verticalScroll(rememberScrollState())) {
            Text(stringResource(R.string.picker_permission_title), style = MaterialTheme.typography.titleLarge, modifier = Modifier.padding(horizontal = 24.dp))
            Spacer(Modifier.height(8.dp))
            if (model != null && unavailable.isNotEmpty()) {
                Text(
                    stringResource(R.string.picker_permission_model_limits, model.displayName, unavailable.joinToString("、") { it.label }),
                    style = MaterialTheme.typography.bodySmall,
                    color = MaterialTheme.colorScheme.onSurfaceVariant,
                    modifier = Modifier.padding(horizontal = 24.dp, vertical = 4.dp),
                )
            }
            for (mode in modes) {
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
