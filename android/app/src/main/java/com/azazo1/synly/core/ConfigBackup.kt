package com.azazo1.synly.core

import android.content.Context
import org.json.JSONArray
import org.json.JSONObject
import uniffi.synly_core.FfiClipboardMode
import uniffi.synly_core.FfiDeviceConfig
import uniffi.synly_core.FfiTrustedDeviceConfig

object ConfigBackup {
    private const val FORMAT = "synly-config"
    private const val VERSION = 1
    private const val MAX_IMAGE_BYTES = 100L * 1024 * 1024

    data class Backup(
        val settings: SynlySettings,
        val identity: FfiDeviceConfig?,
        val trustedDevices: List<FfiTrustedDeviceConfig>,
    )

    fun create(context: Context): String {
        val settings = SettingsStore.load(context)
        val identity = IdentityStore.getOrCreate(context)
        val trustedDevices = TrustedDeviceStore.list(context)
        return JSONObject()
            .put("format", FORMAT)
            .put("version", VERSION)
            .put("exported_at_ms", System.currentTimeMillis())
            .put("settings", settingsJson(settings))
            .put("identity", identityJson(identity))
            .put("trusted_devices", trustedDevicesJson(trustedDevices))
            .toString(2)
    }

    fun parse(raw: String): Backup {
        val root = JSONObject(raw)
        if (root.optString("format") != FORMAT) {
            error("不是 Synly 配置文件")
        }
        if (root.optInt("version", 0) != VERSION) {
            error("不支持的配置版本")
        }
        val settings = root.optJSONObject("settings")?.let(::parseSettings) ?: SynlySettings()
        val identity = root.optJSONObject("identity")?.let(::parseIdentity)
        val trustedDevices = parseTrustedDevices(root.optJSONArray("trusted_devices"))
        return Backup(
            settings = settings,
            identity = identity,
            trustedDevices = trustedDevices,
        )
    }

    fun apply(context: Context, backup: Backup) {
        SettingsStore.save(context, backup.settings)
        backup.identity?.let { IdentityStore.replace(context, it) }
        TrustedDeviceStore.replace(context, backup.trustedDevices)
    }

    private fun settingsJson(settings: SynlySettings): JSONObject {
        return JSONObject()
            .put("clipboard_mode", settings.clipboardMode.name)
            .put("mdns_enabled", settings.mdnsEnabled)
            .put("lnd_enabled", settings.lndEnabled)
            .put("lnd_server_url", settings.lndServerUrl ?: JSONObject.NULL)
            .put("lnd_bearer_token", settings.lndBearerToken ?: JSONObject.NULL)
            .put("lnd_discovery_domain", settings.lndDiscoveryDomain ?: JSONObject.NULL)
            .put("max_image_bytes", settings.maxImageBytes)
            .put("device_name", settings.deviceName)
            .put("last_target", targetJson(settings.lastTarget))
    }

    private fun parseSettings(obj: JSONObject): SynlySettings {
        val mode = obj.optString("clipboard_mode")
        val clipboardMode = runCatching { FfiClipboardMode.valueOf(mode) }
            .getOrElse { error("clipboard_mode 无效") }
        val maxImageBytes = obj.optLong("max_image_bytes", 20L * 1024 * 1024)
            .coerceIn(1L, MAX_IMAGE_BYTES)
        return SynlySettings(
            clipboardMode = clipboardMode,
            mdnsEnabled = obj.optBoolean("mdns_enabled", true),
            lndEnabled = obj.optBoolean("lnd_enabled", false),
            lndServerUrl = obj.optString("lnd_server_url").takeIf { it.isNotBlank() },
            lndBearerToken = obj.optString("lnd_bearer_token").takeIf { it.isNotBlank() },
            lndDiscoveryDomain = obj.optString("lnd_discovery_domain").takeIf { it.isNotBlank() },
            maxImageBytes = maxImageBytes,
            deviceName = obj.optString("device_name", DEFAULT_DEVICE_NAME),
            lastTarget = parseTarget(obj.optJSONObject("last_target")),
        )
    }

