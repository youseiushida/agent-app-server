package dev.aas.android.sync

import dev.aas.android.protocol.ErrorKind
import dev.aas.android.protocol.Event
import dev.aas.android.protocol.WORKSPACE_STREAM
import dev.aas.android.protocol.WorkspaceSnapshotResult
import kotlinx.coroutines.CoroutineStart
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.delay
import kotlinx.coroutines.launch
import kotlinx.serialization.json.JsonObject
import kotlinx.serialization.json.JsonPrimitive
import okhttp3.OkHttpClient
import java.net.InetAddress
import java.net.Socket
import java.util.concurrent.CopyOnWriteArrayList
import java.util.concurrent.atomic.AtomicBoolean
import java.util.concurrent.atomic.AtomicInteger
import javax.net.SocketFactory
import kotlin.random.Random
import kotlin.test.Test
import kotlin.test.assertEquals
import kotlin.test.assertIs
import kotlin.test.assertTrue

/** Liveness, reconnection and the close codes of protocol.md §2.3. */
class SyncEngineConnectionTest {
    private val emptySnapshot = WorkspaceSnapshotResult(emptyList(), emptyList(), emptyList(), emptyList(), emptyList(), 0)

    /** Always the largest delay of the jitter window, so waits are predictable. */
    private object MaxJitter : Random() {
        override fun nextBits(bitCount: Int): Int = 0

        override fun nextLong(from: Long, until: Long): Long = until - 1
    }

    private fun fixture(config: SyncConfig = TEST_CONFIG, random: Random = Random(1), http: OkHttpClient = OkHttpClient()) =
        EngineFixture(config = config, random = random, http = http).also { it.server.snapshot = emptySnapshot }

    /** Counts the TCP sockets the engine's OkHttp client creates and how many are open at once. */
    private class CountingSocketFactory : SocketFactory() {
        val open = AtomicInteger()
        val peak = AtomicInteger()
        val created = AtomicInteger()

        override fun createSocket(): Socket {
            created.incrementAndGet()
            peak.accumulateAndGet(open.incrementAndGet(), ::maxOf)
            return object : Socket() {
                private val closed = AtomicBoolean(false)

                override fun close() {
                    if (closed.compareAndSet(false, true)) open.decrementAndGet()
                    super.close()
                }
            }
        }

        override fun createSocket(host: String, port: Int): Socket = unsupported()

        override fun createSocket(host: String, port: Int, localHost: InetAddress, localPort: Int): Socket = unsupported()

        override fun createSocket(host: InetAddress, port: Int): Socket = unsupported()

        override fun createSocket(address: InetAddress, port: Int, localAddress: InetAddress, localPort: Int): Socket = unsupported()

        private fun unsupported(): Nothing = throw UnsupportedOperationException("OkHttp creates unconnected sockets")
    }

    /**
     * Every connection state from now on, in order. The collector is subscribed before this
     * returns (undispatched) and runs in the thread that changes the status (unconfined), so no
     * state is missed: neither those before a dispatched collector would have started, nor
     * short-lived ones a busy collector would see replaced (a state flow keeps the latest only).
     */
    private fun EngineFixture.recordStates(): CopyOnWriteArrayList<ConnectionState> {
        val states = CopyOnWriteArrayList<ConnectionState>()
        scope.launch(Dispatchers.Unconfined, start = CoroutineStart.UNDISPATCHED) { engine.status.collect { states += it.connection } }
        return states
    }

    @Test
    fun theWatchdogClosesASilentConnectionAfterTheServersClientTimeout() = withFixture(fixture()) { f ->
        f.server.clientTimeoutMs = 400 // the fake server never sends heartbeats
        val states = f.recordStates()
        f.connect()
        f.awaitOnline()
        val start = System.currentTimeMillis()
        eventually(what = "watchdog reconnect") { f.server.connections.size.takeIf { it >= 2 } }
        assertTrue(System.currentTimeMillis() - start >= 300, "not before the timeout")
        f.awaitOnline()
        assertTrue(states.any { it is ConnectionState.Reconnecting && it.cause == DisconnectCause.Watchdog(400) }, "$states")
        assertEquals(1, f.server.requestsFor("workspace/snapshot").size, "the reconnect resumes, it does not resync")
    }

