package com.azazo1.synly.core

import android.content.Context
import org.json.JSONArray
import org.json.JSONObject
import uniffi.synly_core.FfiClipboardMode
import uniffi.synly_core.FfiPathPolicy

const val DEFAULT_DEVICE_NAME = "Android 手机"

data class SynlySettings(
    val clipboardMode: FfiClipboardMode = FfiClipboardMode.BOTH,
    val clipboardPath: FfiPathPolicy = FfiPathPolicy.AUTO,
    val bluetoothEnabled: Boolean = false,
    val mdnsEnabled: Boolean = true,
    val lndEnabled: Boolean = false,
    val lndServerUrl: String? = null,
    val lndBearerToken: String? = null,
    val lndDiscoveryDomain: String? = null,
    val maxClipboardBytes: Long = 100L * 1024 * 1024,
    val maxClipboardCacheBytes: Long = 512L * 1024 * 1024,
    val deviceName: String = DEFAULT_DEVICE_NAME,
    val autoReconnect: Boolean = true,
    val lastTarget: SynlyTarget? = null,
    val recentTargets: List<SynlyTarget> = emptyList(),
)

data class SynlyTarget(
    val addresses: List<String>,
    val port: Int,
    val peerDeviceId: String? = null,
    val bluetoothAddress: String? = null,
)

object SettingsStore {
    private const val PREFS = "synly_settings"
    private const val MAX_RECENT_TARGETS = 8

    fun load(context: Context): SynlySettings {
        val prefs = context.getSharedPreferences(PREFS, Context.MODE_PRIVATE)
        SettingsMigrations.apply(prefs)
        val lastTarget = prefs.getString("last_target", null)?.let { raw ->
            runCatching { parseTarget(JSONObject(raw)) }.getOrNull()
        }
        val recentTargets = prefs.getString("recent_targets", null)?.let { raw ->
            runCatching {
                JSONArray(raw).let { array ->
                    (0 until array.length()).mapNotNull { index ->
                        array.optJSONObject(index)?.let(::parseTarget)
                    }
                }
            }.getOrDefault(emptyList())
        } ?: lastTarget?.let(::listOf).orEmpty()
        return SynlySettings(
            clipboardMode = parseMode(prefs.getString("clipboard_mode", null)),
            clipboardPath = parseClipboardPath(prefs.getString("clipboard_path", null)),
            bluetoothEnabled = prefs.getBoolean("bluetooth_enabled", false),
            mdnsEnabled = prefs.getBoolean("mdns_enabled", true),
            lndEnabled = prefs.getBoolean(
                "lnd_enabled",
                prefs.getString("lnd_server_url", null) != null,
            ),
            lndServerUrl = prefs.getString("lnd_server_url", null)?.takeIf { it.isNotBlank() },
            lndBearerToken = prefs.getString("lnd_bearer_token", null)?.takeIf { it.isNotBlank() },
            lndDiscoveryDomain = prefs.getString("lnd_discovery_domain", null)?.takeIf { it.isNotBlank() },
            maxClipboardBytes = prefs.getLong("max_clipboard_bytes", 100L * 1024 * 1024),
            maxClipboardCacheBytes = prefs.getLong(
                "max_clipboard_cache_bytes",
                512L * 1024 * 1024,
            ),
            deviceName = prefs.getString("device_name", null) ?: DEFAULT_DEVICE_NAME,
            autoReconnect = prefs.getBoolean("auto_reconnect", true),
            lastTarget = lastTarget,
            recentTargets = recentTargets.take(MAX_RECENT_TARGETS),
        )
    }

    fun save(context: Context, settings: SynlySettings) {
        val lastTarget = settings.lastTarget?.let(::targetJson)
        val recentTargets = JSONArray()
        settings.recentTargets
            .distinct()
            .take(MAX_RECENT_TARGETS)
            .forEach { recentTargets.put(targetJson(it)) }
        context.getSharedPreferences(PREFS, Context.MODE_PRIVATE)
            .edit()
            .putString("clipboard_mode", settings.clipboardMode.name)
            .putString("clipboard_path", settings.clipboardPath.name)
            .putBoolean("bluetooth_enabled", settings.bluetoothEnabled)
            .putBoolean("mdns_enabled", settings.mdnsEnabled)
            .putBoolean("lnd_enabled", settings.lndEnabled)
            .putString("lnd_server_url", settings.lndServerUrl.orEmpty())
            .putString("lnd_bearer_token", settings.lndBearerToken.orEmpty())
            .putString("lnd_discovery_domain", settings.lndDiscoveryDomain.orEmpty())
            .putLong("max_clipboard_bytes", settings.maxClipboardBytes)
            .putLong("max_clipboard_cache_bytes", settings.maxClipboardCacheBytes)
            .putString("device_name", settings.deviceName)
            .putBoolean("auto_reconnect", settings.autoReconnect)
            .putString("last_target", lastTarget?.toString().orEmpty())
            .putString("recent_targets", recentTargets.toString())
            .apply()
        ClipboardCache.prune(context)
    }

    private fun targetJson(target: SynlyTarget): JSONObject {
        val addresses = JSONArray()
        target.addresses.forEach { addresses.put(it) }
        return JSONObject()
            .put("addresses", addresses)
            .put("port", target.port)
            .put("peer_device_id", target.peerDeviceId.orEmpty())
            .put("bluetooth_address", target.bluetoothAddress.orEmpty())
    }

    private fun parseTarget(obj: JSONObject): SynlyTarget? {
        val addresses = obj.optJSONArray("addresses") ?: JSONArray()
        val port = obj.optInt("port", 0)
        val bluetooth = obj.optString("bluetooth_address").trim().takeIf { it.isNotEmpty() }?.uppercase(java.util.Locale.ROOT)
        if (bluetooth != null && (port !in 0..65535 || !android.bluetooth.BluetoothAdapter.checkBluetoothAddress(bluetooth))) return null
        val parsedAddresses = (0 until addresses.length())
            .map { addresses.optString(it).trim() }
            .filter(String::isNotBlank)
        if (bluetooth == null && (parsedAddresses.isEmpty() || port !in 1..65535)) return null
        return SynlyTarget(
            addresses = parsedAddresses,
            port = port,
            peerDeviceId = obj.optString("peer_device_id").takeIf { it.isNotBlank() },
            bluetoothAddress = bluetooth,
        )
    }

    private fun parseClipboardPath(raw: String?): FfiPathPolicy {
        if (raw == null) return FfiPathPolicy.AUTO
        return runCatching { FfiPathPolicy.valueOf(raw) }.getOrElse { error("clipboard_path 无效, 不自动放宽路径限制") }
    }

    private fun parseMode(raw: String?): FfiClipboardMode {
        return runCatching { FfiClipboardMode.valueOf(raw ?: return FfiClipboardMode.BOTH) }
            .getOrDefault(FfiClipboardMode.BOTH)
    }
}
