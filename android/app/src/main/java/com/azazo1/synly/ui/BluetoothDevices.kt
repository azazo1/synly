package com.azazo1.synly.ui

import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.padding
import androidx.compose.material3.Button
import androidx.compose.material3.Card
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.OutlinedButton
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.runtime.Composable
import androidx.compose.runtime.DisposableEffect
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.rememberCoroutineScope
import androidx.compose.runtime.rememberUpdatedState
import androidx.compose.runtime.setValue
import androidx.compose.ui.Modifier
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.unit.dp
import com.azazo1.synly.core.SynlyTarget
import com.azazo1.synly.core.SynlyEngine
import com.azazo1.synly.core.SynlyLog
import java.util.concurrent.atomic.AtomicLong
import kotlinx.coroutines.CancellationException
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.Job
import kotlinx.coroutines.TimeoutCancellationException
import kotlinx.coroutines.ensureActive
import kotlinx.coroutines.launch
import kotlinx.coroutines.withContext
import kotlinx.coroutines.withTimeout
import kotlinx.coroutines.sync.Mutex
import kotlinx.coroutines.sync.withLock
import uniffi.synly_core.FfiBluetoothAvailability
import uniffi.synly_core.FfiBluetoothPeer
import uniffi.synly_core.bluetoothAvailability
import uniffi.synly_core.bluetoothPairedDevices
import uniffi.synly_core.bluetoothQueryService

private data class BluetoothChoice(val peer: FfiBluetoothPeer, val connectable: Boolean, val detail: String)
// 取消 coroutine 不会终止正在执行的阻塞 FFI, 槽位到实际返回才释放.
private val bluetoothQueryGate = Mutex()

