package com.azazo1.synly.core

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
}
