package com.azazo1.synly.core

import android.content.ClipDescription
import android.content.ClipboardManager
import android.content.Context
import android.graphics.BitmapFactory
import java.io.ByteArrayOutputStream

object ClipboardReader {
    @Volatile
    private var lastSignature: String? = null

    @Volatile
    private var suppressSignature: String? = null

    fun poll(context: Context, onPayload: (ClipboardPayload) -> Unit) {
        val payload = readNow(context) ?: return
        val signature = payload.signature()
        if (signature == lastSignature) return
        lastSignature = signature
        if (signature == suppressSignature) {
            suppressSignature = null
            return
        }
        onPayload(payload)
    }

    fun readNow(context: Context): ClipboardPayload? {
        val clipboard = context.getSystemService(ClipboardManager::class.java)
        val clip = clipboard.primaryClip ?: return null
        if (clip.itemCount == 0) return null
        val item = clip.getItemAt(0)
        if (clip.description.hasMimeType("text/html")) {
            val html = item.htmlText?.toString()
            val text = item.text?.toString()
            if (html != null || text != null) {
                return ClipboardPayload(text = text, html = html, imagePng = null)
            }
        }
        if (clip.description.hasMimeType(ClipDescription.MIMETYPE_TEXT_PLAIN)) {
            val text = item.text?.toString()
            if (text != null) return ClipboardPayload(text = text, html = null, imagePng = null)
        }
        val imageMime = (0 until clip.description.mimeTypeCount)
            .map { clip.description.getMimeType(it) }
            .firstOrNull { it.startsWith("image/") }
        if (imageMime != null) {
            val uri = item.uri ?: return null
            val bytes = readImageBytes(context, uri) ?: return null
            return ClipboardPayload(text = null, html = null, imagePng = bytes)
        }
        return null
    }

    fun suppress(payload: ClipboardPayload) {
        suppressSignature = payload.signature()
    }

    fun reset() {
        lastSignature = null
        suppressSignature = null
    }

    private fun readImageBytes(context: Context, uri: android.net.Uri): ByteArray? {
        val max = SettingsStore.load(context).maxImageBytes
        val bytes = runCatching {
            context.contentResolver.openInputStream(uri)?.use { it.readBytes() }
        }.getOrNull() ?: return null
        if (bytes.isEmpty() || bytes.size > max) return null
        if (bytes.size >= 8 &&
            bytes[0] == 0x89.toByte() && bytes[1] == 0x50.toByte() &&
            bytes[2] == 0x4E.toByte() && bytes[3] == 0x47.toByte()
        ) {
            return bytes
        }
        val bitmap = BitmapFactory.decodeByteArray(bytes, 0, bytes.size) ?: return null
        return try {
            val output = ByteArrayOutputStream()
            bitmap.compress(android.graphics.Bitmap.CompressFormat.PNG, 100, output)
            output.toByteArray()
        } finally {
            bitmap.recycle()
        }
    }
}