// 已配对设备仍须查询项目服务, 名称和地址不建立应用信任.
@Composable
fun BluetoothDevices(busy: Boolean, recentTargets: List<SynlyTarget>, onConnect: (SynlyTarget) -> Unit) {
    val context = LocalContext.current
    val scope = rememberCoroutineScope()
    val epoch = remember { AtomicLong(0) }
    var query by remember { mutableStateOf<Job?>(null) }
    var rows by remember { mutableStateOf<List<BluetoothChoice>>(emptyList()) }
    var status by remember { mutableStateOf("点击刷新查询系统已配对设备的 Synly 服务") }
    val currentBusy by rememberUpdatedState(busy)
    val connect by rememberUpdatedState(onConnect)
    val currentRecent by rememberUpdatedState(recentTargets)
    val requestPermission = rememberBluetoothPermissionRequest { status = it }
    fun cancelQuery() {
        if (query != null) SynlyLog.i("Synly/Bluetooth", "撤销手动蓝牙刷新")
        epoch.incrementAndGet()
        query?.cancel()
        query = null
    }
    fun refresh() {
        if (currentBusy || query != null) return
        val generation = epoch.incrementAndGet()
        SynlyLog.i("Synly/Bluetooth", "开始手动查询系统已配对设备")
        rows = emptyList()
        status = "读取系统蓝牙状态与配对记录中"
        query = scope.launch {
            try {
                SynlyEngine.init(context.applicationContext)
                val availability = withTimeout(5000) { withContext(Dispatchers.IO) { bluetoothAvailability() } }
                when (availability) {
                    FfiBluetoothAvailability.DISABLED -> { status = "系统蓝牙已关闭, 请打开系统蓝牙设置"; return@launch }
                    FfiBluetoothAvailability.PERMISSION_DENIED -> { status = "附近设备权限未授予, 请在应用权限设置中允许"; return@launch }
                    FfiBluetoothAvailability.UNSUPPORTED -> { status = "当前设备不支持经典蓝牙 RFCOMM"; return@launch }
                    FfiBluetoothAvailability.AVAILABLE -> Unit
                }
                val paired = withTimeout(5000) { withContext(Dispatchers.IO) { bluetoothPairedDevices() } }
                    .distinctBy { it.address }.sortedBy { it.address }
                if (paired.isEmpty()) { status = "没有系统已配对设备, 请先在系统蓝牙设置配对"; return@launch }
                rows = paired.take(16).map { BluetoothChoice(it, false, "等待服务查询") }
                for ((index, peer) in paired.take(16).withIndex()) {
                    ensureActive()
                    if (epoch.get() != generation || currentBusy) return@launch
                    status = "查询 Synly 服务 ${index + 1}/${minOf(paired.size, 16)}"
                    rows = rows.map { if (it.peer.address == peer.address) it.copy(detail = "查询中") else it }
                    val choice = try {
                        val found = withTimeout(12000) { withContext(Dispatchers.IO) {
                            bluetoothQueryGate.withLock { ensureActive(); bluetoothQueryService(peer) }
                        } }
                        BluetoothChoice(peer, found, if (found) "Synly 服务可用, 连接后验证应用身份" else "未找到 Synly 服务, 请在对端开启蓝牙接入")
                    } catch (timeout: TimeoutCancellationException) { ensureActive(); BluetoothChoice(peer, false, "Synly 服务查询超时") }
                    catch (cancelled: CancellationException) { throw cancelled }
                    catch (error: Exception) { BluetoothChoice(peer, false, error.message ?: "服务查询失败, 设备可能离线") }
                    ensureActive()
                    if (epoch.get() != generation) return@launch
                    rows = rows.map { if (it.peer.address == peer.address) choice else it }
                }
                status = "已查询 ${rows.size}/${paired.size} 个已配对设备, ${rows.count { it.connectable }} 个 Synly 服务可用"
                SynlyLog.i("Synly/Bluetooth", status)
            } catch (timeout: TimeoutCancellationException) { ensureActive(); if (epoch.get() == generation) status = "读取系统蓝牙状态或配对记录超时, 可稍后刷新" }
            catch (cancelled: CancellationException) { throw cancelled }
            catch (error: Exception) { if (epoch.get() == generation) status = "蓝牙刷新失败: ${error.message}" }
            finally { if (epoch.get() == generation) query = null }
        }
    }
    LaunchedEffect(busy) { if (busy) { cancelQuery(); status = "会话运行或连接期间暂停手动蓝牙查询, 请先断开" } }
    DisposableEffect(Unit) { onDispose { cancelQuery() } }
    Card {
        Column(Modifier.padding(16.dp), verticalArrangement = Arrangement.spacedBy(8.dp)) {
            Text("已配对蓝牙设备", style = MaterialTheme.typography.titleMedium)
            Text("先在系统设置配对并开启对端 Synly 蓝牙接入. 名称和地址仅作发现线索, 连接后验证应用身份.", style = MaterialTheme.typography.bodySmall)
            Text(status, style = MaterialTheme.typography.bodySmall)
            Row(horizontalArrangement = Arrangement.spacedBy(8.dp)) {
                Button(onClick = {
                    if (query != null) { cancelQuery(); status = "蓝牙刷新已取消" }
                    else requestPermission { refresh() }
                }, enabled = !busy || query != null) { Text(if (query != null) "取消刷新" else "刷新蓝牙") }
                OutlinedButton(onClick = { openBluetoothSettings(context) { status = it } }) { Text("系统蓝牙设置") }
            }
            TextButton(onClick = { openBluetoothAppPermissions(context) { status = it } }) { Text("附近设备权限设置") }
            currentRecent.filter { it.bluetoothAddress != null }.forEach { target ->
                OutlinedButton(onClick = { requestPermission { if (!currentBusy) { cancelQuery(); connect(target) } } }, enabled = !busy, modifier = Modifier.fillMaxWidth()) {
                    Text("重连 ${SynlyEngine.targetLabel(context, target)}")
                }
            }
            rows.forEach { row ->
                Column(Modifier.fillMaxWidth(), verticalArrangement = Arrangement.spacedBy(4.dp)) {
                    Text(row.peer.name.ifBlank { row.peer.address }, style = MaterialTheme.typography.titleSmall)
                    Text("${row.peer.address} | ${row.detail}", style = MaterialTheme.typography.bodySmall)
                    OutlinedButton(onClick = { requestPermission {
                        if (!currentBusy) {
                            cancelQuery()
                            val known = currentRecent.firstOrNull { it.bluetoothAddress == row.peer.address }
                            connect(SynlyTarget(emptyList(), 0, known?.peerDeviceId, row.peer.address))
                        }
                    } }, enabled = row.connectable && !busy) { Text("蓝牙连接") }
                }
            }
        }
    }
}
