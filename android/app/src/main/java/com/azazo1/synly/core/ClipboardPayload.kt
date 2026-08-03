package com.azazo1.synly.core

import java.security.MessageDigest

data class ClipboardFile(
    val name: String,
    val bytes: ByteArray,
)

data class ClipboardPayload(
    val text: String? = null,
    val html: String? = null,
    val imagePng: ByteArray? = null,
    val files: List<ClipboardFile> = emptyList(),
) {
    fun isEmpty(): Boolean =
        text == null && html == null && imagePng == null && files.isEmpty()

    fun signature(): String {
        val digest = MessageDigest.getInstance("SHA-256")
        text?.let { digest.update(it.toByteArray(Charsets.UTF_8)) }
        html?.let { digest.update(it.toByteArray(Charsets.UTF_8)) }
        imagePng?.let { digest.update(it) }
        files.forEach { file ->
            digest.update(file.name.toByteArray(Charsets.UTF_8))
            digest.update(file.bytes)
        }
        return digest.digest().joinToString("") { byte -> "%02x".format(byte) }
    }
}