    @Test
    fun heartbeatsKeepTheConnectionAlive() = withFixture(fixture()) { f ->
        f.server.clientTimeoutMs = 500
        f.connect()
        f.awaitOnline()
        repeat(10) {
            f.server.lastConnection.heartbeat(emptyMap())
            delay(150)
        }
        assertEquals(1, f.server.connections.size, "no reconnect while heartbeats flow")
        assertTrue(f.engine.status.value.lastHeartbeatAtMs != null)
    }

    /**
     * A phone that slept past the client timeout: the watchdog's timer does not count deep sleep
     * (it would still wait), so the app's return to the foreground measures the silence with
     * the engine's clock, which does, and replaces the dead connection at once.
     */
    @Test
    fun afterADeepSleepTheForegroundReplacesTheSilentConnectionAtOnce() {
        val clock = SleepingClock()
        withFixture(EngineFixture(clock = clock).also { it.server.snapshot = emptySnapshot }) { f ->
            f.server.clientTimeoutMs = 60_000
            f.connect()
            f.awaitOnline()
            clock.sleep(61_000)
            delay(QUIET_MS)
            assertEquals(1, f.server.connections.size, "the watchdog's timer did not notice the sleep")
            assertTrue(f.engine.status.value.isOnline, "shown as connected until something checks")

            f.engine.onAppForeground()
            eventually(what = "a new connection") { f.server.connections.size.takeIf { it >= 2 } }
            f.awaitOnline()
            assertTrue(f.logs.any { "the app came to the foreground: no frame for" in it }, "${f.logs}")
            assertEquals(1, f.server.requestsFor("workspace/snapshot").size, "the new connection resumes, it does not resync")
        }
    }

    @Test
    fun aNetworkThatBecomesAvailableAfterADeepSleepAlsoReplacesTheSilentConnection() {
        val clock = SleepingClock()
        withFixture(EngineFixture(clock = clock).also { it.server.snapshot = emptySnapshot }) { f ->
            f.server.clientTimeoutMs = 60_000
            f.connect()
            f.awaitOnline()
            clock.sleep(120_000)
            f.engine.onNetworkAvailable()
            eventually(what = "a new connection") { f.server.connections.size.takeIf { it >= 2 } }
            f.awaitOnline()
        }
    }

    @Test
    fun aForegroundWithinTheClientTimeoutKeepsTheConnection() {
        val clock = SleepingClock()
        withFixture(EngineFixture(clock = clock).also { it.server.snapshot = emptySnapshot }) { f ->
            f.server.clientTimeoutMs = 60_000
            f.connect()
            f.awaitOnline()
            clock.sleep(30_000)
            f.engine.onAppForeground()
            f.engine.onNetworkAvailable()
            delay(QUIET_MS)
            assertEquals(1, f.server.connections.size, "a connection silent for less than the client timeout stays")
            assertTrue(f.engine.status.value.isOnline)
        }
    }

    /**
     * After a sleep shorter than the client timeout, the watchdog is re-armed for the time
     * actually left (measured with the engine's clock), not for what its stopped timer believed.
     */
    @Test
    fun theForegroundReArmsTheWatchdogForTheTimeActuallyLeft() {
        val clock = SleepingClock()
        withFixture(EngineFixture(clock = clock).also { it.server.snapshot = emptySnapshot }) { f ->
            f.server.clientTimeoutMs = 60_000
            f.connect()
            f.awaitOnline()
            clock.sleep(59_000)
            f.engine.onAppForeground()
            assertEquals(1, f.server.connections.size, "not silent for the client timeout yet")
            // Within seconds (the second left), not the minute the stopped timer still waits.
            eventually(timeoutMs = REARMED_WAIT_MS, what = "the re-armed watchdog's reconnect") { f.server.connections.size.takeIf { it >= 2 } }
            assertTrue(f.engine.status.value.connection !is ConnectionState.Suspended)
        }
    }

