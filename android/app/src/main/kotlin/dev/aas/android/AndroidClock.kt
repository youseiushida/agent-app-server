package dev.aas.android

import android.os.SystemClock
import dev.aas.android.sync.Clock

/**
 * The app's clocks: wall-clock time for what users see, and `SystemClock.elapsedRealtime` for
 * durations. elapsedRealtime keeps counting while the phone is in deep sleep, where
 * `System.nanoTime` (the JVM default, [Clock.System]) stops: the sync engine's watchdog measures
 * a connection's silence with it, so a socket that died while the phone slept is noticed when
 * the app comes back instead of being shown as 接続済み (docs/android.md 6.4).
 */
object AndroidClock : Clock {
    override fun nowMs(): Long = System.currentTimeMillis()

    override fun monotonicMs(): Long = SystemClock.elapsedRealtime()
}
