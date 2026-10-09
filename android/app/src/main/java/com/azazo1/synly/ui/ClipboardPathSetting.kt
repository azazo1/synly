package com.azazo1.synly.ui

import androidx.compose.runtime.Composable
import androidx.compose.foundation.layout.Column
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.RadioButton
import androidx.compose.material3.Text
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.selection.selectable
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.semantics.Role
import uniffi.synly_core.FfiPathPolicy

internal fun FfiPathPolicy.label(): String = when (this) {
    FfiPathPolicy.AUTO -> "自动保持健康路径"
    FfiPathPolicy.PREFER_BLUETOOTH -> "蓝牙优先"
    FfiPathPolicy.LAN_ONLY -> "仅局域网"
    FfiPathPolicy.BLUETOOTH_ONLY -> "仅蓝牙"
}

@Composable
internal fun ClipboardPathSetting(selected: FfiPathPolicy, onSelect: (FfiPathPolicy) -> Unit) {
    Column {
        Text("剪贴板路径", style = MaterialTheme.typography.titleSmall)
        FfiPathPolicy.entries.forEach { policy ->
            Row(
                Modifier.fillMaxWidth().selectable(selected = selected == policy, role = Role.RadioButton, onClick = { onSelect(policy) }),
                verticalAlignment = Alignment.CenterVertically,
            ) {
                RadioButton(selected = selected == policy, onClick = null)
                Text(policy.label())
            }
        }
        Text("双方限制取交集. 没有允许的在线路径时暂停剪贴板, 输入与主控制不受影响. 蓝牙须先启用接入并授予附近设备权限.", style = MaterialTheme.typography.bodySmall)
    }
}
