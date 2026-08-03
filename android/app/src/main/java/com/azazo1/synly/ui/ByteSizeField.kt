package com.azazo1.synly.ui

import androidx.compose.foundation.text.KeyboardOptions
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.material3.OutlinedTextField
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.DisposableEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.rememberUpdatedState
import androidx.compose.runtime.setValue
import androidx.compose.ui.Modifier
import androidx.compose.ui.focus.onFocusChanged
import androidx.compose.ui.text.input.KeyboardType
import uniffi.synly_core.formatHumanBytes
import uniffi.synly_core.parseHumanBytes

@Composable
fun ByteSizeField(
    valueBytes: Long,
    onCommit: (Long) -> Unit,
    label: String,
    minBytes: Long = 1,
    maxBytes: Long = Long.MAX_VALUE,
    modifier: Modifier = Modifier,
) {
    var text by remember(valueBytes) {
        mutableStateOf(formatHumanBytes(valueBytes.coerceAtLeast(0).toULong()))
    }
    var error by remember(valueBytes) { mutableStateOf<String?>(null) }
    val currentText by rememberUpdatedState(text)

    fun commit(raw: String) {
        val parsed = runCatching { parseHumanBytes(raw) }.getOrNull()
            ?: run {
                error = "格式无效"
                return
            }
        if (parsed !in minBytes.toULong()..maxBytes.toULong()) {
            error = "有效范围: ${formatHumanBytes(minBytes.toULong())} 到 " +
                formatHumanBytes(maxBytes.toULong())
            return
        }
        error = null
        text = formatHumanBytes(parsed)
        onCommit(parsed.toLong())
    }

    DisposableEffect(Unit) {
        onDispose {
            val parsed = runCatching { parseHumanBytes(currentText) }.getOrNull()
            if (parsed != null && parsed in minBytes.toULong()..maxBytes.toULong()) {
                onCommit(parsed.toLong())
            }
        }
    }

    OutlinedTextField(
        value = text,
        onValueChange = {
            text = it
            error = null
        },
        label = { Text(label) },
        singleLine = true,
        isError = error != null,
        supportingText = {
            error?.let { Text(it) }
        },
        keyboardOptions = KeyboardOptions(keyboardType = KeyboardType.Ascii),
        modifier = modifier
            .fillMaxWidth()
            .onFocusChanged { if (!it.isFocused) commit(text) },
    )
}