    private fun targetJson(target: SynlyTarget?): Any {
        if (target == null) return JSONObject.NULL
        val addresses = JSONArray()
        target.addresses.forEach { addresses.put(it) }
        return JSONObject()
            .put("addresses", addresses)
            .put("port", target.port)
            .put("peer_device_id", target.peerDeviceId ?: JSONObject.NULL)
    }

    private fun parseTarget(obj: JSONObject?): SynlyTarget? {
        if (obj == null) return null
        val addresses = obj.optJSONArray("addresses")
        val port = obj.optInt("port", -1)
        if (addresses == null || addresses.length() == 0 || port !in 1..65535) {
            error("last_target 无效")
        }
        val parsedAddresses = (0 until addresses.length()).map { addresses.getString(it) }
        if (parsedAddresses.any { it.isBlank() }) {
            error("last_target 地址无效")
        }
        return SynlyTarget(
            addresses = parsedAddresses,
            port = port,
            peerDeviceId = obj.optString("peer_device_id").takeIf { it.isNotBlank() },
        )
    }

    private fun identityJson(identity: FfiDeviceConfig): JSONObject {
        return JSONObject()
            .put("device_id", identity.deviceId)
            .put("device_name", identity.deviceName)
            .put("identity_private_key", identity.identityPrivateKey)
            .put("identity_public_key", identity.identityPublicKey)
    }

    private fun parseIdentity(obj: JSONObject): FfiDeviceConfig {
        val identity = FfiDeviceConfig(
            deviceId = obj.optString("device_id"),
            deviceName = obj.optString("device_name"),
            identityPrivateKey = obj.optString("identity_private_key"),
            identityPublicKey = obj.optString("identity_public_key"),
        )
        if (identity.deviceId.isBlank() || identity.deviceName.isBlank() ||
            identity.identityPrivateKey.isBlank() || identity.identityPublicKey.isBlank()
        ) {
            error("identity 字段不完整")
        }
        return identity
    }

    private fun trustedDevicesJson(devices: List<FfiTrustedDeviceConfig>): JSONArray {
        val array = JSONArray()
        devices.forEach { device ->
            array.put(
                JSONObject()
                    .put("device_id", device.deviceId)
                    .put("device_name", device.deviceName)
                    .put("public_key", device.publicKey)
                    .put("tls_root_certificate", device.tlsRootCertificate)
                    .put("trusted_at_ms", device.trustedAtMs.toLong())
                    .put("last_seen_ms", device.lastSeenMs.toLong())
                    .put("successful_sessions", device.successfulSessions.toLong()),
            )
        }
        return array
    }

    private fun parseTrustedDevices(array: JSONArray?): List<FfiTrustedDeviceConfig> {
        if (array == null) return emptyList()
        val ids = mutableSetOf<String>()
        return (0 until array.length()).map { index ->
            val obj = array.getJSONObject(index)
            val trustedAtMs = obj.optLong("trusted_at_ms", 0L)
            val lastSeenMs = obj.optLong("last_seen_ms", 0L)
            val successfulSessions = obj.optLong("successful_sessions", 0L)
            if (trustedAtMs < 0 || lastSeenMs < 0 || successfulSessions < 0) {
                error("trusted_devices 时间无效")
            }
            val device = FfiTrustedDeviceConfig(
                deviceId = obj.optString("device_id"),
                deviceName = obj.optString("device_name"),
                publicKey = obj.optString("public_key"),
                tlsRootCertificate = obj.optString("tls_root_certificate"),
                trustedAtMs = trustedAtMs.toULong(),
                lastSeenMs = lastSeenMs.toULong(),
                successfulSessions = successfulSessions.toULong(),
            )
            if (device.deviceId.isBlank() || device.deviceName.isBlank() ||
                device.publicKey.isBlank() || device.tlsRootCertificate.isBlank()
            ) {
                error("trusted_devices 字段不完整")
            }
            if (!ids.add(device.deviceId)) {
                error("trusted_devices 存在重复设备")
            }
            device
        }
    }
}
