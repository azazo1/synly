package com.azazo1.synly.core

import android.content.Context
import android.provider.OpenableColumns
import android.webkit.MimeTypeMap
import java.io.ByteArrayOutputStream
import java.io.IOException

object ClipboardFiles {
    private const val BUFFER_SIZE = 64 * 1024

    fun read(context: Context, uris: List<android.net.Uri>): List<ClipboardFile> {
        require(uris.isNotEmpty()) { "没有收到文件" }
        val maxBytes = SettingsStore.load(context).maxClipboardBytes
        var totalBytes = 0L
        return uris.mapIndexed { index, uri ->
            val bytes = readBoundedBytes(context, uri, maxBytes)
            totalBytes += bytes.size
            if (totalBytes > maxBytes) {
                throw IOException("剪贴板内容超过大小上限")
            }
            ClipboardFile(displayName(context, uri, index), bytes)
        }
    }

    fun safeName(name: String): String {
        val sanitized = name
            .map { char -> if (char in "\\/:*?\"<>|") '_' else char }
            .joinToString("")
            .trim()
        return sanitized.ifBlank { "file" }
    }

    private fun readBoundedBytes(
        context: Context,
        uri: android.net.Uri,
        maxBytes: Long,
    ): ByteArray {
        val resolver = context.contentResolver
        val declaredSize = querySize(resolver, uri)
        if (declaredSize != null && declaredSize > maxBytes) {
            throw IOException("文件超过剪贴板大小上限")
        }
        val input = resolver.openInputStream(uri) ?: throw IOException("无法读取文件")
        return input.use { stream ->
            val output = ByteArrayOutputStream()
            val buffer = ByteArray(BUFFER_SIZE)
            var total = 0L
            while (true) {
                val count = stream.read(buffer)
                if (count < 0) break
                total += count
                if (total > maxBytes) {
                    throw IOException("文件超过剪贴板大小上限")
                }
                output.write(buffer, 0, count)
            }
            output.toByteArray()
        }
    }

    private fun querySize(
        resolver: android.content.ContentResolver,
        uri: android.net.Uri,
    ): Long? {
        resolver.query(uri, arrayOf(OpenableColumns.SIZE), null, null, null)?.use { cursor ->
            if (!cursor.moveToFirst()) return null
            val column = cursor.getColumnIndex(OpenableColumns.SIZE)
            if (column >= 0 && !cursor.isNull(column)) {
                return cursor.getLong(column)
            }
        }
        return null
    }

    private fun displayName(
        context: Context,
        uri: android.net.Uri,
        index: Int,
    ): String {
        context.contentResolver
            .query(uri, arrayOf(OpenableColumns.DISPLAY_NAME), null, null, null)
            ?.use { cursor ->
                if (cursor.moveToFirst()) {
                    val column = cursor.getColumnIndex(OpenableColumns.DISPLAY_NAME)
                    if (column >= 0) {
                        val name = cursor.getString(column)
                        if (!name.isNullOrBlank()) {
                            return safeName(name)
                        }
                    }
                }
            }
        val mime = context.contentResolver.getType(uri)
        val extension = MimeTypeMap.getSingleton().getExtensionFromMimeType(mime)
        return "synly-${index + 1}.${extension ?: "bin"}"
    }
}
