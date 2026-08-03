package com.azazo1.synly.core

import org.junit.Assert.assertEquals
import org.junit.Assert.assertNotEquals
import org.junit.Assert.assertTrue
import org.junit.Test

class ClipboardPayloadTest {
    @Test
    fun signature_is_stable_for_same_content() {
        val first = ClipboardPayload(text = "hello").signature()
        val second = ClipboardPayload(text = "hello").signature()
        assertEquals(first, second)
    }

    @Test
    fun signature_changes_with_content() {
        val first = ClipboardPayload(text = "hello").signature()
        val second = ClipboardPayload(text = "world").signature()
        assertNotEquals(first, second)
    }

    @Test
    fun signature_covers_image_bytes() {
        val first = ClipboardPayload(imagePng = byteArrayOf(1, 2, 3)).signature()
        val second = ClipboardPayload(imagePng = byteArrayOf(1, 2, 4)).signature()
        assertNotEquals(first, second)
    }

    @Test
    fun signature_covers_files() {
        val first = ClipboardPayload(
            files = listOf(ClipboardFile("a.txt", byteArrayOf(1, 2))),
        ).signature()
        val second = ClipboardPayload(
            files = listOf(ClipboardFile("a.txt", byteArrayOf(1, 3))),
        ).signature()
        val third = ClipboardPayload(
            files = listOf(ClipboardFile("b.txt", byteArrayOf(1, 2))),
        ).signature()
        assertNotEquals(first, second)
        assertNotEquals(first, third)
    }

    @Test
    fun empty_payload_detection() {
        assertTrue(ClipboardPayload().isEmpty())
        assertTrue(!ClipboardPayload(text = "x").isEmpty())
        assertTrue(
            !ClipboardPayload(
                files = listOf(ClipboardFile("a.txt", byteArrayOf(1))),
            ).isEmpty(),
        )
    }
}
