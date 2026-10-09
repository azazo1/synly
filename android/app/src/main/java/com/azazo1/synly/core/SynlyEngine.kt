package com.azazo1.synly.core

import android.content.Context
import android.util.Log
import com.azazo1.synly.SynlyApplication
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.SupervisorJob
import kotlinx.coroutines.launch
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.flow.update
import uniffi.synly_core.FfiClientConfig
import uniffi.synly_core.FfiClientEvent
import uniffi.synly_core.FfiClientHandle
import uniffi.synly_core.FfiClientListener
import uniffi.synly_core.FfiClientState
import uniffi.synly_core.FfiClientTarget
import uniffi.synly_core.FfiClipboardFile
import uniffi.synly_core.FfiClipboardMode
import uniffi.synly_core.FfiPathPolicy
import uniffi.synly_core.FfiDiscoveryConfig
import uniffi.synly_core.FfiDiscoveredPeer
import uniffi.synly_core.FfiLogListener
import uniffi.synly_core.browseDevices
import uniffi.synly_core.initTracing
import uniffi.synly_core.startClient
import uniffi.synly_core.registerBluetoothProvider

object SynlyEngine {
    private const val TAG = "Synly"

    @Volatile
    private var handle: FfiClientHandle? = null

    @Volatile
    private var initialized = false

    @Volatile
    private var currentTarget: SynlyTarget? = null

    private val scope = CoroutineScope(SupervisorJob() + Dispatchers.IO)

    private val _uiState = MutableStateFlow(SynlyUiState())
    val uiState: StateFlow<SynlyUiState> = _uiState

    val logs: StateFlow<List<LogEntry>> = SynlyLog.entries

    private val logListener = object : FfiLogListener {
        override fun log(level: String, target: String, message: String) {
            val priority = when (level.lowercase()) {
                "error" -> Log.ERROR
                "warn" -> Log.WARN
                "debug" -> Log.DEBUG
                "trace" -> Log.VERBOSE
                else -> Log.INFO
            }
            Log.println(priority, "$TAG/$target", message)
            SynlyLog.append(level, "$TAG/$target", message)
        }
    }

    private val listener = object : FfiClientListener {
        override fun onEvent(event: FfiClientEvent) {
            handleEvent(event)
        }
    }

    fun init(context: Context) {
        if (initialized) return
        synchronized(this) {
            if (initialized) return
            runCatching { initTracing(logListener) }
                .onFailure { SynlyLog.w(TAG, "初始化 tracing 失败", it) }
            runCatching { registerBluetoothProvider(BluetoothBackend(context.applicationContext)) }
                .onFailure { SynlyLog.w(TAG, "初始化系统蓝牙提供者失败, 局域网仍可使用", it) }
            initialized = true
        }
    }

    fun start(context: Context, allowAutoReconnect: Boolean = true) {
        val settings = SettingsStore.load(context)
        val target = settings.lastTarget ?: run {
            SynlyLog.i(TAG, "尚无目标设备, 等待用户连接")
            return
        }
        if (allowAutoReconnect && !settings.autoReconnect) {
            SynlyLog.i(TAG, "自动重连已关闭, 跳过上次目标设备")
            return
        }
        if (target == currentTarget) {
            SynlyLog.i(TAG, "目标设备未变化, 忽略重复连接请求")
            return
        }
        currentTarget = target
        _uiState.update {
            it.copy(
                state = FfiClientState.CONNECTING,
                connectedDevice = null,
                transportSummary = null,
                targetLabel = targetLabel(context, target),
                pinRequest = null,
                bluetoothAuthorization = null,
                canSend = false,
                canReceive = false,
            )
        }
        scope.launch { startInternal(context) }
    }

    fun applyDeviceName(context: Context, newName: String): String {
        val name = newName.trim()
        val settings = SettingsStore.load(context)
        val identityName = IdentityStore.getDeviceName(context)
        val effectiveName = if (name.isEmpty()) identityName ?: DEFAULT_DEVICE_NAME else name
        if (settings.deviceName != effectiveName) {
            SettingsStore.save(context, settings.copy(deviceName = effectiveName))
        }
        if (handle != null && effectiveName != identityName) {
            scope.launch {
                runCatching { handle?.stop() }
                handle = null
                currentTarget = null
                startInternal(context)
            }
        }
        return effectiveName
    }

