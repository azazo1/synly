package com.azazo1.synly.core

import android.content.Context
import org.json.JSONArray
import org.json.JSONObject
import uniffi.synly_core.FfiDeviceIdentity
import uniffi.synly_core.FfiTrustedDeviceConfig

object TrustedDeviceStore {
    private const val PREFS = "synly_trusted_devices"
    private const val KEY_DEVICES = "devices"

    fun list(context: Context): List<FfiTrustedDeviceConfig> {
        val raw = context.getSharedPreferences(PREFS, Context.MODE_PRIVATE)
            .getString(KEY_DEVICES, null)
            ?: return emptyList()
        return runCatching {
            val array = JSONArray(raw)
            (0 until array.length()).map { index ->
                val obj = array.getJSONObject(index)
                FfiTrustedDeviceConfig(
                    deviceId = obj.getString("device_id"),
                    deviceName = obj.getString("device_name"),
                    publicKey = obj.getString("public_key"),
                    tlsRootCertificate = obj.getString("tls_root_certificate"),
                    trustedAtMs = obj.getLong("trusted_at_ms").toULong(),
                    lastSeenMs = obj.getLong("last_seen_ms").toULong(),
                    successfulSessions = obj.getLong("successful_sessions").toULong(),
                )
            }
        }.getOrDefault(emptyList())
    }

    fun add(context: Context, identity: FfiDeviceIdentity) {
        val devices = list(context).toMutableList()
        if (devices.any { it.deviceId == identity.deviceId }) return
        val now = System.currentTimeMillis()
        devices.add(
            FfiTrustedDeviceConfig(
                deviceId = identity.deviceId,
                deviceName = identity.deviceName,
                publicKey = identity.identityPublicKey,
                tlsRootCertificate = identity.tlsRootCertificate,
                trustedAtMs = now.toULong(),
                lastSeenMs = now.toULong(),
                successfulSessions = 1uL,
            ),
        )
        save(context, devices)
    }

    fun remove(context: Context, deviceId: String) {
        save(context, list(context).filterNot { it.deviceId == deviceId })
    }

    private fun save(context: Context, devices: List<FfiTrustedDeviceConfig>) {
        val array = JSONArray()
        devices.forEach { device ->
            array.put(
                JSONObject()
                    .put("device_id", device.deviceId)
                    .put("device_name", device.deviceName)
                    .put("public_key", device.publicKey)
                    .put("tls_root_certificate", device.tlsRootCertificate)
                    .put("trusted_at_ms", device.trustedAtMs)
                    .put("last_seen_ms", device.lastSeenMs)
                    .put("successful_sessions", device.successfulSessions),
            )
        }
        context.getSharedPreferences(PREFS, Context.MODE_PRIVATE)
            .edit()
            .putString(KEY_DEVICES, array.toString())
            .apply()
    }
}
