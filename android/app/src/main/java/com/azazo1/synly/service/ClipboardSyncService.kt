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
import android.os.SystemClock
import androidx.core.app.NotificationCompat
import androidx.core.content.ContextCompat
import com.azazo1.synly.MainActivity
import com.azazo1.synly.R
import com.azazo1.synly.core.SynlyEngine
import com.azazo1.synly.core.SynlyLog
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.Job
import kotlinx.coroutines.SupervisorJob
import kotlinx.coroutines.cancel
import kotlinx.coroutines.launch
import uniffi.synly_core.FfiClientState

class ClipboardSyncService : android.app.Service() {
    companion object {
        private const val TAG = "ClipboardSyncService"
        private const val CHANNEL_ID = "synly_sync"
        private const val NOTIFICATION_ID = 1
        private const val MIN_TRIGGER_INTERVAL_MS = 300L

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

    private var lastReadTriggerMs = 0L

    private val scope = CoroutineScope(SupervisorJob() + Dispatchers.Main.immediate)
    private var notificationJob: Job? = null
    private var lastNotificationState: FfiClientState? = null
    private var lastNotificationDevice: String? = null

    private val clipboardListener = ClipboardManager.OnPrimaryClipChangedListener {
        maybeReadClipboard()
    }

    override fun onCreate() {
        super.onCreate()
        createChannel()
    }

    override fun onStartCommand(intent: Intent?, flags: Int, startId: Int): Int {
        startForeground(NOTIFICATION_ID, buildNotification(null, null))
        acquireMulticastLock()
        getSystemService(ClipboardManager::class.java)
            .addPrimaryClipChangedListener(clipboardListener)
        SynlyEngine.init(applicationContext)
        SynlyEngine.start(applicationContext)
        if (notificationJob == null) {
            notificationJob = scope.launch {
                SynlyEngine.uiState.collect { ui ->
                    val state = ui.state
                    val device = ui.connectedDevice
                    if (state != lastNotificationState || device != lastNotificationDevice) {
                        lastNotificationState = state
                        lastNotificationDevice = device
                        getSystemService(NotificationManager::class.java)
                            .notify(NOTIFICATION_ID, buildNotification(state, device))
                    }
                }
            }
        }
        return START_STICKY
    }

    override fun onDestroy() {
        getSystemService(ClipboardManager::class.java)
            .removePrimaryClipChangedListener(clipboardListener)
        releaseMulticastLock()
        notificationJob?.cancel()
        notificationJob = null
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
    ): Notification {
        val pending = PendingIntent.getActivity(
            this,
            0,
            Intent(this, MainActivity::class.java),
            PendingIntent.FLAG_IMMUTABLE,
        )
        val statusText = when (state) {
            FfiClientState.CONNECTING -> getString(R.string.sync_notification_connecting)
            FfiClientState.PAIRING -> getString(R.string.sync_notification_pairing)
            FfiClientState.CONNECTED ->
                getString(R.string.sync_notification_connected, connectedDevice.orEmpty())

            FfiClientState.RECONNECTING -> getString(R.string.sync_notification_reconnecting)
            null -> getString(R.string.sync_notification_disconnected)
        }
        return NotificationCompat.Builder(this, CHANNEL_ID)
            .setSmallIcon(R.drawable.ic_notification)
            .setContentTitle(getString(R.string.sync_notification_title))
            .setContentText(statusText)
            .setContentIntent(pending)
            .setOngoing(true)
            .build()
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

    private fun maybeReadClipboard() {
        if (!SynlyEngine.canSend()) return
        val now = SystemClock.elapsedRealtime()
        if (now - lastReadTriggerMs < MIN_TRIGGER_INTERVAL_MS) return
        lastReadTriggerMs = now
        runCatching {
            startActivity(
                Intent(this, ClipboardReadActivity::class.java)
                    .addFlags(Intent.FLAG_ACTIVITY_NEW_TASK),
            )
        }.onFailure {
            SynlyLog.w(TAG, "启动剪贴板读取界面失败, 请检查悬浮窗权限", it)
        }
    }

}
