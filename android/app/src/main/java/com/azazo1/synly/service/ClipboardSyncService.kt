package com.azazo1.synly.service

import android.app.Notification
import android.app.NotificationChannel
import android.app.NotificationManager
import android.app.PendingIntent
import android.content.ClipboardManager
import android.content.Context
import android.content.Intent
import android.net.wifi.WifiManager
import android.os.Build
import android.os.IBinder
import androidx.core.app.NotificationCompat
import androidx.core.content.ContextCompat
import com.azazo1.synly.MainActivity
import com.azazo1.synly.R
import com.azazo1.synly.core.ClipboardReadGate
import com.azazo1.synly.core.SynlyEngine
import com.azazo1.synly.core.SynlyLog
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.Job
import kotlinx.coroutines.SupervisorJob
import kotlinx.coroutines.cancel
import kotlinx.coroutines.delay
import kotlinx.coroutines.launch
import uniffi.synly_core.FfiClientState

class ClipboardSyncService : android.app.Service() {
    companion object {
        private const val TAG = "ClipboardSyncService"
        private const val CHANNEL_ID = "synly_sync"
        private const val NOTIFICATION_ID = 1
        private const val NOTIFICATION_REFRESH_INTERVAL_MS = 60_000L

        fun start(context: Context) {
            ContextCompat.startForegroundService(
                context,
                Intent(context, ClipboardSyncService::class.java),
            )
        }

        fun stop(context: Context) {
            context.stopService(Intent(context, ClipboardSyncService::class.java))
        }
    }

    private var multicastLock: WifiManager.MulticastLock? = null

    private val scope = CoroutineScope(SupervisorJob() + Dispatchers.Main.immediate)
    private var notificationJob: Job? = null
    private var notificationRefreshJob: Job? = null
    private var lastNotificationState: FfiClientState? = null
    private var lastNotificationDevice: String? = null
    private var lastNotificationTarget: String? = null

    private val clipboardListener = ClipboardManager.OnPrimaryClipChangedListener {
        maybeReadClipboard()
    }

    override fun onCreate() {
        super.onCreate()
        createChannel()
    }

    override fun onStartCommand(intent: Intent?, flags: Int, startId: Int): Int {
        SynlyEngine.init(applicationContext)
        SynlyEngine.start(applicationContext)
        val currentUi = SynlyEngine.uiState.value
        startForeground(
            NOTIFICATION_ID,
            buildNotification(currentUi.state, currentUi.connectedDevice, currentUi.targetLabel),
        )
        acquireMulticastLock()
        getSystemService(ClipboardManager::class.java)
            .addPrimaryClipChangedListener(clipboardListener)
        if (notificationJob == null) {
            notificationJob = scope.launch {
                SynlyEngine.uiState.collect { ui ->
                    val state = ui.state
                    val device = ui.connectedDevice
                    val target = ui.targetLabel
                    if (state != lastNotificationState ||
                        device != lastNotificationDevice ||
                        target != lastNotificationTarget
                    ) {
                        lastNotificationState = state
                        lastNotificationDevice = device
                        lastNotificationTarget = target
                        showNotification(state, device, target)
                    }
                }
            }
        }
        scheduleNotificationRefresh()
        return START_STICKY
    }

    override fun onDestroy() {
        stopForeground(STOP_FOREGROUND_REMOVE)
        getSystemService(ClipboardManager::class.java)
            .removePrimaryClipChangedListener(clipboardListener)
        releaseMulticastLock()
        notificationJob?.cancel()
        notificationJob = null
        notificationRefreshJob?.cancel()
        notificationRefreshJob = null
        scope.cancel()
        SynlyEngine.stop()
        super.onDestroy()
    }

    override fun onBind(intent: Intent?): IBinder? = null

    private fun createChannel() {
        if (Build.VERSION.SDK_INT < Build.VERSION_CODES.O) return
        val manager = getSystemService(NotificationManager::class.java)
        val channel = NotificationChannel(
            CHANNEL_ID,
            getString(R.string.sync_notification_channel),
            NotificationManager.IMPORTANCE_LOW,
        )
        manager.createNotificationChannel(channel)
    }

