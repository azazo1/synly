package com.azazo1.synly.core

import android.content.ClipData
import android.content.ClipboardManager
import android.content.Context
import androidx.core.content.FileProvider
import java.io.File
import java.util.UUID

object ClipboardWriter {
    fun applyRemote(context: Context, payload: ClipboardPayload): Boolean {
        if (payload.isEmpty()) return false
        val clipboard = context.getSystemService(ClipboardManager::class.java)
        val clip = when {
            payload.imagePng != null -> {
                val uri = writePng(context, payload.imagePng!!)
                uri?.let { ClipData.newUri(context.contentResolver, "synly image", it) }
            }

            payload.html != null -> {
                ClipData.newHtmlText(
                    "synly",
                    payload.text ?: htmlToText(payload.html!!),
                    payload.html!!,
                )
            }

            else -> ClipData.newPlainText("synly", payload.text)
        } ?: return false
        clipboard.setPrimaryClip(clip)
        ClipboardReader.suppress(payload)
        return true
    }

    private fun writePng(context: Context, bytes: ByteArray): android.net.Uri? {
        return runCatching {
            val directory = File(context.cacheDir, "clipboard").apply { mkdirs() }
            val file = File(directory, "remote-${UUID.randomUUID()}.png")
            file.writeBytes(bytes)
            FileProvider.getUriForFile(context, "${context.packageName}.files", file)
        }.getOrNull()
    }

    private fun htmlToText(html: String): String {
        return html.replace(Regex("<[^>]+>"), " ").replace(Regex("\\s+"), " ").trim()
    }
}

