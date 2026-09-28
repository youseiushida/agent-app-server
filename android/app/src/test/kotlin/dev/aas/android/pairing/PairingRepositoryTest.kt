package dev.aas.android.pairing

import androidx.datastore.preferences.core.PreferenceDataStoreFactory
import dev.aas.android.AppPolicy
import dev.aas.android.diagnostics.ConnectionLog
import dev.aas.android.protocol.AasJson
import dev.aas.android.protocol.ClientInfo
import dev.aas.android.protocol.PairResponse
import dev.aas.android.protocol.PairServerInfo
import dev.aas.android.security.CredentialStore
import dev.aas.android.security.FakeKeyProvider
import dev.aas.android.security.PairingState
import dev.aas.android.security.TokenCipher
import dev.aas.android.service.ServiceStarter
import dev.aas.android.service.StartReason
import dev.aas.android.sync.AasHttp
import dev.aas.android.sync.InMemorySyncStore
import dev.aas.android.sync.OutboxEntry
import dev.aas.android.sync.SyncEngine
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.SupervisorJob
import kotlinx.coroutines.cancel
import kotlinx.coroutines.runBlocking
import kotlinx.serialization.json.JsonObject
import kotlinx.serialization.json.JsonPrimitive
import mockwebserver3.MockResponse
import mockwebserver3.MockWebServer
import okhttp3.OkHttpClient
import org.junit.After
import org.junit.Before
import org.junit.Rule
import org.junit.Test
import org.junit.runner.RunWith
import androidx.test.ext.junit.runners.AndroidJUnit4
import org.junit.rules.TemporaryFolder
import java.io.File
import java.net.ServerSocket
import kotlin.test.assertEquals
import kotlin.test.assertFailsWith
import kotlin.test.assertFalse
import kotlin.test.assertIs
import kotlin.test.assertTrue

class FakeStarter : ServiceStarter {
    val starts = ArrayList<StartReason>()
    var stops = 0

    override fun requestStart(reason: StartReason): Boolean {
        starts += reason
        return true
    }

    override fun stop() {
        stops++
    }
}

/**
 * Robolectric: DataStore replaces its file with `Files.move(REPLACE_EXISTING)` on API 26+; on
 * the plain JVM (SDK_INT 0) it falls back to `File.renameTo`, which cannot replace a file on
 * Windows.
 */
@RunWith(AndroidJUnit4::class)
class PairingRepositoryTest {
    @get:Rule
    val folder = TemporaryFolder()

    private val scope = CoroutineScope(SupervisorJob() + Dispatchers.Default)
    private val server = MockWebServer()
    private val keys = FakeKeyProvider()
    private val store = InMemorySyncStore()
    private val starter = FakeStarter()
    private lateinit var credentials: CredentialStore
    private lateinit var engine: SyncEngine
    private lateinit var repository: PairingRepository
    private var notificationsCleared = 0

    @Before
    fun setUp() {
        server.start()
        val dataStore = PreferenceDataStoreFactory.create(scope = scope, produceFile = { File(folder.root, "c.preferences_pb") })
        credentials = CredentialStore(dataStore, TokenCipher(keys), Dispatchers.IO, scope, retryInitialMs = 50, retryMaxMs = 200)
        val http = OkHttpClient()
        engine = SyncEngine(store, http, scope, ClientInfo("test", "0", "android"))
        repository = PairingRepository(AasHttp(http), credentials, keys, engine, store, starter, AppPolicy(), ConnectionLog(100, logcat = false), { notificationsCleared++ })
    }

    @After
    fun tearDown() {
        server.close()
        scope.cancel()
    }

    private val wsUrl get() = server.url("/v1/ws").toString().replaceFirst("http", "ws")

    private fun target(url: String = wsUrl) = PairingTarget(url, "ABCD-1234", "home pc")

    private fun pairResponse(epoch: String = "e1") = MockResponse.Builder()
        .body(AasJson.encodeToString(PairResponse.serializer(), PairResponse("dev_new", "token-new", PairServerInfo("home pc", epoch))))
        .build()

    private fun error(code: Int, kind: String) = MockResponse.Builder().code(code).body("""{"kind":"$kind","message":"m"}""").build()