    @Test
    fun aStreamAheadOfItsCursorIsResubscribedAfterTheClientTimeoutNotBefore() = withFixture(fixture()) { f ->
        f.server.clientTimeoutMs = 1_500
        f.connect()
        f.awaitOnline()
        val conn = f.server.lastConnection
        // An event exists on the server but its batch was lost.
        f.server.append(WORKSPACE_STREAM, Event.ProjectUpserted(Samples.project("prj_lost")))
        conn.subscribed[WORKSPACE_STREAM] = 1 // the fake believes it sent it
        val start = System.currentTimeMillis()
        var resubscribedAt = 0L
        while (resubscribedAt == 0L) {
            conn.heartbeat(mapOf(WORKSPACE_STREAM to 1))
            delay(200)
            if (f.server.requestsFor("subscribe").size == 2) resubscribedAt = System.currentTimeMillis()
            assertTrue(System.currentTimeMillis() - start < 10_000, "never resubscribed")
        }
        assertTrue(resubscribedAt - start >= 1_500, "several heartbeats are not enough; the client timeout is (${resubscribedAt - start} ms)")
        assertEquals(mapOf(WORKSPACE_STREAM to 0L), subscriptionsOf(f.server.requestsFor("subscribe").last()))
        eventually(what = "recovered event") { f.store.state.value.projects["prj_lost"] }
        assertEquals(1, f.engine.status.value.stallResubscribes)
        assertEquals(1, f.server.connections.size, "resubscribed on the same connection")
    }

    @Test
    fun headsThatProgressNeverTriggerAResubscription() = withFixture(fixture()) { f ->
        f.server.clientTimeoutMs = 600
        f.connect()
        f.awaitOnline()
        val conn = f.server.lastConnection
        repeat(8) { i ->
            f.server.append(WORKSPACE_STREAM, Event.ProjectUpserted(Samples.project("prj_$i")))
            conn.heartbeat(mapOf(WORKSPACE_STREAM to i + 1L))
            delay(150) // the batch lags one heartbeat behind, but it keeps coming
            conn.pushNew(WORKSPACE_STREAM)
            val applied = eventually(what = "prj_$i") { f.store.state.value.projects["prj_$i"] }
            assertEquals("prj_$i", applied.id)
        }
        assertEquals(1, f.server.requestsFor("subscribe").size)
        assertEquals(0, f.engine.status.value.stallResubscribes)
    }

    @Test
    fun theBackoffResetsOnlyAfterInitializeAndResubscriptionSucceeded() =
        withFixture(fixture(TEST_CONFIG.copy(backoffBaseMs = 60, backoffCapMs = 2_000), MaxJitter)) { f ->
            // The socket opens every time, but the setup fails three times.
            f.server.initializeError = { n -> if (n < 3) ErrorKind.Internal else null }
            val states = f.recordStates()
            f.connect()
            f.awaitOnline()
            val failing = states.filterIsInstance<ConnectionState.Reconnecting>()
            assertEquals(listOf(1, 2, 3), failing.map { it.attempt }.distinct(), "the attempt count grows while the setup fails: $states")
            assertTrue(failing.all { it.cause is DisconnectCause.SetupFailed }, "$failing")
            assertEquals(4, f.server.requestsFor("initialize").size)
            // After a session was established, the next drop starts from the first delay again.
            states.clear()
            f.server.lastConnection.kill()
            eventually(what = "reconnected") { f.server.connections.size.takeIf { it == 5 && f.engine.status.value.isOnline } }
            assertEquals(listOf(1), states.filterIsInstance<ConnectionState.Reconnecting>().map { it.attempt }.distinct(), "$states")
        }

