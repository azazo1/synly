package com.azazo1.synly.core

import org.junit.Assert.assertEquals
import org.junit.Test

class SynlyLogBufferTest {
    private fun entry(index: Int) = LogEntry("INFO", "test", "message-$index", index.toLong())

    @Test
    fun appendKeepsInsertionOrder() {
        val buffer = SynlyLogBuffer(10)
        repeat(5) { buffer.append(entry(it)) }
        assertEquals((0..4).map(::entry), buffer.snapshot())
    }

    @Test
    fun capacityEvictsOldestEntries() {
        val buffer = SynlyLogBuffer(3)
        repeat(5) { buffer.append(entry(it)) }
        assertEquals((2..4).map(::entry), buffer.snapshot())
    }

    @Test
    fun clearEmptiesBuffer() {
        val buffer = SynlyLogBuffer(3)
        repeat(3) { buffer.append(entry(it)) }
        buffer.clear()
        assertEquals(emptyList<LogEntry>(), buffer.snapshot())
    }
}
