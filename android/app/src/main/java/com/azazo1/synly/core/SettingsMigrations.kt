package com.azazo1.synly.core

import android.content.SharedPreferences

// 独立的设置版本迁移, 旧局域网端点与剪贴板设置保持原样.
internal object SettingsMigrations {
    private const val VERSION = 3
    fun apply(prefs: SharedPreferences) {
        val previous = prefs.getInt("schema_version", 0)
        require(previous <= VERSION) { "设置版本高于当前应用支持版本" }
        if (previous < VERSION) {
            val editor = prefs.edit()
            // v1 添加可选蓝牙地址, 缺失地址的旧端点仍按 LAN 读取.
            // v2 新增副承载发现开关, 缺失时关闭, 不自动扫描用户已配对设备.
            if (previous < 2 && !prefs.contains("bluetooth_enabled")) editor.putBoolean("bluetooth_enabled", false)
            // v3 单独添加剪贴板路径, 旧方向, 端点和文件限制不变.
            if (previous < 3 && !prefs.contains("clipboard_path")) editor.putString("clipboard_path", "AUTO")
            check(editor.putInt("schema_version", VERSION).commit()) { "无法保存设置迁移版本" }
        }
    }
}