    @Test
    fun pairsStoresAndStartsTheConnection() = runBlocking<Unit> {
        server.enqueue(pairResponse())
        val device = repository.pair(target(), "Pixel 9")
        val request = server.takeRequest()
        assertEquals("/v1/pair", request.url.encodedPath)
        val body = request.body!!.utf8()
        assertTrue(body.contains("\"code\":\"ABCD-1234\"") && body.contains("\"deviceName\":\"Pixel 9\"") && body.contains("\"platform\":\"android\""))
        // Nothing is stored before apply().
        assertEquals(PairingState.NotPaired, credentials.current())
        assertEquals(OutboxPlan.NothingPending, repository.plan(device))

        repository.apply(device, discardLocal = false)
        val paired = assertIs<PairingState.Paired>(credentials.current()).pairing
        assertEquals("dev_new", paired.info.deviceId)
        assertEquals("home pc", paired.info.serverName)
        assertEquals("Pixel 9", paired.info.deviceName)
        assertEquals(wsUrl, paired.info.wsUrl)
        assertEquals("token-new", paired.token)
        assertEquals(listOf(StartReason.Paired), starter.starts)
    }

    @Test
    fun serverAnswersBecomeTypedErrors() = runBlocking<Unit> {
        server.enqueue(error(400, "invalidCode"))
        assertEquals(PairingError.InvalidCode, assertFailsWith<PairingException> { repository.pair(target(), "p") }.error)
        server.enqueue(error(429, "rateLimited"))
        assertEquals(PairingError.RateLimited, assertFailsWith<PairingException> { repository.pair(target(), "p") }.error)
        server.enqueue(error(400, "invalidParams"))
        assertEquals(PairingError.Rejected(400, "invalidParams", "m"), assertFailsWith<PairingException> { repository.pair(target(), "p") }.error)
        server.enqueue(MockResponse.Builder().code(502).body("<html>bad gateway</html>").build())
        val proxy = assertIs<PairingError.Rejected>(assertFailsWith<PairingException> { repository.pair(target(), "p") }.error)
        assertEquals(502, proxy.status)
        server.enqueue(MockResponse.Builder().body("""{"unexpected":true}""").build())
        assertIs<PairingError.InvalidResponse>(assertFailsWith<PairingException> { repository.pair(target(), "p") }.error)
        assertEquals(PairingState.NotPaired, credentials.current())
        assertTrue(starter.starts.isEmpty())
    }

    @Test
    fun anUnreachableServerIsReportedAsSuch() = runBlocking<Unit> {
        val closedPort = ServerSocket(0).use { it.localPort }
        val error = assertFailsWith<PairingException> { repository.pair(target("ws://127.0.0.1:$closedPort/v1/ws"), "p") }.error
        assertIs<PairingError.Unreachable>(error)
    }

    @Test
    fun pendingRequestsNeedADecisionAndDiscardingForgetsThem() = runBlocking<Unit> {
        store.transaction { tx ->
            tx.setEpoch("e1")
            tx.addOutbox(OutboxEntry("c1", "turn/start", JsonObject(mapOf("clientRequestId" to JsonPrimitive("c1"))), 1))
        }
        server.enqueue(pairResponse(epoch = "e1"))
        val same = repository.pair(target(), "p")
        val plan = assertIs<OutboxPlan.Ask>(repository.plan(same))
        assertTrue(plan.sameServer)
        assertEquals(listOf("c1"), plan.entries.map { it.clientRequestId })

        server.enqueue(pairResponse(epoch = "e2"))
        val other = repository.pair(target(), "p")
        assertFalse(assertIs<OutboxPlan.Ask>(repository.plan(other)).sameServer)

        repository.apply(other, discardLocal = true)
        assertTrue(store.state.value.outbox.isEmpty())
        assertEquals(null, store.state.value.epoch)
        assertIs<PairingState.Paired>(credentials.current())
    }

    @Test
    fun unpairingOfflineForgetsEverythingAndSaysTheServerStillKnowsTheDevice() = runBlocking<Unit> {
        server.enqueue(pairResponse())
        repository.apply(repository.pair(target(), "p"), discardLocal = false)
        store.transaction { it.addOutbox(OutboxEntry("c1", "turn/start", JsonObject(mapOf("clientRequestId" to JsonPrimitive("c1"))), 1)) }
        keys.getOrCreateKey()

        val result = repository.unpair()
        assertEquals(UnpairResult("dev_new", revokedOnServer = false), result)
        assertEquals(PairingState.NotPaired, credentials.current())
        assertTrue(store.state.value.outbox.isEmpty())
        assertEquals(1, starter.stops)
        assertEquals(1, notificationsCleared, "the notifications of the pairing go with it")
    }
}
