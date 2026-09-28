package dev.aas.android.testing

import dev.aas.android.AppPolicy
import dev.aas.android.data.ImageUploadException
import dev.aas.android.data.ImageUploader
import dev.aas.android.data.Reads
import dev.aas.android.data.UploadedImage
import dev.aas.android.protocol.AasJson
import dev.aas.android.protocol.ClientInfo
import dev.aas.android.protocol.Harness
import dev.aas.android.protocol.HarnessCapabilities
import dev.aas.android.protocol.HarnessKind
import dev.aas.android.protocol.Item
import dev.aas.android.protocol.Model
import dev.aas.android.protocol.EffortLevel
import dev.aas.android.protocol.PermissionMode
import dev.aas.android.protocol.RpcMessage
import dev.aas.android.protocol.ThreadReadResult
import dev.aas.android.protocol.WORKSPACE_STREAM
import dev.aas.android.sync.Credentials
import dev.aas.android.sync.FakeServer
import dev.aas.android.sync.InMemorySyncStore
import dev.aas.android.sync.ItemPosition
import dev.aas.android.sync.StoredItem
import dev.aas.android.sync.SyncConfig
import dev.aas.android.sync.SyncEngine
import dev.aas.android.sync.eventually
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.ExperimentalCoroutinesApi
import kotlinx.coroutines.SupervisorJob
import kotlinx.coroutines.asCoroutineDispatcher
import kotlinx.coroutines.cancel
import kotlinx.coroutines.runBlocking
import kotlinx.coroutines.test.resetMain
import kotlinx.coroutines.test.setMain
import kotlinx.coroutines.withTimeout
import kotlinx.serialization.KSerializer
import kotlinx.serialization.json.JsonElement
import kotlinx.serialization.json.jsonObject
import okhttp3.OkHttpClient
import org.junit.rules.TestWatcher
import org.junit.runner.Description
import java.io.File
import java.util.concurrent.ConcurrentHashMap
import java.util.concurrent.ExecutorService
import java.util.concurrent.Executors

/** The protocol's golden fixtures (`fixtures/protocol`, see app/build.gradle.kts). */
object Fixtures {
    private val root: File by lazy { File(System.getProperty("aas.fixtures") ?: error("the aas.fixtures system property is not set")) }

    fun json(category: String, name: String): JsonElement = AasJson.parseToJsonElement(File(root, "$category/$name.json").readText())

    /** The `result` of `responses/<name>.json`. */
    fun <T> result(name: String, serializer: KSerializer<T>): T =
        AasJson.decodeFromJsonElement(serializer, json("responses", name).jsonObject["result"]!!)

    /** `thread/read`: one turn with every item kind and two interactions. */
    val threadRead: ThreadReadResult by lazy { result("thread_read", ThreadReadResult.serializer()) }
}

/** Short engine policy values so tests run in milliseconds. */
val FAST_SYNC = SyncConfig(
    connectTimeoutMs = 5_000,
    initialClientTimeoutMs = 10_000,
    callTimeoutMs = 10_000,
    backoffBaseMs = 20,
    backoffCapMs = 200,
    outboxRetryBaseMs = 50,
    outboxRetryCapMs = 200,
)

/** Upper bound of one test (a hang fails the test instead of the build). */
const val TEST_TIMEOUT_MS = 60_000L

fun blockingTest(block: suspend CoroutineScope.() -> Unit) {
    runBlocking { withTimeout(TEST_TIMEOUT_MS) { block() } }
}

/**
 * A sync engine over an in-memory store, optionally connected to a scripted [FakeServer]
 * (answers of any method through [answers], keyed by method name).
 */
class TestEngine : AutoCloseable {
    val server = FakeServer()
    val store = InMemorySyncStore()
    val scope = CoroutineScope(SupervisorJob() + Dispatchers.Default)
    val engine = SyncEngine(store, OkHttpClient(), scope, ClientInfo("test", "0", "jvm"), FAST_SYNC)

    /** The repositories' reads, as the app wires them (AppContainer.reads). */
    val reads = Reads(engine, AppPolicy().readReconnectWaitMs)

    /** Results by method name; a method without one answers `{}`. */
    val answers = ConcurrentHashMap<String, (RpcMessage) -> Any?>()

    init {
        server.onRequest = { _, msg -> answers[msg.method]?.invoke(msg) ?: kotlinx.serialization.json.JsonObject(emptyMap()) }
    }