    @Test
    fun reconnectTriggersSkipTheBackoffWait() =
        withFixture(fixture(TEST_CONFIG.copy(backoffBaseMs = 30_000, backoffCapMs = 30_000), MaxJitter)) { f ->
            f.connect()
            f.awaitOnline()
            for ((i, trigger) in listOf<(SyncEngine) -> Unit>({ it.reconnectNow() }, { it.onAppForeground() }, { it.onNetworkAvailable() }).withIndex()) {
                f.server.lastConnection.kill()
                val waiting = eventually(what = "backoff") { f.engine.status.value.connection as? ConnectionState.Reconnecting }
                assertTrue(waiting.retryAtMs - System.currentTimeMillis() > 20_000, "the backoff would wait ~30 s")
                trigger(f.engine)
                eventually(timeoutMs = 3_000, what = "immediate reconnect #$i") { f.server.connections.size.takeIf { it == i + 2 } }
                f.awaitOnline()
            }
        }

    @Test
    fun aRevokedDeviceStopsUntilItHasNewCredentials() = withFixture(fixture()) { f ->
        f.connect()
        f.awaitOnline()
        f.server.lastConnection.closeWith(SyncEngine.CLOSE_REVOKED, "device revoked")
        eventually(what = "suspended") { (f.engine.status.value.connection as? ConnectionState.Suspended)?.takeIf { it.reason == SuspendReason.Revoked } }
        f.engine.reconnectNow()
        f.engine.onAppForeground()
        f.engine.onNetworkAvailable()
        delay(300)
        assertEquals(1, f.server.connections.size, "no reconnect after a revocation, whatever the trigger")
        assertIs<ConnectionState.Suspended>(f.engine.status.value.connection)
        // Pairing again gives new credentials.
        f.engine.setCredentials(null)
        eventually(what = "not paired") { f.engine.status.value.connection.takeIf { it == ConnectionState.NotPaired } }
        f.engine.setCredentials(Credentials(f.server.wsUrl, "tok"))
        f.awaitOnline()
        assertEquals(2, f.server.connections.size)
    }

    @Test
    fun anInvalidTokenSuspendsInsteadOfRetrying() = withFixture(fixture()) { f ->
        f.connect(token = "wrong")
        val suspended = eventually(what = "suspended") { f.engine.status.value.connection as? ConnectionState.Suspended }
        assertEquals(SuspendReason.Unauthorized(401), suspended.reason)
        delay(300)
        assertEquals(0, f.server.connections.size)
    }

    @Test
    fun aReplacedConnectionWaitsForTheForegroundOrAUserAction() = withFixture(fixture()) { f ->
        f.connect()
        f.awaitOnline()
        fun replace() {
            val conn = f.server.lastConnection
            conn.notify("connection/replaced", JsonObject(emptyMap()))
            conn.closeWith(SyncEngine.CLOSE_REPLACED, "replaced")
        }
        replace()
        eventually(what = "suspended") { (f.engine.status.value.connection as? ConnectionState.Suspended)?.takeIf { it.reason == SuspendReason.Replaced } }
        f.engine.onNetworkAvailable()
        delay(300)
        assertEquals(1, f.server.connections.size, "a network change does not fight the other connection")
        f.engine.onAppForeground()
        f.awaitOnline()
        assertEquals(2, f.server.connections.size)
        replace()
        eventually(what = "suspended again") { f.engine.status.value.connection as? ConnectionState.Suspended }
        f.engine.reconnectNow()
        f.awaitOnline()
        assertEquals(3, f.server.connections.size)
    }

