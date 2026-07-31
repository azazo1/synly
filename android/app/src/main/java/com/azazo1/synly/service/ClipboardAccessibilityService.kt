package com.azazo1.synly.service

import android.accessibilityservice.AccessibilityService
import android.view.accessibility.AccessibilityEvent
import com.azazo1.synly.core.ClipboardReader
import com.azazo1.synly.core.SynlyEngine
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.SupervisorJob
import kotlinx.coroutines.cancel
import kotlinx.coroutines.delay
import kotlinx.coroutines.isActive
import kotlinx.coroutines.launch

class ClipboardAccessibilityService : AccessibilityService() {
    private val scope = CoroutineScope(SupervisorJob() + Dispatchers.IO)

    override fun onServiceConnected() {
        super.onServiceConnected()
        scope.launch {
            while (isActive) {
                runCatching {
                    if (SynlyEngine.canSend()) {
                        ClipboardReader.poll(applicationContext) { payload ->
                            SynlyEngine.sendClipboard(payload)
                        }
                    }
                }
                delay(POLL_INTERVAL_MS)
            }
        }
    }

    override fun onAccessibilityEvent(event: AccessibilityEvent?) {
        // 剪贴板读取采用轮询, 无障碍事件仅用于保持服务活跃.
    }

    override fun onInterrupt() {
        // 无操作.
    }

    override fun onDestroy() {
        scope.cancel()
        ClipboardReader.reset()
        super.onDestroy()
    }

    companion object {
        private const val POLL_INTERVAL_MS = 500L
    }
}

