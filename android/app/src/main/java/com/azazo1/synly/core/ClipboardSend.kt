package com.azazo1.synly.core

import android.content.ClipData
import android.content.ClipboardManager
import android.content.Context
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.withContext

object ClipboardSend {
    suspend fun send(context: Context, uris: List<android.net.Uri>): Result<String> {
        return withContext(Dispatchers.IO) {
            runCatching {
                if (!SynlyEngine.canSend()) {
                    error("未连接")
                }
                val files = ClipboardFiles.read(context, uris)
                if (files.isEmpty()) {
                    error("没有收到文件")
                }
                SynlyLog.i("ClipboardSend", "已读取 ${files.size} 个文件")
                val payload = ClipboardPayload(files = files)
                val clipUris = ClipboardCache.writeLocal(context, files)
                val clip = ClipData.newUri(
                    context.contentResolver,
                    "synly files",
                    clipUris.first(),
                )
                clipUris.drop(1).forEach { clip.addItem(ClipData.Item(it)) }
                context.getSystemService(ClipboardManager::class.java)
                    .setPrimaryClip(clip)
                ClipboardReader.markSent(payload)
                if (!SynlyEngine.sendClipboard(payload)) {
                    error("发送失败")
                }
                val message = "已发送 ${files.size} 个文件"
                SynlyLog.i("ClipboardSend", message)
                message
            }
        }
    }
}
