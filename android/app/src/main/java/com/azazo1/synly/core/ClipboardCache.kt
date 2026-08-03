package com.azazo1.synly.core

import android.content.Context
import androidx.core.content.FileProvider
import java.io.File

object ClipboardCache {
    fun writeLocal(context: Context, files: List<ClipboardFile>): List<android.net.Uri> {
        return write(context, "send", files)
    }

    fun writeRemote(context: Context, files: List<ClipboardFile>): List<android.net.Uri> {
        return write(context, "remote", files)
    }

    fun prune(context: Context) {
        prune(context, emptySet())
    }

    private fun prune(context: Context, keep: Set<File>) {
        val maxBytes = SettingsStore.load(context).maxClipboardCacheBytes
        if (maxBytes <= 0) return
        val directory = File(context.cacheDir, "clipboard")
        val files = directory.listFiles()
            ?.filter { it.isFile }
            ?.filterNot { it in keep }
            ?.sortedBy { it.lastModified() }
            ?: return
        var total = files.sumOf { it.length() }
        for (file in files) {
            if (total <= maxBytes) break
            val length = file.length()
            if (file.delete()) {
                total -= length
            }
        }
    }

    private fun write(
        context: Context,
        prefix: String,
        files: List<ClipboardFile>,
    ): List<android.net.Uri> {
        val directory = File(context.cacheDir, "clipboard").apply { mkdirs() }
        prune(context)
        val writtenFiles = mutableListOf<File>()
        val uris = files.map { file ->
            val safeName = ClipboardFiles.safeName(file.name)
            val target = uniqueCacheFile(directory, safeName)
            target.writeBytes(file.bytes)
            writtenFiles += target
            FileProvider.getUriForFile(context, "${context.packageName}.files", target)
        }
        prune(context, writtenFiles.toSet())
        return uris
    }

    private fun uniqueCacheFile(directory: File, name: String): File {
        var candidate = File(directory, name)
        if (!candidate.exists()) return candidate
        val dot = name.lastIndexOf('.')
        val stem = if (dot > 0) name.substring(0, dot) else name
        val extension = if (dot > 0) name.substring(dot) else ""
        var index = 2
        while (true) {
            candidate = File(directory, "$stem-$index$extension")
            if (!candidate.exists()) return candidate
            index += 1
        }
    }
}