    @Test
    fun aProtocolViolationWaitsTheMaximumBackoff() =
        withFixture(fixture(TEST_CONFIG.copy(backoffBaseMs = 20, backoffCapMs = 1_500))) { f ->
            f.connect()
            f.awaitOnline()
            f.server.lastConnection.closeWith(SyncEngine.CLOSE_PROTOCOL_VIOLATION, "binary frame")
            val waiting = eventually(what = "backoff") { f.engine.status.value.connection as? ConnectionState.Reconnecting }
            assertEquals(DisconnectCause.ProtocolViolation("binary frame"), waiting.cause)
            assertTrue(waiting.retryAtMs - System.currentTimeMillis() > 1_200, "the maximum backoff, not a jittered one")
            assertTrue(f.engine.status.value.lastError!!.message.contains("4003"))
            f.engine.onAppForeground()
            f.engine.onNetworkAvailable()
            delay(400)
            assertEquals(1, f.server.connections.size, "only a user action skips this wait")
            f.engine.reconnectNow()
            eventually(timeoutMs = 1_000, what = "reconnect") { f.server.connections.size.takeIf { it == 2 } }
            f.awaitOnline()
        }

    @Test
    fun aServerShutdownIsShownAndFollowedByAReconnect() = withFixture(fixture()) { f ->
        f.connect()
        f.awaitOnline()
        val conn = f.server.lastConnection
        // The daemon's storage fail-stop: it announces why, closes with 1001 and is restarted.
        conn.notify("server/shuttingDown", kotlinx.serialization.json.buildJsonObject {
            put("reason", JsonPrimitive("storageFailure"))
            put("restartExpected", JsonPrimitive(true))
        })
        eventually(what = "shutting down") { f.engine.status.value.takeIf { it.serverShuttingDown } }
        assertEquals(dev.aas.android.protocol.ShutdownReason.StorageFailure, f.engine.status.value.serverShutdownReason)
        assertEquals(true, f.engine.status.value.serverRestartExpected)
        conn.closeWith(SyncEngine.CLOSE_GOING_AWAY, "shutdown")
        eventually(what = "reconnected") { f.server.connections.size.takeIf { it == 2 } }
        f.awaitOnline()
        assertEquals(false, f.engine.status.value.serverShuttingDown)
        assertEquals(null, f.engine.status.value.serverShutdownReason)
        assertEquals(null, f.engine.status.value.serverRestartExpected)
    }

    @Test
    fun aStopWithoutARestartIsShownAndStillRetried() = withFixture(fixture()) { f ->
        f.connect()
        f.awaitOnline()
        val conn = f.server.lastConnection
        // `agent-app-server stop`: nothing will start the server again by itself.
        conn.notify("server/shuttingDown", kotlinx.serialization.json.buildJsonObject {
            put("reason", JsonPrimitive("shutdown"))
            put("restartExpected", JsonPrimitive(false))
        })
        eventually(what = "shutting down") { f.engine.status.value.takeIf { it.serverShuttingDown } }
        assertEquals(false, f.engine.status.value.serverRestartExpected)
        assertEquals(dev.aas.android.protocol.ShutdownReason.Shutdown, f.engine.status.value.serverShutdownReason)
        conn.closeWith(SyncEngine.CLOSE_GOING_AWAY, "shutdown")
        // The client cannot know when someone starts it again: it keeps trying with backoff.
        eventually(what = "reconnected") { f.server.connections.size.takeIf { it == 2 } }
        f.awaitOnline()
    }

