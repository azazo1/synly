package com.azazo1.synly.core

import android.content.Context
import org.json.JSONArray
import org.json.JSONObject
import uniffi.synly_core.FfiClipboardMode

const val DEFAULT_DEVICE_NAME = "Android 手机"

data class SynlySettings(
    val clipboardMode: FfiClipboardMode = FfiClipboardMode.BOTH,
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
)

data class SynlyTarget(
    val addresses: List<String>,
    val port: Int,
    val peerDeviceId: String? = null,
)

object SettingsStore {
    private const val PREFS = "synly_settings"

    fun load(context: Context): SynlySettings {
        val prefs = context.getSharedPreferences(PREFS, Context.MODE_PRIVATE)
        val targetJson = prefs.getString("last_target", null)
        val lastTarget = targetJson?.let { raw ->
            runCatching {
                val obj = JSONObject(raw)
                SynlyTarget(
                    addresses = obj.getJSONArray("addresses").let { array ->
                        (0 until array.length()).map { array.getString(it) }
                    },
                    port = obj.getInt("port"),
                    peerDeviceId = obj.optString("peer_device_id").takeIf { it.isNotBlank() },
                )
            }.getOrNull()
        }
        return SynlySettings(
            clipboardMode = parseMode(prefs.getString("clipboard_mode", null)),
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
        )
    }

    fun save(context: Context, settings: SynlySettings) {
        val target = settings.lastTarget?.let { target ->
            val addresses = JSONArray()
            target.addresses.forEach { addresses.put(it) }
            JSONObject()
                .put("addresses", addresses)
                .put("port", target.port)
                .put("peer_device_id", target.peerDeviceId.orEmpty())
                .toString()
        }
        context.getSharedPreferences(PREFS, Context.MODE_PRIVATE)
            .edit()
            .putString("clipboard_mode", settings.clipboardMode.name)
            .putBoolean("mdns_enabled", settings.mdnsEnabled)
            .putBoolean("lnd_enabled", settings.lndEnabled)
            .putString("lnd_server_url", settings.lndServerUrl.orEmpty())
            .putString("lnd_bearer_token", settings.lndBearerToken.orEmpty())
            .putString("lnd_discovery_domain", settings.lndDiscoveryDomain.orEmpty())
            .putLong("max_clipboard_bytes", settings.maxClipboardBytes)
            .putLong("max_clipboard_cache_bytes", settings.maxClipboardCacheBytes)
            .putString("device_name", settings.deviceName)
            .putBoolean("auto_reconnect", settings.autoReconnect)
            .putString("last_target", target.orEmpty())
            .apply()
        ClipboardCache.prune(context)
    }

    private fun parseMode(raw: String?): FfiClipboardMode {
        return runCatching { FfiClipboardMode.valueOf(raw ?: return FfiClipboardMode.BOTH) }
            .getOrDefault(FfiClipboardMode.BOTH)
    }
}
