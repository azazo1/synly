package com.azazo1.synly.ui

import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Switch
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.runtime.Composable
import androidx.compose.runtime.getValue
import androidx.compose.runtime.rememberUpdatedState
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.unit.dp

@Composable
fun BluetoothAccessSetting(enabled: Boolean, onChanged: (Boolean) -> Unit, report: (String) -> Unit) {
    val context = LocalContext.current
    val changed by rememberUpdatedState(onChanged)
    val requestPermission = rememberBluetoothPermissionRequest(report)
    Column(verticalArrangement = Arrangement.spacedBy(4.dp)) {
        Row(Modifier.fillMaxWidth(), verticalAlignment = Alignment.CenterVertically) {
            Column(Modifier.weight(1f)) {
                Text("自动接入蓝牙副承载", style = MaterialTheme.typography.titleSmall)
                Text("允许同一设备的在线局域网会话添加安全蓝牙路径. 修改后重建当前连接.", style = MaterialTheme.typography.bodySmall)
            }
            Switch(checked = enabled, onCheckedChange = { desired ->
                if (desired) requestPermission { changed(true) } else changed(false)
            })
        }
        TextButton(onClick = { openBluetoothAppPermissions(context, report) }) { Text("附近设备权限设置") }
    }
}