    /** Stores [read] (and its thread) as if synced earlier; the workspace cursor makes it a known store. */
    suspend fun seed(read: ThreadReadResult, harnesses: List<Harness> = listOf(fakeHarness()), projects: List<dev.aas.android.protocol.Project> = emptyList()) {
        store.transaction { tx ->
            tx.setEpoch(server.epoch)
            tx.setCursor(WORKSPACE_STREAM, 0)
            tx.replaceHarnesses(harnesses)
            projects.forEach { tx.upsertProject(it) }
            tx.upsertThread(read.thread)
            read.turns.forEach { tx.upsertTurn(it) }
            read.items.forEachIndexed { i, item -> tx.upsertItem(StoredItem(item, ItemPosition(read.turns.firstOrNull { it.id == item.turnId }?.index ?: ItemPosition.UNKNOWN_TURN, i.toLong()))) }
            read.interactions.forEach { tx.upsertInteraction(it) }
            tx.replaceQueued(read.thread.id, read.queued)
        }
    }

    /** The fake server's workspace and `thread/read` answer: [read]'s thread in [projects]. */
    fun serve(read: ThreadReadResult?, harnesses: List<Harness> = listOf(fakeHarness()), projects: List<dev.aas.android.protocol.Project> = emptyList()) {
        server.snapshot = dev.aas.android.protocol.WorkspaceSnapshotResult(
            harnesses = harnesses,
            projects = projects,
            threads = listOfNotNull(read?.thread),
            pendingInteractions = read?.interactions?.filter { it.status == dev.aas.android.protocol.InteractionStatus.Pending }.orEmpty(),
            operations = emptyList(),
            head = 0,
        )
        if (read != null) server.threadReads[read.thread.id] = read
    }

    /** Starts the engine against the fake server and waits for the session. */
    suspend fun connect() {
        engine.setCredentials(Credentials(server.wsUrl, "tok"))
        engine.start()
        eventually(what = "online") { engine.status.value.isOnline.takeIf { it } }
    }

    /** Starts the engine without a reachable server (the app's offline state). */
    fun startOffline() {
        engine.setCredentials(Credentials("ws://127.0.0.1:9/v1/ws", "tok"))
        engine.start()
    }

    fun requests(method: String): List<RpcMessage> = server.requestsFor(method).map { it.second }

    override fun close() {
        engine.stop()
        scope.cancel()
        server.close()
    }

    companion object {
        fun fakeHarness(steer: Boolean = true, images: Boolean = true) = Harness(
            id = "fake", kind = HarnessKind.Fake, displayName = "Fake", available = true,
            capabilities = HarnessCapabilities(interrupt = true, steer = steer, approvals = true, questions = true, resume = true, fork = true, images = images),
            models = listOf(Model("small", "Small", isDefault = true, effortLevels = listOf("low")), Model("large", "Large")),
            defaultModel = "small",
            effortLevels = listOf(EffortLevel("low", "Low"), EffortLevel("high", "High")),
            permissionModes = listOf(PermissionMode("ask", "Ask", isDefault = true), PermissionMode("full", "Full access", description = "No questions asked")),
            defaultPermissionMode = "ask",
        )
    }
}

/**
 * A single-threaded Main dispatcher for view models (viewModelScope uses Main.immediate), like
 * Android's main thread: the view model's coroutines run one at a time.
 */
@OptIn(ExperimentalCoroutinesApi::class)
class MainDispatcherRule : TestWatcher() {
    private lateinit var executor: ExecutorService

    override fun starting(description: Description) {
        executor = Executors.newSingleThreadExecutor { r -> Thread(r, "test-main") }
        Dispatchers.setMain(executor.asCoroutineDispatcher())
    }

    override fun finished(description: Description) {
        Dispatchers.resetMain()
        executor.shutdownNow()
    }
}

/** An image uploader that answers from a table (URI → blob) or fails. */
class FakeUploader(private val failing: Set<String> = emptySet()) : ImageUploader {
    val uploaded = mutableListOf<String>()

    override suspend fun upload(uri: String): UploadedImage {
        if (uri in failing) throw ImageUploadException.Unreadable(java.io.IOException("unreadable $uri"))
        uploaded += uri
        return UploadedImage("blb_${uri.hashCode().toUInt()}", "image/png", 1)
    }
}

/** The items of the fixture's turn, by kind. */
inline fun <reified T : Item> ThreadReadResult.item(): T = items.filterIsInstance<T>().first()
