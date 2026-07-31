package com.azazo1.synly.ui

import android.content.Intent
import android.provider.Settings
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.foundation.lazy.items
import androidx.compose.foundation.text.KeyboardOptions
import androidx.compose.material3.AlertDialog
import androidx.compose.material3.Button
import androidx.compose.material3.Card
import androidx.compose.material3.FilterChip
import androidx.compose.material3.HorizontalDivider
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.OutlinedButton
import androidx.compose.material3.OutlinedTextField
import androidx.compose.material3.Scaffold
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.runtime.Composable
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.rememberCoroutineScope
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.text.font.FontFamily
import androidx.compose.ui.text.input.KeyboardType
import androidx.compose.ui.unit.dp
import androidx.lifecycle.compose.collectAsStateWithLifecycle
import com.azazo1.synly.core.SettingsStore
import com.azazo1.synly.core.SynlyEngine
import com.azazo1.synly.core.SynlyTarget
import com.azazo1.synly.core.TrustedDeviceStore
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.launch
import uniffi.synly_core.FfiClientState
import uniffi.synly_core.FfiClipboardMode
import uniffi.synly_core.FfiDiscoveredPeer

@Composable
fun MainScreen() {
    val context = LocalContext.current
    val uiState by SynlyEngine.uiState.collectAsStateWithLifecycle()
    var peers by remember { mutableStateOf<List<FfiDiscoveredPeer>>(emptyList()) }
    var scanning by remember { mutableStateOf(false) }
    var manualAddress by remember { mutableStateOf("") }
    var pin by remember { mutableStateOf("") }
    val scope = rememberCoroutineScope()

    Scaffold { padding ->
        LazyColumn(
            modifier = Modifier
                .fillMaxSize()
                .padding(padding)
                .padding(16.dp),
            verticalArrangement = Arrangement.spacedBy(12.dp),
        ) {
            item { StatusCard(uiState.state, uiState.connectedDevice) }

            uiState.lastMessage?.let { message ->
                item { Text(message, color = MaterialTheme.colorScheme.error) }
            }

            item {
                Card {
                    Column(Modifier.padding(16.dp), verticalArrangement = Arrangement.spacedBy(8.dp)) {
                        Text("局域网设备", style = MaterialTheme.typography.titleMedium)
                        Row(verticalAlignment = Alignment.CenterVertically) {
                            Button(
                                onClick = {
                                    scanning = true
                                    scope.launch(Dispatchers.IO) {
                                        runCatching { SynlyEngine.browseDevices(context, 3000) }
                                            .onSuccess { peers = it }
                                            .onFailure {
                                                SynlyEngine.publishMessage(it.message ?: "扫描失败")
                                            }
                                        scanning = false
                                    }
                                },
                                enabled = !scanning,
                            ) {
                                Text(if (scanning) "扫描中" else "扫描设备")
                            }
                            Spacer(Modifier.width(12.dp))
                            OutlinedButton(
                                onClick = {
                                    val parts = manualAddress.trim().split(":")
                                    val address = parts.getOrNull(0).orEmpty()
                                    val port = parts.getOrNull(1)?.toIntOrNull() ?: return@OutlinedButton
                                    if (address.isNotBlank() && port in 1..65535) {
                                        SynlyEngine.connect(
                                            context,
                                            SynlyTarget(listOf(address), port),
                                        )
                                    }
                                },
                            ) {
                                Text("连接")
                            }
                        }
                        OutlinedTextField(
                            value = manualAddress,
                            onValueChange = { manualAddress = it },
                            label = { Text("手动地址 ip:port") },
                            singleLine = true,
                            keyboardOptions = KeyboardOptions(keyboardType = KeyboardType.Uri),
                            modifier = Modifier.fillMaxWidth(),
                        )
                    }
                }
            }

            if (peers.isNotEmpty()) {
                item { Text("发现 ${peers.size} 台设备", style = MaterialTheme.typography.titleSmall) }
                items(peers, key = { it.deviceId }) { peer ->
                    PeerCard(peer = peer, onClick = {
                        SynlyEngine.connect(
                            context,
                            SynlyTarget(peer.addresses, peer.port.toInt(), peer.deviceId),
                        )
                    })
                }
            }

            item { HorizontalDivider() }
            item { SettingsSection() }

            uiState.lastReceivedText?.let { text ->
                item {
                    Card {
                        Column(Modifier.padding(16.dp)) {
                            Text("最近收到", style = MaterialTheme.typography.titleSmall)
                            Text(text)
                        }
                    }
                }
            }
        }
    }

    uiState.pinRequest?.let { request ->
        AlertDialog(
            onDismissRequest = {
                SynlyEngine.cancelPin()
                SynlyEngine.dismissPinRequest()
            },
            title = { Text("输入配对 PIN") },
            text = {
                Column(verticalArrangement = Arrangement.spacedBy(8.dp)) {
                    Text("请核对桌面端显示的指纹:")
                    Text("本机 ${request.bootstrapShort}", fontFamily = FontFamily.Monospace)
                    Text(request.bootstrapRandomart, fontFamily = FontFamily.Monospace)
                    Text("会话 ${request.sessionShort}", fontFamily = FontFamily.Monospace)
                    Text(request.sessionRandomart, fontFamily = FontFamily.Monospace)
                    OutlinedTextField(
                        value = pin,
                        onValueChange = { input -> pin = input.filter { it.isDigit() }.take(6) },
                        label = { Text("6 位 PIN") },
                        singleLine = true,
                        keyboardOptions = KeyboardOptions(keyboardType = KeyboardType.NumberPassword),
                    )
                }
            },
            confirmButton = {
                TextButton(
                    onClick = {
                        if (pin.length == 6) {
                            SynlyEngine.submitPin(pin)
                            pin = ""
                            SynlyEngine.dismissPinRequest()
                        }
                    },
                    enabled = pin.length == 6,
                ) {
                    Text("确认")
                }
            },
            dismissButton = {
                TextButton(
                    onClick = {
                        SynlyEngine.cancelPin()
                        SynlyEngine.dismissPinRequest()
                    },
                ) {
                    Text("取消")
                }
            },
        )
    }
}

