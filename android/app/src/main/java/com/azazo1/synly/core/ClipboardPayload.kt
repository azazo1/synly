package com.azazo1.synly.core

import java.security.MessageDigest

data class ClipboardPayload(
    val text: String? = null,
    val html: String? = null,
    val imagePng: ByteArray? = null,
) {
    fun isEmpty(): Boolean = text == null && html == null && imagePng == null

    fun signature(): String {
        val digest = MessageDigest.getInstance("SHA-256")
        text?.let { digest.update(it.toByteArray(Charsets.UTF_8)) }
        html?.let { digest.update(it.toByteArray(Charsets.UTF_8)) }
        imagePng?.let { digest.update(it) }
        return digest.digest().joinToString("") { byte -> "%02x".format(byte) }
    }
}

