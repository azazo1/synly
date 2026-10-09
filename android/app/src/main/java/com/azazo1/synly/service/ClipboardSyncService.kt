package com.azazo1.synly.service

import android.app.Notification
import android.app.NotificationChannel
import android.app.NotificationManager
import android.app.PendingIntent
import android.content.Context
import android.content.Intent
import android.net.ConnectivityManager
import android.net.Network
import android.net.NetworkCapabilities
import android.net.NetworkRequest
import android.net.wifi.WifiManager
import android.os.Build
import android.os.IBinder
import androidx.core.app.NotificationCompat
import androidx.core.content.ContextCompat
import com.azazo1.synly.MainActivity
import com.azazo1.synly.R
import com.azazo1.synly.core.SynlyEngine
import com.azazo1.synly.core.SynlyLog
import java.net.Inet4Address
import java.net.NetworkInterface
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.Job
import kotlinx.coroutines.SupervisorJob
import kotlinx.coroutines.cancel
import kotlinx.coroutines.delay
import kotlinx.coroutines.launch
import kotlinx.coroutines.withContext
import uniffi.synly_core.FfiClientState

class ClipboardSyncService : android.app.Service() {
    companion object {
        private const val TAG = "ClipboardSyncService"
        private const val CHANNEL_ID = "synly_sync"
        private const val NOTIFICATION_ID = 1
        private const val NOTIFICATION_REFRESH_INTERVAL_MS = 60_000L
        private const val WIFI_LOST_EXIT_DELAY_MS = 30_000L

        // 蜂窝数据 (含 464xlat) 与隧道的接口名, 这些不算可用的局域网.
        private val NON_LOCAL_INTERFACE_PREFIXES = listOf(
            "rmnet",
            "ccmni",
            "pdp",
            "wwan",
            "clat",
            "v4-",
            "tun",
            "sit",
            "ip6tnl",
            "dummy",
        )

        // 热点等共享模式的接口名, 没有客户端接入时可能尚未配置 IPv4 地址.
        private val AP_INTERFACE_PREFIXES = listOf("ap", "softap", "swlan", "wlan1")

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
    private var wifiExitJob: Job? = null
    private var connectivityManager: ConnectivityManager? = null
    private var networkCallback: ConnectivityManager.NetworkCallback? = null
    private var lastNotificationState: FfiClientState? = null
    private var lastNotificationDevice: String? = null
    private var lastNotificationTarget: String? = null
    private var lastBluetoothPath: Boolean? = null

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
        monitorWifiAvailability()
        if (notificationJob == null) {
            notificationJob = scope.launch {
                SynlyEngine.uiState.collect { ui ->
                    val state = ui.state
                    val device = ui.connectedDevice
                    val target = ui.targetLabel
                    val bluetooth = SynlyEngine.hasBluetoothPath()
                    if (lastBluetoothPath != bluetooth) {
                        lastBluetoothPath = bluetooth
                        evaluateWifiAvailability()
                    }
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
        stopMonitoringWifi()
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
            Intent(this, ClipboardReadActivity::class.java),
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

    private fun monitorWifiAvailability() {
        if (networkCallback != null) return
        val manager = getSystemService(ConnectivityManager::class.java) ?: return
        connectivityManager = manager
        val callback = object : ConnectivityManager.NetworkCallback() {
            override fun onAvailable(network: Network) {
                scope.launch { evaluateWifiAvailability() }
            }

            override fun onLost(network: Network) {
                scope.launch { evaluateWifiAvailability() }
            }
        }
        val request = NetworkRequest.Builder()
            .addTransportType(NetworkCapabilities.TRANSPORT_WIFI)
            .build()
        runCatching { manager.registerNetworkCallback(request, callback) }
            .onSuccess {
                networkCallback = callback
                scope.launch { evaluateWifiAvailability() }
            }
            .onFailure {
                connectivityManager = null
                SynlyLog.w(TAG, "注册 Wi-Fi 网络监听失败", it)
            }
    }

    private suspend fun evaluateWifiAvailability() {
        // 监听不可用时无法判断 Wi-Fi 状态, 保持后台同步而不是误判退出.
        if (networkCallback == null) return
        if (SynlyEngine.hasBluetoothPath() || hasWifiSideNetwork()) {
            cancelWifiExit()
        } else {
            scheduleWifiExit()
        }
    }

    private fun scheduleWifiExit() {
        if (wifiExitJob != null) return
        SynlyLog.i(TAG, "没有 Wi-Fi 与热点, ${WIFI_LOST_EXIT_DELAY_MS / 1000} 秒后停止后台同步")
        wifiExitJob = scope.launch {
            delay(WIFI_LOST_EXIT_DELAY_MS)
            wifiExitJob = null
            if (SynlyEngine.hasBluetoothPath() || hasWifiSideNetwork()) {
                SynlyLog.i(TAG, "已有蓝牙路径, Wi-Fi 或热点, 继续后台同步")
            } else {
                SynlyLog.i(TAG, "仍没有 Wi-Fi 与热点, 停止后台同步服务")
                stopSelf()
            }
        }
    }

    private fun cancelWifiExit() {
        wifiExitJob?.let {
            it.cancel()
            wifiExitJob = null
            SynlyLog.i(TAG, "已有蓝牙路径, Wi-Fi 或热点, 取消自动退出")
        }
    }

    // Wi-Fi 连接与热点都能承载局域网同步, 其中热点不会以 TRANSPORT_WIFI 网络的形式出现,
    // 所以再按本机接口判断一次.
    private suspend fun hasWifiSideNetwork(): Boolean {
        val manager = connectivityManager ?: return false
        val wifiNetwork = manager.allNetworks.any { network ->
            manager.getNetworkCapabilities(network)
                ?.hasTransport(NetworkCapabilities.TRANSPORT_WIFI) == true
        }
        return wifiNetwork || hasLocalInterface()
    }

    private suspend fun hasLocalInterface(): Boolean = withContext(Dispatchers.IO) {
        val interfaces = runCatching { NetworkInterface.getNetworkInterfaces() }.getOrNull()
            ?: return@withContext false
        while (interfaces.hasMoreElements()) {
            if (hasLocalAddress(interfaces.nextElement())) return@withContext true
        }
        false
    }

    private fun hasLocalAddress(networkInterface: NetworkInterface): Boolean {
        val name = networkInterface.name ?: return false
        val active = runCatching {
            networkInterface.isUp && !networkInterface.isLoopback
        }.getOrDefault(false)
        if (!active) return false
        if (NON_LOCAL_INTERFACE_PREFIXES.any { name.startsWith(it) }) return false
        if (AP_INTERFACE_PREFIXES.any { name.startsWith(it) }) return true
        val addresses = runCatching { networkInterface.inetAddresses }.getOrNull() ?: return false
        while (addresses.hasMoreElements()) {
            val address = addresses.nextElement()
            if (address is Inet4Address && !address.isLoopbackAddress) return true
        }
        return false
    }

    private fun stopMonitoringWifi() {
        wifiExitJob?.cancel()
        wifiExitJob = null
        val manager = connectivityManager
        val callback = networkCallback
        if (manager != null && callback != null) {
            runCatching { manager.unregisterNetworkCallback(callback) }
        }
        networkCallback = null
        connectivityManager = null
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
}
