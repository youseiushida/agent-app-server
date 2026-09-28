package dev.aas.android.sync

import dev.aas.android.protocol.ClientInfo
import dev.aas.android.protocol.Delivery
import dev.aas.android.protocol.InputPart
import dev.aas.android.protocol.RpcMessage
import dev.aas.android.protocol.TurnStartParams
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.SupervisorJob
import kotlinx.coroutines.cancel
import kotlinx.coroutines.launch
import kotlinx.coroutines.runBlocking
import kotlinx.coroutines.withTimeout
import kotlinx.serialization.json.JsonObject
import kotlinx.serialization.json.jsonArray
import kotlinx.serialization.json.jsonObject
import kotlinx.serialization.json.jsonPrimitive
import kotlinx.serialization.json.long
import okhttp3.OkHttpClient
import java.util.concurrent.CopyOnWriteArrayList
import kotlin.random.Random

/** Short policy values so engine tests run in milliseconds. */
val TEST_CONFIG = SyncConfig(
    connectTimeoutMs = 5_000,
    initialClientTimeoutMs = 10_000,
    callTimeoutMs = 10_000,
    backoffBaseMs = 20,
    backoffCapMs = 200,
    outboxRetryBaseMs = 50,
    outboxRetryCapMs = 200,
)

/** Upper bound of one engine test (a hang fails the test instead of the build). */
const val ENGINE_TEST_TIMEOUT_MS = 60_000L

/** Runs a test body with a timeout; always returns Unit (JUnit 4 needs void test methods). */
fun engineTest(block: suspend CoroutineScope.() -> Any?) {
    runBlocking { withTimeout(ENGINE_TEST_TIMEOUT_MS) { block() } }
}

/** A [SyncEngine] against a [FakeServer], with its store, scope and recorded signals/results. */
class EngineFixture(
    val server: FakeServer = FakeServer(),
    val store: InMemorySyncStore = InMemorySyncStore(),
    config: SyncConfig = TEST_CONFIG,
    random: Random = Random(1),
    /** The store the engine uses when it is not [store] itself (a wrapper around it). */
    storeOverride: SyncStore? = null,
    /** The HTTP client the engine builds its WebSocket client from (e.g. with a counting socket factory). */
    val http: OkHttpClient = OkHttpClient(),
) : AutoCloseable {
    val scope = CoroutineScope(SupervisorJob() + Dispatchers.Default)
    val signals = CopyOnWriteArrayList<SyncSignal>()
    val results = CopyOnWriteArrayList<OutboxResult>()
    val logs = CopyOnWriteArrayList<String>()
    val engine = SyncEngine(
        store = storeOverride ?: store,
        http = http,
        scope = scope,
        clientInfo = ClientInfo("test", "0", "jvm"),
        config = config,
        random = random,
        logger = { level, message, error -> logs += "$level $message${error?.let { " ($it)" } ?: ""}" },
    )

    init {
        scope.launch { engine.signals.collect { signals += it } }
        scope.launch { engine.results.collect { results += it } }
    }

    fun connect(token: String = "tok"): SyncEngine {
        engine.setCredentials(Credentials(server.wsUrl, token))
        engine.start()
        return engine
    }

    suspend fun awaitOnline() {
        try {
            eventually(what = "online") { engine.status.value.isOnline.takeIf { it } }
        } catch (e: AssertionError) {
            throw AssertionError("${e.message}; status ${engine.status.value}; log ${logs.takeLast(10)}")
        }
    }

    override fun close() {
        engine.stop()
        scope.cancel()
        server.close()
    }
}

fun turnStart(threadId: String, text: String): (String) -> TurnStartParams =
    { crid -> TurnStartParams(crid, threadId, listOf(InputPart.Text(text)), Delivery.Auto) }

fun subscriptionsOf(request: Pair<Int, RpcMessage>): Map<String, Long> =
    request.second.params!!.jsonObject["subscriptions"]!!.jsonArray.associate {
        it.jsonObject["stream"]!!.jsonPrimitive.content to it.jsonObject["after"]!!.jsonPrimitive.long
    }

fun crid(request: Pair<Int, RpcMessage>): String = (request.second.params as JsonObject)["clientRequestId"]!!.jsonPrimitive.content

fun withFixture(fixture: EngineFixture = EngineFixture(), block: suspend CoroutineScope.(EngineFixture) -> Any?): Unit =
    fixture.use { f -> engineTest { block(f) } }