    private fun startInternal(context: Context) {
        val settings = SettingsStore.load(context)
        val target = settings.lastTarget ?: return
        _uiState.update { it.copy(targetLabel = targetLabel(context, target)) }
        val identity = IdentityStore.getOrCreate(context)
        val trusted = TrustedDeviceStore.list(context)
        val config = FfiClientConfig(
            device = identity,
            trustedDevices = trusted,
            maxMetaLen = 20u * 1024u * 1024u,
            maxFrameDataLen = 128u * 1024u * 1024u,
            maxClipboardBinaryLen = settings.maxClipboardBytes.toUInt(),
            clipboardMode = settings.clipboardMode,
            clipboardPath = settings.clipboardPath,
            instanceName = null,
            requestTrust = true,
            bluetoothEnabled = settings.bluetoothEnabled,
            discovery = discoveryConfig(settings),
        )
        val ffiTarget = FfiClientTarget(
            addresses = target.addresses,
            port = target.port.toUShort(),
            peerDeviceId = target.peerDeviceId,
            bluetoothAddress = target.bluetoothAddress,
        )
        runCatching {
            handle?.stop()
            handle = startClient(config, ffiTarget, listener)
            currentTarget = target
            SynlyLog.i(TAG, "客户端已启动: ${target.addresses.joinToString()}:${target.port}")
        }.onFailure { error ->
            handle = null
            currentTarget = null
            SynlyLog.e(TAG, "启动客户端失败", error)
            clearUiState()
            _uiState.update { it.copy(lastMessage = error.message ?: "启动客户端失败") }
        }
    }

    fun stop() {
        val handleToStop = handle
        handle = null
        currentTarget = null
        clearUiState()
        if (handleToStop != null) {
            scope.launch {
                runCatching { handleToStop.stop() }
            }
        }
    }

    fun disconnect(context: Context) {
        val handleToStop = handle
        handle = null
        currentTarget = null
        val settings = SettingsStore.load(context).copy(lastTarget = null)
        SettingsStore.save(context, settings)
        clearUiState()
        if (handleToStop != null) {
            scope.launch {
                runCatching { handleToStop.stop() }
                if (handle == null) {
                    clearUiState()
                }
            }
        }
    }

    fun reloadConfiguration(context: Context) {
        restartClient(context, requireAutoReconnect = true)
    }

    fun reconnect(context: Context) {
        restartClient(context, requireAutoReconnect = false)
    }

    private fun restartClient(context: Context, requireAutoReconnect: Boolean) {
        val oldHandle = handle
        handle = null
        currentTarget = null
        if (oldHandle != null) {
            scope.launch {
                runCatching { oldHandle.stop() }
            }
        }
        val settings = SettingsStore.load(context)
        if (settings.lastTarget == null || (requireAutoReconnect && !settings.autoReconnect)) {
            clearUiState()
            return
        }
        start(context, allowAutoReconnect = requireAutoReconnect)
    }

    private fun clearUiState() {
        _uiState.update {
            it.copy(
                state = null,
                connectedDevice = null,
                targetLabel = null,
                transportSummary = null,
                pinRequest = null,
                bluetoothAuthorization = null,
                canSend = false,
                canReceive = false,
            )
        }
    }

    fun authorizeBluetooth(requestId: String, accepted: Boolean, remember: Boolean) {
        if (_uiState.value.bluetoothAuthorization?.requestId != requestId) return
        runCatching { handle?.authorizeBluetooth(requestId, accepted, remember) }
            .onFailure { SynlyLog.e(TAG, "提交蓝牙应用授权失败", it) }
        _uiState.update { it.copy(bluetoothAuthorization = null) }
    }

    fun submitPin(pin: String) {
        runCatching { handle?.submitPin(pin) }
            .onFailure { SynlyLog.e(TAG, "提交 PIN 失败", it) }
    }

    fun cancelPin() {
        runCatching { handle?.cancelPin() }
    }

    fun setClipboardPath(policy: FfiPathPolicy) {
        runCatching { handle?.setClipboardPath(policy) }.onFailure { SynlyLog.e(TAG, "更新剪贴板路径失败", it) }
    }

    fun setClipboardMode(mode: FfiClipboardMode) {
        runCatching { handle?.setClipboardMode(mode) }
            .onFailure { SynlyLog.e(TAG, "更新剪贴板模式失败", it) }
    }

