package com.azazo1.synly.core

import android.content.ClipData
import android.content.ClipboardManager
import android.content.Context
import androidx.core.content.FileProvider
import java.io.File
import java.util.UUID

object ClipboardWriter {
    fun applyRemote(context: Context, payload: ClipboardPayload, strict: Boolean = false): Boolean {
        if (payload.isEmpty()) return false
        if (strict) {
            val binaryBytes = payload.files.sumOf { it.bytes.size.toLong() } + (payload.imagePng?.size?.toLong() ?: 0L)
            if (binaryBytes > SettingsStore.load(context).maxClipboardBytes) return false
        }
        val clipboard = context.getSystemService(ClipboardManager::class.java)
        val clip = when {
            payload.files.isNotEmpty() -> {
                val uris = ClipboardCache.writeRemote(context, payload.files)
                if (uris.isEmpty() || (strict && uris.size != payload.files.size)) {
                    null
                } else {
                    val uriClip = ClipData.newUri(
                        context.contentResolver,
                        "synly files",
                        uris.first(),
                    )
                    uris.drop(1).forEach { uriClip.addItem(ClipData.Item(it)) }
                    uriClip
                }
            }

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
        return true
    }

    private fun writePng(context: Context, bytes: ByteArray): android.net.Uri? {
        return runCatching {
            val directory = File(context.cacheDir, "clipboard").apply { mkdirs() }
            val file = File(directory, "remote-${UUID.randomUUID()}.png")
            file.writeBytes(bytes)
            FileProvider.getUriForFile(context, "${context.packageName}.files", file)
        }.getOrNull()
            ?.also { ClipboardCache.prune(context) }
    }

    private fun htmlToText(html: String): String {
        return html.replace(Regex("<[^>]+>"), " ").replace(Regex("\\s+"), " ").trim()
    }
}
