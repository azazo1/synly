package com.azazo1.synly

import android.app.Application
import com.azazo1.synly.core.SynlyEngine

class SynlyApplication : Application() {
    companion object {
        @Volatile
        var instance: SynlyApplication? = null
            private set
    }

    override fun onCreate() {
        super.onCreate()
        instance = this
        SynlyEngine.init(this)
    }
}

