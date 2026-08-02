package com.azazo1.synly.ui

import android.content.ClipData
import android.content.ClipboardManager
import androidx.activity.compose.rememberLauncherForActivityResult
import androidx.activity.result.contract.ActivityResultContracts
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.foundation.text.KeyboardOptions
import androidx.compose.material3.AlertDialog
import androidx.compose.material3.Card
import androidx.compose.material3.Icon
import androidx.compose.material3.IconButton
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.OutlinedButton
import androidx.compose.material3.OutlinedTextField
import androidx.compose.material3.Scaffold
import androidx.compose.material3.Switch
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
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.graphics.SolidColor
import androidx.compose.ui.graphics.vector.ImageVector
import androidx.compose.ui.graphics.vector.addPathNodes
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.text.input.PasswordVisualTransformation
import androidx.compose.ui.text.input.VisualTransformation
import androidx.compose.ui.text.input.KeyboardType
import androidx.compose.ui.unit.dp
import com.azazo1.synly.core.ConfigBackup
import com.azazo1.synly.core.SettingsStore
import com.azazo1.synly.core.SynlyEngine
import com.azazo1.synly.core.TrustedDeviceStore
import java.text.SimpleDateFormat
import java.util.Date
import java.util.Locale
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.launch
import kotlinx.coroutines.withContext