    private fun buildNotification(
        state: FfiClientState?,
        connectedDevice: String?,
        targetLabel: String?,
    ): Notification {
        val openAppPending = PendingIntent.getActivity(
            this,
            0,
            Intent(this, MainActivity::class.java),
            PendingIntent.FLAG_IMMUTABLE,
        )
        val pickFilePending = notificationAction(
            1,
            ClipboardSendActivity.ACTION_PICK_FILE,
        )
        val capturePhotoPending = notificationAction(
            2,
            ClipboardSendActivity.ACTION_CAPTURE_PHOTO,
        )
        val sendClipboardPending = PendingIntent.getActivity(
            this,
            3,
            Intent(this, ClipboardReadActivity::class.java)
                .putExtra(ClipboardReadActivity.EXTRA_MANUAL, true),
            PendingIntent.FLAG_IMMUTABLE or PendingIntent.FLAG_UPDATE_CURRENT,
        )
        val target = targetLabel ?: getString(R.string.sync_notification_unknown)
        val statusText = when (state) {
            FfiClientState.CONNECTING -> getString(R.string.sync_notification_connecting, target)
            FfiClientState.PAIRING -> getString(R.string.sync_notification_pairing)
            FfiClientState.CONNECTED ->
                getString(R.string.sync_notification_connected, connectedDevice.orEmpty())

            FfiClientState.RECONNECTING -> getString(R.string.sync_notification_reconnecting, target)
            null -> getString(R.string.sync_notification_disconnected)
        }
        return NotificationCompat.Builder(this, CHANNEL_ID)
            .setSmallIcon(R.drawable.ic_notification)
            .setContentTitle(getString(R.string.sync_notification_title))
            .setContentText(statusText)
            .setContentIntent(openAppPending)
            .addAction(
                0,
                getString(R.string.sync_notification_action_pick_file),
                pickFilePending,
            )
            .addAction(
                0,
                getString(R.string.sync_notification_action_capture_photo),
                capturePhotoPending,
            )
            .addAction(
                0,
                getString(R.string.sync_notification_action_send_clipboard),
                sendClipboardPending,
            )
            .setOngoing(true)
            .setOnlyAlertOnce(true)
            .setShowWhen(true)
            .setWhen(System.currentTimeMillis())
            .build()
    }

    private fun notificationAction(requestCode: Int, action: String): PendingIntent {
        return PendingIntent.getActivity(
            this,
            requestCode,
            Intent(this, ClipboardSendActivity::class.java).setAction(action),
            PendingIntent.FLAG_IMMUTABLE or PendingIntent.FLAG_UPDATE_CURRENT,
        )
    }

    private fun acquireMulticastLock() {
        if (multicastLock != null) return
        val wifi = applicationContext.getSystemService(Context.WIFI_SERVICE) as WifiManager
        multicastLock = wifi.createMulticastLock("synly-mdns").apply {
            setReferenceCounted(false)
            acquire()
        }
    }

    private fun releaseMulticastLock() {
        multicastLock?.takeIf { it.isHeld }?.release()
        multicastLock = null
    }

    private fun scheduleNotificationRefresh() {
        if (notificationRefreshJob != null) return
        notificationRefreshJob = scope.launch {
            while (true) {
                delay(NOTIFICATION_REFRESH_INTERVAL_MS)
                val ui = SynlyEngine.uiState.value
                showNotification(ui.state, ui.connectedDevice, ui.targetLabel)
            }
        }
    }

    private fun showNotification(
        state: FfiClientState?,
        connectedDevice: String?,
        targetLabel: String?,
    ) {
        getSystemService(NotificationManager::class.java)
            .notify(NOTIFICATION_ID, buildNotification(state, connectedDevice, targetLabel))
    }

    private fun maybeReadClipboard() {
        if (!SynlyEngine.canSend()) return
        if (!ClipboardReadGate.tryAcquire()) return
        runCatching {
            startActivity(
                Intent(this, ClipboardReadActivity::class.java)
                    .addFlags(Intent.FLAG_ACTIVITY_NEW_TASK),
            )
        }.onFailure {
            SynlyLog.w(TAG, "启动剪贴板读取界面失败", it)
        }
    }

}
