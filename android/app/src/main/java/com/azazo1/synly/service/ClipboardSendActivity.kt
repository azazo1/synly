package com.azazo1.synly.service

import android.app.Activity
import android.content.Intent
import android.net.Uri
import android.os.Build
import android.os.Bundle
import android.provider.MediaStore
import android.widget.Toast
import androidx.activity.ComponentActivity
import androidx.activity.result.contract.ActivityResultContracts
import androidx.core.content.FileProvider
import androidx.lifecycle.lifecycleScope
import com.azazo1.synly.R
import com.azazo1.synly.core.ClipboardSend
import java.io.File
import java.text.SimpleDateFormat
import java.util.Date
import java.util.Locale
import kotlinx.coroutines.launch

/**
 * 处理通知栏发送动作与系统文件分享, 将文件先放入剪贴板再同步给桌面端.
 */
class ClipboardSendActivity : ComponentActivity() {
    private var pendingCaptureUri: Uri? = null

    private val pickFileLauncher =
        registerForActivityResult(ActivityResultContracts.OpenDocument()) { uri ->
            if (uri != null) {
                sendUris(listOf(uri))
            } else {
                finish()
            }
        }

    private val takePictureLauncher =
        registerForActivityResult(ActivityResultContracts.StartActivityForResult()) { result ->
            val uri = pendingCaptureUri
            pendingCaptureUri = null
            if (result.resultCode == Activity.RESULT_OK && uri != null) {
                sendUris(listOf(uri))
            } else {
                deleteCapture(uri)
                finish()
            }
        }

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        pendingCaptureUri = savedInstanceState?.getParcelable(KEY_CAPTURE_URI)
        when (intent.action) {
            ACTION_PICK_FILE -> pickFileLauncher.launch(arrayOf("*/*"))
            ACTION_CAPTURE_PHOTO -> startCapture()
            Intent.ACTION_SEND, Intent.ACTION_SEND_MULTIPLE -> handleShare(intent)
            else -> {
                toast(R.string.send_action_unknown)
                finish()
            }
        }
    }

    override fun onSaveInstanceState(outState: Bundle) {
        outState.putParcelable(KEY_CAPTURE_URI, pendingCaptureUri)
        super.onSaveInstanceState(outState)
    }

    private fun startCapture() {
        val directory = File(cacheDir, "captures").apply { mkdirs() }
        val fileName =
            "IMG_${SimpleDateFormat("yyyyMMdd_HHmmss", Locale.US).format(Date())}.jpg"
        val file = File(directory, fileName)
        val uri = FileProvider.getUriForFile(this, "${packageName}.files", file)
        pendingCaptureUri = uri
        val intent = Intent(MediaStore.ACTION_IMAGE_CAPTURE)
            .putExtra(MediaStore.EXTRA_OUTPUT, uri)
            .addFlags(Intent.FLAG_GRANT_WRITE_URI_PERMISSION)
        takePictureLauncher.launch(intent)
    }

    private fun handleShare(intent: Intent) {
        val uris = intent.streamUris()
        if (uris.isEmpty()) {
            toast(R.string.send_file_only)
            finish()
        } else {
            sendUris(uris)
        }
    }

    private fun sendUris(uris: List<Uri>) {
        lifecycleScope.launch {
            val result = ClipboardSend.send(applicationContext, uris)
            val message = result.fold(
                onSuccess = { it },
                onFailure = { getString(R.string.send_file_failed, it.message ?: "未知错误") },
            )
            toast(message)
            deleteCapture()
            finish()
        }
    }

    private fun deleteCapture(uri: Uri? = pendingCaptureUri) {
        pendingCaptureUri = null
        uri?.path?.let { path -> File(path).delete() }
    }

    private fun toast(message: CharSequence) {
        Toast.makeText(this, message, Toast.LENGTH_LONG).show()
    }

    private fun toast(messageRes: Int) {
        toast(getString(messageRes))
    }

    private fun Intent.streamUris(): List<Uri> {
        val extraUris = if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.TIRAMISU) {
            getParcelableArrayListExtra(Intent.EXTRA_STREAM, Uri::class.java)
        } else {
            @Suppress("DEPRECATION")
            getParcelableArrayListExtra(Intent.EXTRA_STREAM)
        }.orEmpty()
        val clipUris = clipData
            ?.let { clip -> (0 until clip.itemCount).mapNotNull { clip.getItemAt(it).uri } }
            .orEmpty()
        return (extraUris + clipUris).distinct()
    }

    companion object {
        const val ACTION_PICK_FILE = "com.azazo1.synly.action.PICK_FILE"
        const val ACTION_CAPTURE_PHOTO = "com.azazo1.synly.action.CAPTURE_PHOTO"

        private const val KEY_CAPTURE_URI = "capture_uri"
    }
}
