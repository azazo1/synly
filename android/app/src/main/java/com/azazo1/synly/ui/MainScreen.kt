package com.azazo1.synly.ui

import android.content.Context
import android.content.Intent
import android.net.Uri
import android.os.PowerManager
import android.provider.Settings
import androidx.activity.compose.BackHandler
import androidx.compose.foundation.Canvas
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.PaddingValues
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.foundation.text.KeyboardOptions
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.filled.Check
import androidx.compose.material3.AlertDialog
import androidx.compose.material3.Button
import androidx.compose.material3.ButtonColors
import androidx.compose.material3.ButtonDefaults
import androidx.compose.material3.Card
import androidx.compose.material3.FilterChip
import androidx.compose.material3.HorizontalDivider
import androidx.compose.material3.Icon
import androidx.compose.material3.IconButton
import androidx.compose.material3.LocalContentColor
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.OutlinedCard
import androidx.compose.material3.OutlinedButton
import androidx.compose.material3.OutlinedTextField
import androidx.compose.material3.Scaffold
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.runtime.Composable
import androidx.compose.runtime.DisposableEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.rememberCoroutineScope
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.focus.onFocusChanged
import androidx.compose.ui.geometry.Offset
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.graphics.Path
import androidx.compose.ui.graphics.drawscope.Stroke
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.text.font.FontFamily
import androidx.compose.ui.text.input.KeyboardType
import androidx.compose.ui.unit.dp
import androidx.lifecycle.Lifecycle
import androidx.lifecycle.LifecycleEventObserver
import androidx.lifecycle.compose.LocalLifecycleOwner
import androidx.lifecycle.compose.collectAsStateWithLifecycle
import com.azazo1.synly.R
import com.azazo1.synly.core.SettingsStore
import com.azazo1.synly.core.SynlyEngine
import com.azazo1.synly.core.SynlyTarget
import com.azazo1.synly.service.ClipboardReadActivity
import com.azazo1.synly.service.ClipboardSendActivity
import com.azazo1.synly.service.ClipboardSyncService
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.launch
import uniffi.synly_core.FfiClientState
import uniffi.synly_core.FfiClipboardMode
import uniffi.synly_core.FfiDiscoverySource
import uniffi.synly_core.FfiDiscoveredPeer

@Composable
fun MainScreen() {
    var showSettings by remember { mutableStateOf(false) }
    var showLogs by remember { mutableStateOf(false) }
    BackHandler(enabled = showSettings || showLogs) {
        showSettings = false
        showLogs = false
    }
    if (showSettings) {
        SettingsScreen(onBack = { showSettings = false })
    } else if (showLogs) {
        LogsScreen(onBack = { showLogs = false })
    } else {
        HomeScreen(
            onOpenSettings = { showSettings = true },
            onOpenLogs = { showLogs = true },
        )
    }
}

