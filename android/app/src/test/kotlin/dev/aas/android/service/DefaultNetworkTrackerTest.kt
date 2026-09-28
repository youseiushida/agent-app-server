package dev.aas.android.service

import org.junit.Test
import kotlin.test.assertEquals

class DefaultNetworkTrackerTest {
    private val calls = ArrayList<String>()
    private val tracker = DefaultNetworkTracker(object : NetworkSink {
        override fun available() {
            calls += "available"
        }

        override fun changed() {
            calls += "changed"
        }

        override fun lost() {
            calls += "lost"
        }
    })

    private val wifi = Any()
    private val cell = Any()

    @Test
    fun firstNetworkSwitchAndLoss() {
        tracker.onAvailable(wifi)
        tracker.onAvailable(wifi) // the same network again (capabilities refresh): nothing
        tracker.onAvailable(cell) // the system switched without onLost for Wi-Fi
        tracker.onLost(wifi) // late loss of the old network: nothing
        tracker.onLost(cell)
        tracker.onAvailable(wifi)
        assertEquals(listOf("available", "changed", "lost", "available"), calls)
    }

    @Test
    fun noNetworkAtStartGoesOffline() {
        tracker.onNoNetworkAtStart()
        tracker.onAvailable(wifi)
        assertEquals(listOf("lost", "available"), calls)
    }

    @Test
    fun noNetworkAtStartIsIgnoredWhenOneArrivedFirst() {
        tracker.onAvailable(wifi)
        tracker.onNoNetworkAtStart()
        assertEquals(listOf("available"), calls)
    }

    @Test
    fun blockingIsTreatedLikeALossOfTheCurrentNetwork() {
        tracker.onAvailable(wifi)
        tracker.onBlockedStatusChanged(wifi, true)
        tracker.onBlockedStatusChanged(wifi, true) // repeated: nothing
        tracker.onBlockedStatusChanged(cell, false) // not the default network: nothing
        tracker.onBlockedStatusChanged(wifi, false)
        assertEquals(listOf("available", "lost", "available"), calls)
    }
}
