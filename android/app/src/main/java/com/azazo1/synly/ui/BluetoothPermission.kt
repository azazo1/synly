package com.azazo1.synly.ui

import android.content.Context
import android.content.Intent
import android.content.pm.PackageManager
import android.net.Uri
import android.provider.Settings
import androidx.activity.compose.rememberLauncherForActivityResult
import androidx.activity.result.contract.ActivityResultContracts
import androidx.compose.runtime.Composable
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.rememberUpdatedState
import androidx.compose.runtime.setValue
import androidx.compose.ui.platform.LocalContext
import com.azazo1.synly.core.BluetoothBackend

fun missingBluetoothPermissions(context: Context): Array<String> = BluetoothBackend.runtimePermissions()
    .filter { context.checkSelfPermission(it) != PackageManager.PERMISSION_GRANTED }.toTypedArray()

@Composable
fun rememberBluetoothPermissionRequest(onDenied: (String) -> Unit): ((() -> Unit) -> Unit) {
    val context = LocalContext.current
    val report by rememberUpdatedState(onDenied)
    var pending by remember { mutableStateOf<(() -> Unit)?>(null) }
    val launcher = rememberLauncherForActivityResult(ActivityResultContracts.RequestMultiplePermissions()) {
        val action = pending
        pending = null
        if (missingBluetoothPermissions(context).isEmpty()) action?.invoke()
        else report("附近设备权限未授予. 可在应用权限设置中允许后重试, 局域网仍可使用.")
    }
    return { action ->
        if (pending == null) {
            val permissions = missingBluetoothPermissions(context)
            if (permissions.isEmpty()) action()
            else {
                pending = action
                launcher.launch(permissions)
            }
        }
    }
}

fun openBluetoothSettings(context: Context, report: (String) -> Unit) {
    runCatching { context.startActivity(Intent(Settings.ACTION_BLUETOOTH_SETTINGS)) }
        .onFailure { report("无法打开系统蓝牙设置: ${it.message}") }
}

fun openBluetoothAppPermissions(context: Context, report: (String) -> Unit) {
    runCatching { context.startActivity(Intent(Settings.ACTION_APPLICATION_DETAILS_SETTINGS, Uri.parse("package:${context.packageName}"))) }
        .onFailure { report("无法打开应用权限设置: ${it.message}") }
}
