package dev.aas.android.ui.components

import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.material3.AlertDialog
import androidx.compose.material3.OutlinedTextField
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.runtime.Composable
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.saveable.rememberSaveable
import androidx.compose.runtime.setValue
import androidx.compose.ui.Modifier
import androidx.compose.ui.res.stringResource
import dev.aas.android.R

/**
 * A dialog with one text field (renaming, editing a queued message, a diff comment).
 * [validate] returns the problem to show, or `null` when the text can be confirmed.
 */
@Composable
fun TextInputDialog(
    title: String,
    initial: String,
    confirm: String,
    onConfirm: (String) -> Unit,
    onDismiss: () -> Unit,
    label: String? = null,
    singleLine: Boolean = true,
    validate: (String) -> String? = { if (it.isBlank()) "" else null },
) {
    var text by rememberSaveable { mutableStateOf(initial) }
    val problem = validate(text)
    AlertDialog(
        onDismissRequest = onDismiss,
        title = { Text(title) },
        text = {
            OutlinedTextField(
                value = text,
                onValueChange = { text = it },
                label = label?.let { { Text(it) } },
                singleLine = singleLine,
                minLines = if (singleLine) 1 else MULTILINE_MIN_LINES,
                isError = !problem.isNullOrEmpty(),
                supportingText = problem?.takeIf { it.isNotEmpty() }?.let { { Text(it) } },
                modifier = Modifier.fillMaxWidth(),
            )
        },
        confirmButton = { TextButton(onClick = { onConfirm(text) }, enabled = problem == null) { Text(confirm) } },
        dismissButton = { TextButton(onClick = onDismiss) { Text(stringResource(R.string.cancel)) } },
    )
}

private const val MULTILINE_MIN_LINES = 3
