package com.azazo1.synly.core

import android.util.Log
import java.util.concurrent.locks.ReentrantLock
import kotlin.concurrent.withLock
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.StateFlow

data class LogEntry(
    val level: String,
    val target: String,
    val message: String,
    val timestampMs: Long,
)

class SynlyLogBuffer(private val capacity: Int) {
    private val lock = ReentrantLock()
    private val entries = ArrayDeque<LogEntry>()

    fun append(entry: LogEntry) {
        lock.withLock {
            entries.addLast(entry)
            while (entries.size > capacity) {
                entries.removeFirst()
            }
        }
    }

    fun snapshot(): List<LogEntry> = lock.withLock { entries.toList() }

    fun clear() {
        lock.withLock { entries.clear() }
    }
}

object SynlyLog {
    private const val CAPACITY = 500

    private val buffer = SynlyLogBuffer(CAPACITY)
    private val _entries = MutableStateFlow<List<LogEntry>>(emptyList())
    val entries: StateFlow<List<LogEntry>> = _entries

    fun append(level: String, target: String, message: String) {
        val canonical = when (level.lowercase()) {
            "error" -> "ERROR"
            "warn" -> "WARN"
            "debug" -> "DEBUG"
            "trace" -> "TRACE"
            else -> "INFO"
        }
        buffer.append(LogEntry(canonical, target, message, System.currentTimeMillis()))
        _entries.value = buffer.snapshot()
    }

    fun clear() {
        buffer.clear()
        _entries.value = emptyList()
    }

    fun i(tag: String, message: String) {
        log(Log.INFO, "INFO", tag, message, null)
    }

    fun w(tag: String, message: String, throwable: Throwable? = null) {
        log(Log.WARN, "WARN", tag, message, throwable)
    }

    fun e(tag: String, message: String, throwable: Throwable? = null) {
        log(Log.ERROR, "ERROR", tag, message, throwable)
    }

    private fun log(
        priority: Int,
        level: String,
        tag: String,
        message: String,
        throwable: Throwable?,
    ) {
        val full = if (throwable != null) {
            "$message\n${Log.getStackTraceString(throwable)}"
        } else {
            message
        }
        Log.println(priority, tag, full)
        append(level, tag, full)
    }
}
