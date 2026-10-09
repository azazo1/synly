package com.azazo1.synly.ui

import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.material3.AlertDialog
import androidx.compose.material3.Checkbox
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.runtime.Composable
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.text.font.FontFamily
import androidx.compose.ui.unit.dp
import com.azazo1.synly.core.BluetoothAuthorization
import com.azazo1.synly.core.SynlyEngine

// 蓝牙首次应用身份授权, 不使用第二组 PIN.
@Composable
fun BluetoothAuthorizationDialog(request: BluetoothAuthorization) {
    var rememberPeer by remember(request.requestId) { mutableStateOf(false) }
    val reject = { SynlyEngine.authorizeBluetooth(request.requestId, false, false) }
    AlertDialog(
        onDismissRequest = reject,
        title = { Text("授权 Synly 蓝牙身份") },
        text = {
            Column(verticalArrangement = Arrangement.spacedBy(8.dp)) {
                if (request.changedIdentity) Text("警告: 同一设备 ID 的身份公钥已改变, 请核实后再授权")
                Text("系统配对已经保护无线链路, 仍需确认 Synly 应用身份")
                Text(request.displayName)
                Text("设备 ID: ${request.deviceId}", fontFamily = FontFamily.Monospace)
                Text("身份指纹: ${request.fingerprint}", fontFamily = FontFamily.Monospace)
                Text("蓝牙地址仅为发现线索: ${request.systemAddress}")
                Text(request.capabilitiesSummary)
                Row {
                    Checkbox(checked = rememberPeer, onCheckedChange = { rememberPeer = it })
                    Text("保存此身份, 后续使用长期 mTLS")
                }
            }
        },
        confirmButton = { TextButton(onClick = { SynlyEngine.authorizeBluetooth(request.requestId, true, rememberPeer) }) { Text("授权") } },
        dismissButton = { TextButton(onClick = reject) { Text("拒绝") } },
    )
}