@Composable
fun SettingsScreen(onBack: () -> Unit) {
    val context = LocalContext.current
    var settings by remember { mutableStateOf(SettingsStore.load(context)) }
    var trustedDevices by remember { mutableStateOf(TrustedDeviceStore.list(context)) }
    var showToken by remember { mutableStateOf(false) }
    var busy by remember { mutableStateOf(false) }
    var statusMessage by remember { mutableStateOf<String?>(null) }
    var pendingImport by remember { mutableStateOf<ConfigBackup.Backup?>(null) }
    val scope = rememberCoroutineScope()

    val exportLauncher = rememberLauncherForActivityResult(
        ActivityResultContracts.CreateDocument("application/json"),
    ) { uri ->
        if (uri != null) {
            busy = true
            statusMessage = null
            scope.launch {
                val result = withContext(Dispatchers.IO) {
                    runCatching {
                        val json = ConfigBackup.create(context)
                        val stream = context.contentResolver.openOutputStream(uri)
                            ?: error("无法打开输出文件")
                        stream.use { it.write(json.toByteArray(Charsets.UTF_8)) }
                    }
                }
                result
                    .onSuccess { statusMessage = "配置已导出" }
                    .onFailure { statusMessage = "导出失败: ${it.message ?: "未知错误"}" }
                busy = false
            }
        }
    }
    val importLauncher = rememberLauncherForActivityResult(
        ActivityResultContracts.OpenDocument(),
    ) { uri ->
        if (uri != null) {
            busy = true
            statusMessage = null
            scope.launch {
                val result = withContext(Dispatchers.IO) {
                    runCatching {
                        val raw = context.contentResolver.openInputStream(uri)?.use { input ->
                            input.readBytes().toString(Charsets.UTF_8)
                        } ?: error("无法打开输入文件")
                        ConfigBackup.parse(raw)
                    }
                }
                result
                    .onSuccess { pendingImport = it }
                    .onFailure { statusMessage = "导入失败: ${it.message ?: "未知错误"}" }
                busy = false
            }
        }
    }
    val copyConfigToClipboard: () -> Unit = {
        val clipboard = context.getSystemService(ClipboardManager::class.java)
        if (clipboard == null) {
            statusMessage = "剪贴板不可用"
        } else {
            busy = true
            statusMessage = null
            scope.launch {
                val result = withContext(Dispatchers.IO) {
                    runCatching {
                        val json = ConfigBackup.create(context)
                        clipboard.setPrimaryClip(ClipData.newPlainText("Synly 配置", json))
                    }
                }
                result
                    .onSuccess { statusMessage = "配置已复制到剪贴板" }
                    .onFailure { statusMessage = "复制失败: ${it.message ?: "未知错误"}" }
                busy = false
            }
        }
    }
    val importConfigFromClipboard: () -> Unit = {
        val clipboard = context.getSystemService(ClipboardManager::class.java)
        val text = clipboard?.primaryClip?.getItemAt(0)?.text?.toString().orEmpty()
        if (text.isBlank()) {
            statusMessage = "剪贴板中没有配置文本"
        } else {
            busy = true
            statusMessage = null
            scope.launch {
                val result = withContext(Dispatchers.IO) {
                    runCatching { ConfigBackup.parse(text) }
                }
                result
                    .onSuccess { pendingImport = it }
                    .onFailure { statusMessage = "导入失败: ${it.message ?: "未知错误"}" }
                busy = false
            }
        }
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
                    TextButton(onClick = onBack) {
                        Text("返回")
                    }
                    Text("设置", style = MaterialTheme.typography.headlineSmall)
                }
            }

            item {
                Card {
                    Column(Modifier.padding(16.dp), verticalArrangement = Arrangement.spacedBy(8.dp)) {
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
            }

            item {
                Card {
                    Column(Modifier.padding(16.dp), verticalArrangement = Arrangement.spacedBy(8.dp)) {
                        Row(
                            modifier = Modifier.fillMaxWidth(),
                            verticalAlignment = Alignment.CenterVertically,
                        ) {
                            Text("mDNS 发现", modifier = Modifier.weight(1f))
                            Switch(
                                checked = settings.mdnsEnabled,
                                onCheckedChange = {
                                    settings = settings.copy(mdnsEnabled = it)
                                    SettingsStore.save(context, settings)
                                },
                            )
                        }
                        Row(
                            modifier = Modifier.fillMaxWidth(),
                            verticalAlignment = Alignment.CenterVertically,
                        ) {
                            Text("LND 发现", modifier = Modifier.weight(1f))
                            Switch(
                                checked = settings.lndEnabled,
                                onCheckedChange = {
                                    settings = settings.copy(lndEnabled = it)
                                    SettingsStore.save(context, settings)
                                },
                            )
                        }
                        Text("LND 配置", style = MaterialTheme.typography.titleSmall)
                        OutlinedTextField(
                            value = settings.lndServerUrl.orEmpty(),
                            onValueChange = {
                                settings = settings.copy(lndServerUrl = it.takeIf(String::isNotBlank))
                                SettingsStore.save(context, settings)
                            },
                            label = { Text("LND 服务器地址") },
                            singleLine = true,
                            enabled = settings.lndEnabled,
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
                            enabled = settings.lndEnabled,
                            visualTransformation = if (showToken) {
                                VisualTransformation.None
                            } else {
                                PasswordVisualTransformation()
                            },
                            keyboardOptions = KeyboardOptions(keyboardType = KeyboardType.Password),
                            trailingIcon = {
                                IconButton(onClick = { showToken = !showToken }) {
                                    Icon(
                                        imageVector = if (showToken) {
                                            EyeOffIcon
                                        } else {
                                            EyeIcon
                                        },
                                        contentDescription = if (showToken) "隐藏 Token" else "显示 Token",
                                    )
                                }
                            },
                            modifier = Modifier.fillMaxWidth(),
                        )
                        OutlinedTextField(
                            value = settings.lndDiscoveryDomain.orEmpty(),
                            onValueChange = {
                                settings = settings.copy(lndDiscoveryDomain = it.takeIf(String::isNotBlank))
                                SettingsStore.save(context, settings)
                            },
                            label = { Text("LND Discovery Domain") },
                            singleLine = true,
                            enabled = settings.lndEnabled,
                            modifier = Modifier.fillMaxWidth(),
                        )
                    }
                }
            }

            item {
                Card {
                    Column(Modifier.padding(16.dp), verticalArrangement = Arrangement.spacedBy(8.dp)) {
                        Text("配置导入导出", style = MaterialTheme.typography.titleSmall)
                        Text(
                            "导出文件包含本机身份私钥和 LND Token, 请妥善保管",
                            style = MaterialTheme.typography.bodySmall,
                        )
                        Row(
                            modifier = Modifier.fillMaxWidth(),
                            horizontalArrangement = Arrangement.spacedBy(8.dp),
                        ) {
                            OutlinedButton(
                                onClick = {
                                    exportLauncher.launch(
                                        "synly-config-${SimpleDateFormat("yyyyMMdd-HHmmss", Locale.US).format(Date())}.json",
                                    )
                                },
                                enabled = !busy,
                                modifier = Modifier.weight(1f),
                            ) {
                                Text(if (busy) "处理中" else "导出到文件")
                            }
                            OutlinedButton(
                                onClick = { importLauncher.launch(arrayOf("*/*")) },
                                enabled = !busy,
                                modifier = Modifier.weight(1f),
                            ) {
                                Text("从文件导入")
                            }
                        }
                        Row(
                            modifier = Modifier.fillMaxWidth(),
                            horizontalArrangement = Arrangement.spacedBy(8.dp),
                        ) {
                            OutlinedButton(
                                onClick = copyConfigToClipboard,
                                enabled = !busy,
                                modifier = Modifier.weight(1f),
                            ) {
                                Text("复制到剪贴板")
                            }
                            OutlinedButton(
                                onClick = importConfigFromClipboard,
                                enabled = !busy,
                                modifier = Modifier.weight(1f),
                            ) {
                                Text("从剪贴板导入")
                            }
                        }
                        statusMessage?.let { message ->
                            Text(message, style = MaterialTheme.typography.bodySmall)
                        }
                    }
                }
            }

            if (trustedDevices.isNotEmpty()) {
                item {
                    Card {
                        Column(Modifier.padding(16.dp), verticalArrangement = Arrangement.spacedBy(8.dp)) {
                            Text("可信设备")
                            trustedDevices.forEach { device ->
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
                                        trustedDevices = TrustedDeviceStore.list(context)
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

            item {
                Card {
                    Column(Modifier.padding(16.dp), verticalArrangement = Arrangement.spacedBy(8.dp)) {
                        Text("关于", style = MaterialTheme.typography.titleSmall)
                        Text(
                            "Synly ${SynlyEngine.buildVersion()}",
                            style = MaterialTheme.typography.bodySmall,
                        )
                    }
                }
            }
        }
    }

    pendingImport?.let { backup ->
        AlertDialog(
            onDismissRequest = { pendingImport = null },
            title = { Text("导入配置") },
            text = { Text("将覆盖当前设置, 身份和可信设备. 确定继续吗?") },
            confirmButton = {
                TextButton(onClick = {
                    pendingImport = null
                    busy = true
                    statusMessage = null
                    scope.launch {
                        val result = withContext(Dispatchers.IO) {
                            runCatching {
                                ConfigBackup.apply(context, backup)
                                SynlyEngine.reloadConfiguration(context)
                            }
                        }
                        result
                            .onSuccess {
                                settings = SettingsStore.load(context)
                                trustedDevices = TrustedDeviceStore.list(context)
                                statusMessage = "配置已导入"
                            }
                            .onFailure {
                                statusMessage = "导入失败: ${it.message ?: "未知错误"}"
                            }
                        busy = false
                    }
                }) {
                    Text("导入")
                }
            },
            dismissButton = {
                TextButton(onClick = { pendingImport = null }) {
                    Text("取消")
                }
            },
        )
    }
}

private val EyeIcon: ImageVector by lazy {
    ImageVector.Builder(
        name = "Visibility",
        defaultWidth = 24.dp,
        defaultHeight = 24.dp,
        viewportWidth = 24f,
        viewportHeight = 24f,
    ).apply {
        addPath(
            pathData = addPathNodes(
                "M12,4.5C7,4.5 2.73,7.61 1,12c1.73,4.39 6,7.5 11,7.5s9.27,-3.11 11,-7.5" +
                    "c-1.73,-4.39 -6,-7.5 -11,-7.5zM12,17c-2.76,0 -5,-2.24 -5,-5s2.24,-5 5,-5" +
                    " 5,2.24 5,5 -2.24,5 -5,5zM12,9c-1.66,0 -3,1.34 -3,3s1.34,3 3,3 3,-1.34 3,-3" +
                    " -1.34,-3 -3,-3z",
            ),
            fill = SolidColor(Color.Black),
        )
    }.build()
}

private val EyeOffIcon: ImageVector by lazy {
    ImageVector.Builder(
        name = "VisibilityOff",
        defaultWidth = 24.dp,
        defaultHeight = 24.dp,
        viewportWidth = 24f,
        viewportHeight = 24f,
    ).apply {
        addPath(
            pathData = addPathNodes(
                "M12,7c2.76,0 5,2.24 5,5 0,0.65 -0.13,1.26 -0.36,1.83l2.92,2.92" +
                    "c1.51,-1.26 2.7,-2.89 3.43,-4.75 -1.73,-4.39 -6,-7.5 -11,-7.5" +
                    " -1.4,0 -2.74,0.25 -3.98,0.7l2.16,2.16C10.74,7.13 11.35,7 12,7z" +
                    "M2,4.27l2.28,2.28 0.46,0.46C3.08,8.3 1.78,10.02 1,12c1.73,4.39 6,7.5 11,7.5" +
                    " 1.55,0 3.03,-0.3 4.38,-0.84l0.42,0.42L19.73,22 21,20.73 3.27,3 2,4.27z" +
                    "M7.53,9.8l1.55,1.55c-0.05,0.21 -0.08,0.43 -0.08,0.65 0,1.66 1.34,3 3,3" +
                    " 0.22,0 0.44,-0.03 0.65,-0.08l1.55,1.55c-0.67,0.33 -1.41,0.53 -2.2,0.53" +
                    " -2.76,0 -5,-2.24 -5,-5 0,-0.79 0.2,-1.53 0.53,-2.2zM11.84,9.02l3.15,3.15" +
                    " 0.02,-0.16c0,-1.66 -1.34,-3 -3,-3l-0.17,0.01z",
            ),
            fill = SolidColor(Color.Black),
        )
    }.build()
}
