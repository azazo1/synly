package com.azazo1.synly.core

import org.json.JSONObject

// 导入格式版本独立于应用版本和本机 SharedPreferences schema.
internal object ConfigBackupMigrations {
    const val VERSION = 3
    fun apply(root: JSONObject) {
        val previous = root.optInt("version", 0)
        require(previous in 1..VERSION) { "不支持的配置备份版本" }
        if (previous < 2) {
            root.optJSONObject("settings")?.let { settings ->
                if (!settings.has("bluetooth_enabled")) settings.put("bluetooth_enabled", false)
            }
            root.put("version", 2)
        }
        if (previous < 3) {
            root.optJSONObject("settings")?.let { settings ->
                if (!settings.has("clipboard_path")) settings.put("clipboard_path", "AUTO")
            }
            root.put("version", 3)
        }
    }
}
