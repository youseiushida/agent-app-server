package dev.aas.android

import android.app.Application
import androidx.lifecycle.DefaultLifecycleObserver
import androidx.lifecycle.LifecycleOwner
import androidx.lifecycle.ProcessLifecycleOwner
import dev.aas.android.notify.NotificationChannels

/** Creates the [AppContainer] and connects the process lifecycle to the connection. */
open class AasApplication : Application() {
    lateinit var container: AppContainer
        private set

    override fun onCreate() {
        super.onCreate()
        container = createContainer()
        NotificationChannels.createAll(this)
        container.start()
        // ON_START of the process: an activity became visible. That is the always-allowed moment
        // to (re)start the foreground service, and a reconnect trigger (design.md §15).
        ProcessLifecycleOwner.get().lifecycle.addObserver(object : DefaultLifecycleObserver {
            override fun onStart(owner: LifecycleOwner) = container.onAppForeground()

            override fun onStop(owner: LifecycleOwner) = container.onAppBackground()
        })
    }

    /** The object graph; tests override it (a software key instead of the Android Keystore). */
    protected open fun createContainer(): AppContainer = AppContainer(this)

    /** Only test runners call this (Robolectric, between tests); a real process just dies. */
    override fun onTerminate() {
        container.close()
        super.onTerminate()
    }
}
