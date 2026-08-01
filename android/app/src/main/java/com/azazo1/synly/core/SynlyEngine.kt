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
import uniffi.synly_core.FfiClipboardMode
import uniffi.synly_core.FfiDiscoveryConfig
import uniffi.synly_core.FfiDiscoveredPeer
import uniffi.synly_core.FfiLogListener
import uniffi.synly_core.browseDevices
import uniffi.synly_core.initTracing
import uniffi.synly_core.startClient

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
            initialized = true
        }
    }

    fun start(context: Context) {
        val settings = SettingsStore.load(context)
        val target = settings.lastTarget ?: run {
            SynlyLog.i(TAG, "尚无目标设备, 等待用户连接")
            return
        }
        if (target == currentTarget && handle != null) {
            SynlyLog.i(TAG, "目标设备未变化, 忽略重复连接请求")
            return
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
        val identity = IdentityStore.getOrCreate(context)
        val trusted = TrustedDeviceStore.list(context)
        val config = FfiClientConfig(
            device = identity,
            trustedDevices = trusted,
            maxMetaLen = 20u * 1024u * 1024u,
            maxFrameDataLen = 128u * 1024u * 1024u,
            maxClipboardBinaryLen = settings.maxImageBytes.toUInt(),
            clipboardMode = settings.clipboardMode,
            instanceName = null,
            requestTrust = true,
            discovery = discoveryConfig(settings),
        )
        val ffiTarget = FfiClientTarget(
            addresses = target.addresses,
            port = target.port.toUShort(),
            peerDeviceId = target.peerDeviceId,
        )
        runCatching {
            handle?.stop()
            handle = startClient(config, ffiTarget, listener)
            currentTarget = target
            SynlyLog.i(TAG, "客户端已启动: ${target.addresses.joinToString()}:${target.port}")
        }.onFailure { error ->
            SynlyLog.e(TAG, "启动客户端失败", error)
            _uiState.update { it.copy(lastMessage = error.message ?: "启动客户端失败") }
        }
    }

    fun stop() {
        scope.launch {
            runCatching { handle?.stop() }
            handle = null
            currentTarget = null
            _uiState.update {
                it.copy(
                    state = null,
                    connectedDevice = null,
                    pinRequest = null,
                    canSend = false,
                    canReceive = false,
                )
            }
        }
    }

    fun submitPin(pin: String) {
        runCatching { handle?.submitPin(pin) }
            .onFailure { SynlyLog.e(TAG, "提交 PIN 失败", it) }
    }

    fun cancelPin() {
        runCatching { handle?.cancelPin() }
    }

    fun setClipboardMode(mode: FfiClipboardMode) {
        runCatching { handle?.setClipboardMode(mode) }
            .onFailure { SynlyLog.e(TAG, "更新剪贴板模式失败", it) }
    }

    fun sendClipboard(payload: ClipboardPayload) {
        if (payload.isEmpty()) return
        runCatching {
            handle?.sendClipboard(payload.text, payload.html, payload.imagePng)
        }.onFailure { SynlyLog.e(TAG, "发送剪贴板失败", it) }
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
        start(context)
    }

    private fun handleEvent(event: FfiClientEvent) {
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

            is FfiClientEvent.Connected -> {
                _uiState.update {
                    it.copy(
                        state = FfiClientState.CONNECTED,
                        connectedDevice = event.remote.deviceName,
                        pinRequest = null,
                        canSend = event.clientToHost,
                        canReceive = event.hostToClient,
                    )
                }
                if (event.clientToHost) {
                    val context = SynlyApplication.instance
                    if (context != null) {
                        val payload = ClipboardReader.readNow(context)
                        if (payload != null) sendClipboard(payload)
                    }
                }
            }

            is FfiClientEvent.ClipboardReceived -> {
                val payload = ClipboardPayload(event.text, event.html, event.imagePng)
                val context = SynlyApplication.instance
                if (context != null) {
                    ClipboardWriter.applyRemote(context, payload)
                }
                _uiState.update {
                    it.copy(
                        lastReceivedText = event.text?.take(200),
                        lastReceivedImagePng = event.imagePng,
                    )
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
                        pinRequest = null,
                        canSend = false,
                        canReceive = false,
                    )
                }
            }

            is FfiClientEvent.PairingFailed -> {
                // core 在配对终止后不再自动重连, 清空句柄以便下次重新启动
                handle = null
                currentTarget = null
                _uiState.update { it.copy(lastMessage = event.message) }
            }
        }
    }
}