@Composable
private fun HomeScreen(onOpenSettings: () -> Unit, onOpenLogs: () -> Unit) {
    val context = LocalContext.current
    val uiState by SynlyEngine.uiState.collectAsStateWithLifecycle()
    var peers by remember { mutableStateOf<List<FfiDiscoveredPeer>>(emptyList()) }
    var scanning by remember { mutableStateOf(false) }
    var manualAddress by remember { mutableStateOf("") }
    var pin by remember { mutableStateOf("") }
    var settings by remember { mutableStateOf(SettingsStore.load(context)) }
    val scope = rememberCoroutineScope()
    var batteryIgnored by remember { mutableStateOf(false) }
    var revealReceived by remember { mutableStateOf(false) }
    val lifecycleOwner = LocalLifecycleOwner.current
    DisposableEffect(lifecycleOwner) {
        val observer = LifecycleEventObserver { _, event ->
            if (event == Lifecycle.Event.ON_RESUME) {
                batteryIgnored = isIgnoringBatteryOptimizations(context)
            }
        }
        lifecycleOwner.lifecycle.addObserver(observer)
        onDispose { lifecycleOwner.lifecycle.removeObserver(observer) }
    }

    Scaffold { padding ->
        LazyColumn(
            modifier = Modifier
                .fillMaxSize()
                .padding(padding)
                .padding(16.dp),
            verticalArrangement = Arrangement.spacedBy(12.dp),
        ) {
            item {
                Row(
                    modifier = Modifier.fillMaxWidth(),
                    verticalAlignment = Alignment.CenterVertically,
                ) {
                    Text("Synly", style = MaterialTheme.typography.headlineSmall, modifier = Modifier.weight(1f))
                    TextButton(onClick = onOpenLogs) {
                        Text("日志")
                    }
                    TextButton(onClick = onOpenSettings) {
                        Text("设置")
                    }
                }
            }

            item {
                Row(
                    modifier = Modifier.fillMaxWidth(),
                    verticalAlignment = Alignment.CenterVertically,
                    horizontalArrangement = Arrangement.spacedBy(12.dp),
                ) {
                    StatusCard(
                        state = uiState.state,
                        targetLabel = uiState.targetLabel,
                        modifier = Modifier.weight(1f),
                    )
                    Button(
                        onClick = {
                            SynlyEngine.disconnect(context)
                            ClipboardSyncService.stop(context)
                        },
                        enabled = uiState.state != null,
                    ) {
                        Text("断开连接")
                    }
                }
            }

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
                                        connectSync(
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
                        if (peers.isNotEmpty()) {
                            Text(
                                "发现 ${peers.size} 台设备",
                                style = MaterialTheme.typography.titleSmall,
                            )
                            peers.forEach { peer ->
                                PeerCard(peer = peer, onClick = {
                                    connectSync(
                                        context,
                                        SynlyTarget(peer.addresses, peer.port.toInt(), peer.deviceId),
                                    )
                                })
                            }
                        }
                    }
                }
            }

            item { HorizontalDivider() }

            item {
                Card {
                    Column(Modifier.padding(16.dp), verticalArrangement = Arrangement.spacedBy(8.dp)) {
                        Text("剪贴板同步方向", style = MaterialTheme.typography.titleSmall)
                        Row(horizontalArrangement = Arrangement.spacedBy(8.dp)) {
                            FfiClipboardMode.entries.forEach { mode ->
                                FilterChip(
                                    selected = settings.clipboardMode == mode,
                                    onClick = {
                                        settings = settings.copy(clipboardMode = mode)
                                        SettingsStore.save(context, settings)
                                        SynlyEngine.setClipboardMode(mode)
                                    },
                                    label = { Text(mode.homeLabel()) },
                                )
                            }
                        }
                        OutlinedTextField(
                            value = settings.deviceName,
                            onValueChange = {
                                settings = settings.copy(deviceName = it)
                                SettingsStore.save(context, settings)
                            },
                            modifier = Modifier
                                .fillMaxWidth()
                                .onFocusChanged { focusState ->
                                    if (!focusState.isFocused) {
                                        settings = settings.copy(
                                            deviceName = SynlyEngine.applyDeviceName(
                                                context,
                                                settings.deviceName,
                                            ),
                                        )
                                    }
                                },
                            label = { Text("设备名称") },
                            singleLine = true,
                        )
                    }
                }
            }

            item {
                Card {
                    Column(Modifier.padding(16.dp), verticalArrangement = Arrangement.spacedBy(8.dp)) {
                        Text("快速发送", style = MaterialTheme.typography.titleSmall)
                        Row(
                            modifier = Modifier.fillMaxWidth(),
                            horizontalArrangement = Arrangement.spacedBy(8.dp),
                        ) {
                            Button(
                                onClick = {
                                    context.startActivity(
                                        Intent(context, ClipboardSendActivity::class.java)
                                            .setAction(ClipboardSendActivity.ACTION_PICK_FILE),
                                    )
                                },
                                modifier = Modifier.weight(1f),
                                contentPadding = PaddingValues(horizontal = 8.dp),
                            ) {
                                Text(
                                    context.getString(R.string.main_action_pick_file),
                                    maxLines = 1,
                                )
                            }
                            Button(
                                onClick = {
                                    context.startActivity(
                                        Intent(context, ClipboardSendActivity::class.java)
                                            .setAction(ClipboardSendActivity.ACTION_CAPTURE_PHOTO),
                                    )
                                },
                                modifier = Modifier.weight(1f),
                                contentPadding = PaddingValues(horizontal = 8.dp),
                            ) {
                                Text(
                                    context.getString(R.string.main_action_capture_photo),
                                    maxLines = 1,
                                )
                            }
                            Button(
                                onClick = {
                                    context.startActivity(
                                        Intent(context, ClipboardReadActivity::class.java)
                                            .putExtra(ClipboardReadActivity.EXTRA_MANUAL, true),
                                    )
                                },
                                modifier = Modifier.weight(1f),
                                contentPadding = PaddingValues(horizontal = 8.dp),
                            ) {
                                Text(
                                    context.getString(R.string.main_action_send_clipboard),
                                    maxLines = 1,
                                )
                            }
                        }
                    }
                }
            }

            item {
                Card {
                    Column(Modifier.padding(16.dp), verticalArrangement = Arrangement.spacedBy(8.dp)) {
                        Text("权限与后台", style = MaterialTheme.typography.titleSmall)
                        OutlinedButton(
                            onClick = {
                                val intent = Intent(
                                    Settings.ACTION_REQUEST_IGNORE_BATTERY_OPTIMIZATIONS,
                                    android.net.Uri.parse("package:${context.packageName}"),
                                )
                                runCatching { context.startActivity(intent) }
                            },
                            colors = permissionButtonColors(batteryIgnored),
                            modifier = Modifier.fillMaxWidth(),
                        ) {
                            if (batteryIgnored) {
                                Icon(Icons.Filled.Check, contentDescription = null)
                                Spacer(Modifier.width(8.dp))
                            }
                            Text(if (batteryIgnored) "电池优化已忽略" else "忽略电池优化")
                        }
                    }
                }
            }

            uiState.lastReceivedText?.let { text ->
                item {
                    Card {
                        Column(
                            Modifier.padding(16.dp),
                            verticalArrangement = Arrangement.spacedBy(8.dp),
                        ) {
                            Row(verticalAlignment = Alignment.CenterVertically) {
                                Text(
                                    "最近收到",
                                    style = MaterialTheme.typography.titleSmall,
                                    modifier = Modifier.weight(1f),
                                )
                                IconButton(onClick = { revealReceived = !revealReceived }) {
                                    EyeIcon(revealed = revealReceived)
                                }
                            }
                            Text(
                                text = if (revealReceived) text else maskText(text),
                                style = MaterialTheme.typography.bodyMedium,
                            )
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
private fun permissionButtonColors(enabled: Boolean): ButtonColors {
    return if (enabled) {
        ButtonDefaults.outlinedButtonColors(
            containerColor = Color(0xFFE8F5E9),
            contentColor = Color(0xFF2E7D32),
        )
    } else {
        ButtonDefaults.outlinedButtonColors()
    }
}

@Composable
private fun EyeIcon(revealed: Boolean) {
    val color = LocalContentColor.current
    Canvas(modifier = Modifier.size(24.dp)) {
        val strokeWidth = 1.8.dp.toPx()
        val halfWidth = size.width * 0.42f
        val halfHeight = size.height * 0.28f
        val centerX = size.width / 2f
        val centerY = size.height / 2f
        val eye = Path().apply {
            moveTo(centerX - halfWidth, centerY)
            cubicTo(
                centerX - halfWidth * 0.4f,
                centerY - halfHeight,
                centerX + halfWidth * 0.4f,
                centerY - halfHeight,
                centerX + halfWidth,
                centerY,
            )
            cubicTo(
                centerX + halfWidth * 0.4f,
                centerY + halfHeight,
                centerX - halfWidth * 0.4f,
                centerY + halfHeight,
                centerX - halfWidth,
                centerY,
            )
            close()
        }
        drawPath(eye, color = color, style = Stroke(width = strokeWidth))
        if (revealed) {
            drawCircle(
                color = color,
                radius = halfWidth * 0.22f,
                center = Offset(centerX, centerY),
                style = Stroke(width = strokeWidth),
            )
        } else {
            drawLine(
                color = color,
                start = Offset(centerX - halfWidth * 0.85f, centerY - halfHeight * 0.85f),
                end = Offset(centerX + halfWidth * 0.85f, centerY + halfHeight * 0.85f),
                strokeWidth = strokeWidth,
            )
        }
    }
}

private fun maskText(text: String): String {
    return text.map { "•" }.joinToString("")
}

private fun connectSync(context: Context, target: SynlyTarget) {
    SynlyEngine.connect(context, target)
    ClipboardSyncService.start(context)
}

private fun isIgnoringBatteryOptimizations(context: Context): Boolean {
    val powerManager = context.getSystemService(Context.POWER_SERVICE) as? PowerManager ?: return false
    return powerManager.isIgnoringBatteryOptimizations(context.packageName)
}

@Composable
private fun StatusCard(
    state: FfiClientState?,
    targetLabel: String?,
    modifier: Modifier = Modifier,
) {
    val label = when (state) {
        FfiClientState.CONNECTING -> targetLabel?.let { "连接 $it 中" } ?: "连接中"
        FfiClientState.PAIRING -> "配对中"
        FfiClientState.CONNECTED -> "已连接 ${targetLabel.orEmpty()}"
        FfiClientState.RECONNECTING -> targetLabel?.let { "重连 $it 中" } ?: "重连中"
        null -> "未连接"
    }
    Card(modifier = modifier) {
        Column(Modifier.padding(16.dp)) {
            Text(label, style = MaterialTheme.typography.titleMedium)
        }
    }
}

@Composable
private fun PeerCard(peer: FfiDiscoveredPeer, onClick: () -> Unit) {
    OutlinedCard(onClick = onClick, modifier = Modifier.fillMaxWidth()) {
        Column(Modifier.padding(12.dp), verticalArrangement = Arrangement.spacedBy(4.dp)) {
            Text(peer.deviceName, style = MaterialTheme.typography.titleSmall)
            Text(
                "${peer.addresses.joinToString()}:${peer.port}  剪贴板:${peer.clipboardMode.name}  来源:${peer.source.label()}",
                style = MaterialTheme.typography.bodySmall,
            )
        }
    }
}

private fun FfiClipboardMode.homeLabel(): String {
    return when (this) {
        FfiClipboardMode.OFF -> "关闭"
        FfiClipboardMode.SEND -> "发送"
        FfiClipboardMode.RECEIVE -> "接收"
        FfiClipboardMode.BOTH -> "双向"
    }
}

private fun FfiDiscoverySource.label(): String {
    return when (this) {
        FfiDiscoverySource.MDNS -> "mDNS"
        FfiDiscoverySource.LND -> "LND"
        FfiDiscoverySource.MDNS_AND_LND -> "mDNS+LND"
    }
}
