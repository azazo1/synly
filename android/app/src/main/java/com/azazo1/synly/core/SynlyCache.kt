package com.azazo1.synly.core

import android.content.Context
import java.io.File

data class CacheClearResult(
    val fileCount: Int,
    val bytes: Long,
)

object SynlyCache {
    private const val TAG = "SynlyCache"

    fun clear(context: Context): CacheClearResult {
        val directories = listOf(
            File(context.cacheDir, "clipboard"),
            File(context.cacheDir, "captures"),
        )
        var fileCount = 0
        var bytes = 0L
        for (directory in directories) {
            directory.listFiles()?.forEach { file ->
                if (!file.isFile) return@forEach
                val length = file.length()
                if (file.delete()) {
                    fileCount += 1
                    bytes += length
                }
            }
        }
        SynlyLog.i(TAG, "缓存已清理, 共 $fileCount 个文件, $bytes 字节")
        return CacheClearResult(fileCount, bytes)
    }
}