    @Test
    fun theEngineNeverHoldsTwoSocketsAtOnce() {
        val sockets = CountingSocketFactory()
        val fixture = fixture(http = OkHttpClient.Builder().socketFactory(sockets).build())
        withFixture(fixture) { f ->
            f.connect()
            f.awaitOnline()
            // Every way a socket is replaced: network changes (one by one and in a burst), the
            // server dropping it, new credentials.
            repeat(3) { i ->
                f.engine.onNetworkChanged()
                eventually(what = "connection ${i + 2}") { f.server.connections.size.takeIf { it >= i + 2 } }
                f.awaitOnline()
            }
            repeat(5) { f.engine.onNetworkChanged() }
            delay(200)
            f.awaitOnline()
            val before = f.server.connections.size
            f.server.lastConnection.kill()
            eventually(what = "reconnect after a kill") { f.server.connections.size.takeIf { it > before } }
            f.awaitOnline()
            f.engine.setCredentials(Credentials(f.server.wsUrl + "?again", "tok"))
            eventually(what = "reconnect with new credentials") { f.server.connections.size.takeIf { it > before + 1 } }
            f.awaitOnline()
            assertEquals(1, sockets.peak.get(), "at most one socket at any time (${sockets.created.get()} created)")
            assertEquals(1, sockets.open.get(), "the current one")
            assertTrue(sockets.created.get() >= 6, "sockets created: ${sockets.created.get()}")
        }
    }

    @Test
    fun aCloseCodeSentToASocketTheEngineAlreadyLeftIsIgnored() = withFixture(fixture()) { f ->
        // The server replaces the older connection when the newer one arrives, like the real
        // one, even when the client gave the older one up already (a network change).
        f.server.replaceOlderConnections = true
        val states = f.recordStates()
        f.connect()
        f.awaitOnline()
        repeat(3) { i ->
            f.engine.onNetworkChanged()
            eventually(what = "connection ${i + 2}") { f.server.connections.size.takeIf { it == i + 2 } }
            f.awaitOnline()
        }
        delay(300)
        assertTrue(f.engine.status.value.isOnline, "${f.engine.status.value}")
        assertTrue(states.none { it is ConnectionState.Suspended }, "never replaced by its own newer socket: $states")
        assertEquals(4, f.server.connections.size, "no extra reconnects")
    }

    @Test
    fun twoHoldersOfOneTokenDoNotFightForTheConnection() = withFixture(fixture()) { f ->
        f.server.replaceOlderConnections = true
        f.connect()
        f.awaitOnline()
        // A second app instance with the same device token (a restored backup, a second install).
        val other = SyncEngine(InMemorySyncStore(), f.http, f.scope, dev.aas.android.protocol.ClientInfo("other", "0", "jvm"), TEST_CONFIG)
        other.setCredentials(Credentials(f.server.wsUrl, "tok"))
        other.start()
        eventually(what = "the other online") { other.status.value.isOnline.takeIf { it } }
        eventually(what = "this one replaced") {
            (f.engine.status.value.connection as? ConnectionState.Suspended)?.takeIf { it.reason == SuspendReason.Replaced }
        }
        // Neither reconnects on its own: no ping-pong between the two.
        f.engine.onNetworkAvailable()
        delay(500)
        assertEquals(2, f.server.connections.size)
        assertTrue(other.status.value.isOnline)
        other.stop().join()
    }

    @Test
    fun anIncompatibleServerSuspends() = withFixture(fixture()) { f ->
        f.server.initializeError = { ErrorKind.ProtocolVersionUnsupported }
        f.connect()
        val suspended = eventually(what = "suspended") { f.engine.status.value.connection as? ConnectionState.Suspended }
        assertIs<SuspendReason.Incompatible>(suspended.reason)
        delay(300)
        assertEquals(1, f.server.requestsFor("initialize").size, "no retry loop")
        // The server was updated: a user action retries.
        f.server.initializeError = { null }
        f.engine.reconnectNow()
        f.awaitOnline()
    }

    @Test
    fun aServerOfAnotherProtocolVersionSuspends() = withFixture(fixture()) { f ->
        f.server.protocolVersion = 2
        f.connect()
        val suspended = eventually(what = "suspended") { f.engine.status.value.connection as? ConnectionState.Suspended }
        assertIs<SuspendReason.Incompatible>(suspended.reason)
    }