    fun sendClipboard(payload: ClipboardPayload): Boolean {
        if (payload.isEmpty()) return false
        val result = runCatching {
            val files = payload.files.map { FfiClipboardFile(it.name, it.bytes) }
            handle?.sendClipboard(payload.text, payload.html, payload.imagePng, files)
        }
        return result
            .onFailure { SynlyLog.e(TAG, "发送剪贴板失败", it) }
            .isSuccess && handle != null
    }

    fun canSend(): Boolean = _uiState.value.canSend

    fun publishMessage(message: String) {
        _uiState.update { it.copy(lastMessage = message) }
    }

    fun dismissPinRequest() {
        _uiState.update { it.copy(pinRequest = null) }
    }

    fun refreshTrustedDevices(context: Context) {
        runCatching {
            handle?.updateTrustedDevices(TrustedDeviceStore.list(context))
        }.onFailure { SynlyLog.e(TAG, "更新可信设备失败", it) }
    }

    fun buildVersion(): String =
        runCatching { uniffi.synly_core.buildVersion() }.getOrDefault("unknown")

    fun browseDevices(context: Context, timeoutMs: Long): List<FfiDiscoveredPeer> {
        val settings = SettingsStore.load(context)
        return browseDevices(discoveryConfig(settings), timeoutMs.toULong())
    }

    private fun discoveryConfig(settings: SynlySettings): FfiDiscoveryConfig {
        val lndServerUrl = if (settings.lndEnabled) settings.lndServerUrl else null
        val lndBearerToken = if (settings.lndEnabled) settings.lndBearerToken else null
        return FfiDiscoveryConfig(
            mdnsEnabled = settings.mdnsEnabled,
            lndServerUrl = lndServerUrl,
            lndBearerToken = lndBearerToken,
            lndDiscoveryDomain = if (settings.lndEnabled) settings.lndDiscoveryDomain else null,
        )
    }

    fun connect(context: Context, target: SynlyTarget) {
        val settings = SettingsStore.load(context).copy(lastTarget = target)
        SettingsStore.save(context, settings)
        start(context, allowAutoReconnect = false)
    }

    fun targetLabel(context: Context, target: SynlyTarget): String {
        val trustedName = target.peerDeviceId
            ?.let { id ->
                TrustedDeviceStore.list(context).firstOrNull { it.deviceId == id }?.deviceName
            }
        return trustedName ?: target.bluetoothAddress?.let { "蓝牙 $it" } ?: "${target.addresses.joinToString(", ")}:${target.port}"
    }

    private fun rememberConnectedAddress(
        address: String?,
        remotePort: Int?,
        remoteDeviceId: String,
    ) {
        val context = SynlyApplication.instance ?: return
        val settings = SettingsStore.load(context)
        val target = settings.lastTarget ?: return
        if (target.peerDeviceId != null && target.peerDeviceId != remoteDeviceId) return
        if (target.bluetoothAddress != null) {
            val updated = target.copy(peerDeviceId = remoteDeviceId)
            val recent = listOf(updated) + settings.recentTargets.filter { it.bluetoothAddress != target.bluetoothAddress }
            SettingsStore.save(context, settings.copy(lastTarget = updated, recentTargets = recent))
            currentTarget = updated
            return
        }
        val normalized = address?.trim()?.takeIf { it.isNotEmpty() }
            ?: target.addresses.firstOrNull()
            ?: return
        val effectivePort = remotePort?.takeIf { it in 1..65535 } ?: target.port
        val addresses = buildList {
            add(normalized)
            target.addresses.forEach { candidate ->
                if (candidate != normalized) add(candidate)
            }
        }
        val updatedTarget = target.copy(
            addresses = addresses,
            port = effectivePort,
            peerDeviceId = remoteDeviceId,
        )
        val recentTargets = buildList {
            add(updatedTarget)
            settings.recentTargets.forEach { recent ->
                val sameDevice = recent.peerDeviceId == remoteDeviceId ||
                    (recent.peerDeviceId == null && recent.port == target.port &&
                        recent.addresses.any { it in target.addresses })
                if (!sameDevice) add(recent)
            }
        }
        SettingsStore.save(context, settings.copy(lastTarget = updatedTarget, recentTargets = recentTargets))
        currentTarget = updatedTarget
        SynlyLog.i(TAG, "已记忆最近成功对侧: $normalized:$effectivePort")
    }

