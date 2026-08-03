package com.azazo1.synly.service

import android.os.Bundle
import android.widget.Toast
import androidx.activity.ComponentActivity
import androidx.lifecycle.lifecycleScope
import com.azazo1.synly.R
import com.azazo1.synly.core.ClipboardReader
import com.azazo1.synly.core.SynlyEngine
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.delay
import kotlinx.coroutines.launch
import kotlinx.coroutines.withContext

/**
 * 手动发送剪贴板的读取界面.
 * 用户点击通知或快速发送按钮后, 通过本界面读取当前剪贴板并同步给桌面端,
 * 完成后立即关闭.
 */
class ClipboardReadActivity : ComponentActivity() {
    private val manual: Boolean
        get() = intent.getBooleanExtra(EXTRA_MANUAL, false)

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        SynlyEngine.init(applicationContext)
        SynlyEngine.start(applicationContext)
        ClipboardSyncService.start(applicationContext)
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
                val message = withContext(Dispatchers.IO) {
                    if (manual) {
                        if (!SynlyEngine.canSend()) {
                            SynlyEngine.start(applicationContext)
                            val deadline = System.currentTimeMillis() + CONNECT_WAIT_MS
                            while (System.currentTimeMillis() < deadline && !SynlyEngine.canSend()) {
                                delay(100)
                            }
                            if (!SynlyEngine.canSend()) {
                                return@withContext getString(R.string.send_clipboard_disconnected)
                            }
                        }
                        val payload = ClipboardReader.readNow(applicationContext)
                            ?: return@withContext getString(R.string.send_clipboard_empty)
                        if (!SynlyEngine.sendClipboard(payload)) {
                            return@withContext getString(R.string.send_clipboard_failed)
                        }
                        getString(R.string.send_clipboard_sent)
                    } else {
                        if (SynlyEngine.canSend()) {
                            ClipboardReader.takePending(applicationContext) { payload ->
                                SynlyEngine.sendClipboard(payload)
                            }
                        }
                        null
                    }
                }
                if (message != null) {
                    Toast.makeText(this@ClipboardReadActivity, message, Toast.LENGTH_SHORT).show()
                }
            } finally {
                finish()
            }
        }
    }

    companion object {
        const val EXTRA_MANUAL = "manual"

        private const val READ_DELAY_MS = 120L
        private const val CONNECT_WAIT_MS = 3000L
    }
}
