package dev.aas.android.service

import android.net.ConnectivityManager
import android.net.Network
import dev.aas.android.diagnostics.ConnectionLog
import dev.aas.android.sync.SyncEngine

/** What the connection should do about a change of the default network. */
interface NetworkSink {
    /** A usable default network appeared (there was none). */
    fun available()

    /** The default network is now a different one: sockets on the old one are stale. */
    fun changed()

    /** There is no usable default network. */
    fun lost()
}

/**
 * Turns the callbacks of `registerDefaultNetworkCallback` into [NetworkSink] calls. Pure logic
 * (networks are compared by identity), tested without Android.
 *
 * * `onAvailable(n)`: the first network → [NetworkSink.available]; a different one than the
 *   current default → [NetworkSink.changed] (the system switched, e.g. Wi-Fi → mobile, without
 *   an `onLost` for the old one); the same one again → nothing.
 * * `onLost(n)` of the current default → [NetworkSink.lost]; of an older one → nothing.
 * * `onBlockedStatusChanged(n, true)` (Doze or data saver blocks this app's traffic) is treated
 *   like a loss, so the engine waits for the unblock instead of burning reconnect attempts;
 *   unblocking counts as the network becoming available again.
 */
class DefaultNetworkTracker(private val sink: NetworkSink) {
    private var current: Any? = null
    private var blocked = false

    /** Called once after registering when there is no default network at all. */
    fun onNoNetworkAtStart() {
        if (current == null) sink.lost()
    }

    fun onAvailable(network: Any) {
        val previous = current
        current = network
        blocked = false
        when {
            previous == null -> sink.available()
            previous != network -> sink.changed()
        }
    }

    fun onLost(network: Any) {
        if (network != current) return
        current = null
        blocked = false
        sink.lost()
    }

    fun onBlockedStatusChanged(network: Any, isBlocked: Boolean) {
        if (network != current || isBlocked == blocked) return
        blocked = isBlocked
        if (isBlocked) sink.lost() else sink.available()
    }
}

/** Feeds the default network's changes to the [SyncEngine] while the connection service runs. */
class NetworkMonitor(
    private val connectivity: ConnectivityManager,
    private val engine: SyncEngine,
    private val log: ConnectionLog,
) {
    private val tracker = DefaultNetworkTracker(object : NetworkSink {
        override fun available() {
            log.info(ConnectionLog.SOURCE_NETWORK, "network available")
            engine.onNetworkAvailable()
        }

        override fun changed() {
            log.info(ConnectionLog.SOURCE_NETWORK, "default network changed: reconnecting")
            engine.onNetworkChanged()
        }

        override fun lost() {
            log.info(ConnectionLog.SOURCE_NETWORK, "no usable network")
            engine.onNetworkLost()
        }
    })

    private val callback = object : ConnectivityManager.NetworkCallback() {
        override fun onAvailable(network: Network) = synchronized(tracker) { tracker.onAvailable(network) }

        override fun onLost(network: Network) = synchronized(tracker) { tracker.onLost(network) }

        override fun onBlockedStatusChanged(network: Network, blocked: Boolean) =
            synchronized(tracker) { tracker.onBlockedStatusChanged(network, blocked) }
    }

    private var registered = false

    fun start() {
        if (registered) return
        connectivity.registerDefaultNetworkCallback(callback)
        registered = true
        // The callback reports an existing default network right away; with none it stays silent.
        if (connectivity.activeNetwork == null) synchronized(tracker) { tracker.onNoNetworkAtStart() }
    }

    fun stop() {
        if (!registered) return
        connectivity.unregisterNetworkCallback(callback)
        registered = false
    }
}