    @Test
    fun noNetworkMeansOfflineAndANetworkReconnectsAtOnce() =
        withFixture(fixture(TEST_CONFIG.copy(backoffBaseMs = 30_000, backoffCapMs = 30_000), MaxJitter)) { f ->
            f.connect()
            f.awaitOnline()
            f.engine.onNetworkLost()
            eventually(what = "offline") { f.engine.status.value.connection.takeIf { it == ConnectionState.Offline } }
            eventually(what = "socket closed") { f.server.lastConnection.takeIf { it.closed } }
            delay(300)
            assertEquals(1, f.server.connections.size, "no attempts without a network")
            f.engine.onNetworkAvailable()
            f.awaitOnline()
            // A changed default network replaces a healthy-looking socket at once (no backoff).
            f.engine.onNetworkChanged()
            eventually(timeoutMs = 3_000, what = "new socket") { f.server.connections.size.takeIf { it == 3 } }
            f.awaitOnline()
        }

    @Test
    fun stopClosesTheConnectionAndStartResumes() = withFixture(fixture()) { f ->
        f.connect()
        f.awaitOnline()
        f.engine.stop().join()
        eventually(what = "stopped") { f.engine.status.value.connection.takeIf { it == ConnectionState.Stopped } }
        eventually(what = "socket closed") { f.server.lastConnection.takeIf { it.closed } }
        f.engine.start()
        f.awaitOnline()
        assertEquals(2, f.server.connections.size)
        assertEquals(1, f.server.requestsFor("workspace/snapshot").size)
    }

    /** Throws from the next transaction once [armed] is set (a disk error). */
    private class FlakyStore(private val inner: SyncStore) : SyncStore {
        @Volatile
        var armed = false

        override suspend fun <T> transaction(block: suspend (SyncTx) -> T): T {
            if (armed) {
                armed = false
                throw java.io.IOException("disk I/O error")
            }
            return inner.transaction(block)
        }
    }

    @Test
    fun aStoreFailureDropsTheSessionAndTheReplayRecovers() {
        val inner = InMemorySyncStore()
        val flaky = FlakyStore(inner)
        val f = EngineFixture(storeOverride = flaky, store = inner)
        f.server.snapshot = emptySnapshot
        withFixture(f) {
            val states = f.recordStates()
            f.connect()
            f.awaitOnline()
            flaky.armed = true
            f.server.append(WORKSPACE_STREAM, Event.ProjectUpserted(Samples.project("prj_1")))
            f.server.lastConnection.pushNew(WORKSPACE_STREAM)
            eventually(what = "replayed after the failure") { inner.state.value.projects["prj_1"] }
            assertTrue(states.any { it is ConnectionState.Reconnecting && it.cause is DisconnectCause.ClientError }, "$states")
            assertTrue(f.logs.any { "disk I/O error" in it }, "the failure is reported: ${f.logs}")
            // The replayed batch can be applied before the new session's setup completes; the
            // error goes when the session is established, not with the batch.
            eventually(what = "a successful session clears the error") { f.engine.status.value.takeIf { it.isOnline && it.lastError == null } }
            assertEquals(2, f.server.connections.size)
            assertEquals(1L, inner.state.value.cursors[WORKSPACE_STREAM])
        }
    }

    @Test
    fun anUnusableServerUrlSuspends() = withFixture(fixture()) { f ->
        f.engine.setCredentials(Credentials("not a url", "tok"))
        f.engine.start()
        val suspended = eventually(what = "suspended") { f.engine.status.value.connection as? ConnectionState.Suspended }
        assertIs<SuspendReason.InvalidServerUrl>(suspended.reason)
    }

    private companion object {
        /** Long enough for a reconnect to show if one were (wrongly) made. */
        const val QUIET_MS = 500L

        /** The re-armed watchdog fires after the second left; the stopped timer would wait a minute. */
        const val REARMED_WAIT_MS = 10_000L
    }
}