    private fun handleEvent(event: FfiClientEvent) {
        if (handle == null && currentTarget == null) return
        when (event) {
            is FfiClientEvent.StateChanged -> {
                _uiState.update { it.copy(state = event.state) }
            }

            is FfiClientEvent.PinRequired -> {
                _uiState.update {
                    it.copy(
                        pinRequest = PinRequest(
                            requestId = event.requestId,
                            bootstrapShort = event.bootstrapShort,
                            bootstrapRandomart = event.bootstrapRandomart,
                            sessionShort = event.sessionShort,
                            sessionRandomart = event.sessionRandomart,
                        ),
                    )
                }
            }

            is FfiClientEvent.BluetoothAuthorizationRequired -> {
                _uiState.update { it.copy(pinRequest = null, bluetoothAuthorization = BluetoothAuthorization(
                    requestId = event.requestId, displayName = event.remote.deviceName, deviceId = event.remote.deviceId,
                    fingerprint = event.fingerprint, systemAddress = event.systemAddress,
                    changedIdentity = event.changedIdentity, capabilitiesSummary = event.capabilitiesSummary,
                )) }
            }

            is FfiClientEvent.Connected -> {
                _uiState.update {
                    it.copy(
                        state = FfiClientState.CONNECTED,
                        connectedDevice = event.remote.deviceName,
                        targetLabel = event.remote.deviceName,
                        pinRequest = null,
                        bluetoothAuthorization = null,
                        lastMessage = null,
                        canSend = event.clientToHost,
                        canReceive = event.hostToClient,
                    )
                }
                rememberConnectedAddress(
                    event.remoteAddress,
                    event.remotePort?.toInt(),
                    event.remote.deviceId,
                )
            }

            is FfiClientEvent.TransportChanged -> {
                val links = listOfNotNull(if (event.lanAvailable) "局域网" else null, if (event.bluetoothAvailable) "蓝牙" else null).joinToString(" + ")
                _uiState.update { it.copy(transportSummary = "主控制 ${event.primary} / 已接入 $links / 剪贴板 ${event.clipboardStatus}") }
            }

            is FfiClientEvent.ClipboardReceived -> {
                val payload = ClipboardPayload(
                    text = event.text,
                    html = event.html,
                    imagePng = event.imagePng,
                    files = event.files.map { ClipboardFile(it.name, it.bytes) },
                )
                val context = SynlyApplication.instance
                val deliveryHandle = handle
                // 文件缓存与系统剪贴板写入离开 FFI 回调线程, 控制和收包继续运行.
                scope.launch {
                    val applied = context != null && runCatching {
                        ClipboardWriter.applyRemote(context, payload, strict = event.deliveryId != null)
                    }.onFailure { SynlyLog.w(TAG, "应用远端剪贴板失败", it) }.getOrDefault(false)
                    event.deliveryId?.let { id ->
                        runCatching { deliveryHandle?.confirmClipboard(id, applied) }
                            .onFailure { SynlyLog.w(TAG, "发送剪贴板应用回执失败", it) }
                    }
                    if (applied && handle === deliveryHandle) _uiState.update {
                        it.copy(
                            lastReceivedText = event.text?.take(200),
                            lastReceivedImagePng = event.imagePng,
                        )
                    }
                }
            }

            is FfiClientEvent.TrustEstablished -> {
                val context = SynlyApplication.instance
                if (context != null) {
                    TrustedDeviceStore.add(context, event.device)
                    runCatching {
                        handle?.updateTrustedDevices(TrustedDeviceStore.list(context))
                    }
                }
            }

            is FfiClientEvent.Disconnected -> {
                _uiState.update {
                    it.copy(
                        state = null,
                        connectedDevice = null,
                        transportSummary = null,
                        pinRequest = null,
                        bluetoothAuthorization = null,
                        canSend = false,
                        canReceive = false,
                    )
                }
            }

            is FfiClientEvent.PairingFailed -> {
                // core 在配对终止后不再自动重连, 清空句柄以便下次重新启动
                handle = null
                currentTarget = null
                _uiState.update {
                    it.copy(
                        state = null,
                        targetLabel = null,
                transportSummary = null,
                        bluetoothAuthorization = null,
                        lastMessage = event.message,
                    )
                }
            }
        }
    }
}