@Composable
private fun StatusCard(state: FfiClientState?, connectedDevice: String?) {
    val label = when (state) {
        FfiClientState.CONNECTING -> "连接中"
        FfiClientState.PAIRING -> "配对中"
        FfiClientState.CONNECTED -> "已连接 ${connectedDevice.orEmpty()}"
        FfiClientState.RECONNECTING -> "重连中"
        null -> "未连接"
    }
    Card {
        Text(
            label,
            style = MaterialTheme.typography.titleMedium,
            modifier = Modifier.padding(16.dp),
        )
    }
}

@Composable
private fun PeerCard(peer: FfiDiscoveredPeer, onClick: () -> Unit) {
    Card(onClick = onClick, modifier = Modifier.fillMaxWidth()) {
        Column(Modifier.padding(12.dp), verticalArrangement = Arrangement.spacedBy(4.dp)) {
            Text(peer.deviceName, style = MaterialTheme.typography.titleSmall)
            Text(
                "${peer.addresses.joinToString()}:${peer.port}  剪贴板:${peer.clipboardMode.name}",
                style = MaterialTheme.typography.bodySmall,
            )
        }
    }
}

@Composable
private fun SettingsSection() {
    val context = LocalContext.current
    var settings by remember { mutableStateOf(SettingsStore.load(context)) }

    Column(verticalArrangement = Arrangement.spacedBy(12.dp)) {
        Text("设置", style = MaterialTheme.typography.titleMedium)

        Card {
            Column(Modifier.padding(16.dp), verticalArrangement = Arrangement.spacedBy(8.dp)) {
                Text("剪贴板同步方向")
                Row(horizontalArrangement = Arrangement.spacedBy(8.dp)) {
                    FfiClipboardMode.entries.forEach { mode ->
                        FilterChip(
                            selected = settings.clipboardMode == mode,
                            onClick = {
                                settings = settings.copy(clipboardMode = mode)
                                SettingsStore.save(context, settings)
                                SynlyEngine.setClipboardMode(mode)
                            },
                            label = { Text(mode.label()) },
                        )
                    }
                }
                OutlinedTextField(
                    value = settings.deviceName,
                    onValueChange = {
                        settings = settings.copy(deviceName = it)
                        SettingsStore.save(context, settings)
                    },
                    label = { Text("设备名称") },
                    singleLine = true,
                    modifier = Modifier.fillMaxWidth(),
                )
                OutlinedTextField(
                    value = settings.lndServerUrl.orEmpty(),
                    onValueChange = {
                        settings = settings.copy(lndServerUrl = it.takeIf(String::isNotBlank))
                        SettingsStore.save(context, settings)
                    },
                    label = { Text("LND 服务器地址") },
                    singleLine = true,
                    modifier = Modifier.fillMaxWidth(),
                )
                OutlinedTextField(
                    value = settings.lndBearerToken.orEmpty(),
                    onValueChange = {
                        settings = settings.copy(lndBearerToken = it.takeIf(String::isNotBlank))
                        SettingsStore.save(context, settings)
                    },
                    label = { Text("LND Bearer Token") },
                    singleLine = true,
                    modifier = Modifier.fillMaxWidth(),
                )
                OutlinedTextField(
                    value = (settings.maxImageBytes / 1024 / 1024).toString(),
                    onValueChange = {
                        val mb = it.toLongOrNull()?.coerceIn(1, 100) ?: return@OutlinedTextField
                        settings = settings.copy(maxImageBytes = mb * 1024 * 1024)
                        SettingsStore.save(context, settings)
                    },
                    label = { Text("图片大小上限 (MB)") },
                    singleLine = true,
                    keyboardOptions = KeyboardOptions(keyboardType = KeyboardType.Number),
                    modifier = Modifier.fillMaxWidth(),
                )
            }
        }

        Card {
            Column(Modifier.padding(16.dp), verticalArrangement = Arrangement.spacedBy(8.dp)) {
                Text("权限与后台")
                OutlinedButton(
                    onClick = {
                        context.startActivity(Intent(Settings.ACTION_ACCESSIBILITY_SETTINGS))
                    },
                    modifier = Modifier.fillMaxWidth(),
                ) {
                    Text("开启无障碍服务")
                }
                OutlinedButton(
                    onClick = {
                        val intent = Intent(
                            Settings.ACTION_REQUEST_IGNORE_BATTERY_OPTIMIZATIONS,
                            android.net.Uri.parse("package:${context.packageName}"),
                        )
                        runCatching { context.startActivity(intent) }
                    },
                    modifier = Modifier.fillMaxWidth(),
                ) {
                    Text("忽略电池优化")
                }
            }
        }

        val trusted = TrustedDeviceStore.list(context)
        if (trusted.isNotEmpty()) {
            Card {
                Column(Modifier.padding(16.dp), verticalArrangement = Arrangement.spacedBy(8.dp)) {
                    Text("可信设备")
                    trusted.forEach { device ->
                        Row(
                            modifier = Modifier.fillMaxWidth(),
                            verticalAlignment = Alignment.CenterVertically,
                        ) {
                            Text(
                                device.deviceName,
                                modifier = Modifier.weight(1f),
                            )
                            TextButton(onClick = {
                                TrustedDeviceStore.remove(context, device.deviceId)
                                SynlyEngine.refreshTrustedDevices(context)
                            }) {
                                Text("撤销")
                            }
                        }
                    }
                }
            }
        }
    }
}

private fun FfiClipboardMode.label(): String {
    return when (this) {
        FfiClipboardMode.OFF -> "关闭"
        FfiClipboardMode.SEND -> "发送"
        FfiClipboardMode.RECEIVE -> "接收"
        FfiClipboardMode.BOTH -> "双向"
    }
}
