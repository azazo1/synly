package com.azazo1.synly.core

import android.content.Context
import android.security.keystore.KeyGenParameterSpec
import android.security.keystore.KeyProperties
import android.util.Base64
import uniffi.synly_core.FfiDeviceConfig
import uniffi.synly_core.generateDeviceConfig
import java.security.KeyStore
import javax.crypto.Cipher
import javax.crypto.KeyGenerator
import javax.crypto.SecretKey
import javax.crypto.spec.GCMParameterSpec

object IdentityStore {
    private const val PREFS = "synly_identity"
    private const val KEY_ALIAS = "synly_identity_key"
    private const val PREF_DEVICE_ID = "device_id"
    private const val PREF_DEVICE_NAME = "device_name"
    private const val PREF_PUBLIC_KEY = "public_key"
    private const val PREF_PRIVATE_KEY = "private_key_enc"

    fun getOrCreate(context: Context): FfiDeviceConfig {
        val prefs = context.getSharedPreferences(PREFS, Context.MODE_PRIVATE)
        val deviceId = prefs.getString(PREF_DEVICE_ID, null)
        val deviceName = prefs.getString(PREF_DEVICE_NAME, null)
        val publicKey = prefs.getString(PREF_PUBLIC_KEY, null)
        val privateKeyEnc = prefs.getString(PREF_PRIVATE_KEY, null)
        if (deviceId != null && deviceName != null && publicKey != null && privateKeyEnc != null) {
            val privateKey = decrypt(privateKeyEnc)
            return FfiDeviceConfig(
                deviceId = deviceId,
                deviceName = deviceName,
                identityPrivateKey = privateKey,
                identityPublicKey = publicKey,
            )
        }
        val fallbackName = SettingsStore.load(context).deviceName
        val generated = generateDeviceConfig(fallbackName)
        val encrypted = encrypt(generated.identityPrivateKey)
        prefs.edit()
            .putString(PREF_DEVICE_ID, generated.deviceId)
            .putString(PREF_DEVICE_NAME, generated.deviceName)
            .putString(PREF_PUBLIC_KEY, generated.identityPublicKey)
            .putString(PREF_PRIVATE_KEY, encrypted)
            .apply()
        return generated
    }

    private fun key(): SecretKey {
        val keyStore = KeyStore.getInstance("AndroidKeyStore").apply { load(null) }
        (keyStore.getKey(KEY_ALIAS, null) as? SecretKey)?.let { return it }
        val generator = KeyGenerator.getInstance(KeyProperties.KEY_ALGORITHM_AES, "AndroidKeyStore")
        generator.init(
            KeyGenParameterSpec.Builder(
                KEY_ALIAS,
                KeyProperties.PURPOSE_ENCRYPT or KeyProperties.PURPOSE_DECRYPT,
            )
                .setBlockModes(KeyProperties.BLOCK_MODE_GCM)
                .setEncryptionPaddings(KeyProperties.ENCRYPTION_PADDING_NONE)
                .build(),
        )
        return generator.generateKey()
    }

    private fun encrypt(plain: String): String {
        val cipher = Cipher.getInstance("AES/GCM/NoPadding")
        cipher.init(Cipher.ENCRYPT_MODE, key())
        val iv = cipher.iv
        val encrypted = cipher.doFinal(plain.toByteArray(Charsets.UTF_8))
        return Base64.encodeToString(iv + encrypted, Base64.NO_WRAP)
    }

    private fun decrypt(encoded: String): String {
        val data = Base64.decode(encoded, Base64.NO_WRAP)
        val iv = data.copyOfRange(0, 12)
        val encrypted = data.copyOfRange(12, data.size)
        val cipher = Cipher.getInstance("AES/GCM/NoPadding")
        cipher.init(Cipher.DECRYPT_MODE, key(), GCMParameterSpec(128, iv))
        return String(cipher.doFinal(encrypted), Charsets.UTF_8)
    }
}

