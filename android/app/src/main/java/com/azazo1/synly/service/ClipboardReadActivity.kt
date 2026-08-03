package com.azazo1.synly.service

import android.app.Activity
import android.os.Bundle
import androidx.lifecycle.lifecycleScope
import com.azazo1.synly.core.ClipboardReader
import com.azazo1.synly.core.SynlyEngine
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.launch
import kotlinx.coroutines.withContext

/**
 * 透明的剪贴板读取界面.
 * Android 10+ 只允许前台有焦点的应用读取剪贴板, 因此后台收到剪贴板变化通知后,
 * 通过本界面短暂抢占前台焦点完成读取, 随后立即关闭.
 */
class ClipboardReadActivity : Activity() {
    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
    }

    override fun onWindowFocusChanged(hasFocus: Boolean) {
        super.onWindowFocusChanged(hasFocus)
        if (hasFocus) {
            window.decorView.postDelayed({
                readAndFinish()
            }, READ_DELAY_MS)
        }
    }

    private fun readAndFinish() {
        lifecycleScope.launch {
            try {
                withContext(Dispatchers.IO) {
                    if (SynlyEngine.canSend()) {
                        ClipboardReader.takePending(applicationContext) { payload ->
                            SynlyEngine.sendClipboard(payload)
                        }
                    }
                }
            } finally {
                finish()
            }
        }
    }

    companion object {
        private const val READ_DELAY_MS = 120L
    }
}
