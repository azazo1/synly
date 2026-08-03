package com.azazo1.synly.core

import android.os.SystemClock

/**
 * 剪贴板读取界面触发门控.
 *
 * 剪贴板可能在短时间内连续变化, 通过最小触发间隔合并为一次读取界面启动.
 */
object ClipboardReadGate {
    private const val MIN_TRIGGER_INTERVAL_MS = 300L

    @Volatile
    private var lastTriggerMs = 0L

    @Synchronized
    fun tryAcquire(): Boolean {
        val now = SystemClock.elapsedRealtime()
        if (now - lastTriggerMs < MIN_TRIGGER_INTERVAL_MS) return false
        lastTriggerMs = now
        return true
    }
}
