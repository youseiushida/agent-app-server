package dev.aas.android.data

import dev.aas.android.R
import dev.aas.android.protocol.AasJson
import dev.aas.android.protocol.ErrorKind
import dev.aas.android.protocol.FsListParams
import dev.aas.android.protocol.FsListResult
import dev.aas.android.protocol.Methods
import dev.aas.android.sync.CallTimeoutException
import dev.aas.android.sync.ConnectionLostException
import dev.aas.android.sync.NotConnectedException
import dev.aas.android.testing.Fixtures
import dev.aas.android.testing.TEST_TIMEOUT_MS
import dev.aas.android.testing.TestEngine
import dev.aas.android.ui.common.UiText
import dev.aas.android.ui.common.requestFailed
import kotlinx.coroutines.asCoroutineDispatcher
import kotlinx.coroutines.flow.first
import kotlinx.coroutines.runBlocking
import kotlinx.coroutines.withContext
import kotlinx.coroutines.withTimeout
import org.junit.After
import org.junit.Test
import java.io.IOException
import java.util.concurrent.Executors
import java.util.concurrent.atomic.AtomicInteger
import kotlin.test.assertEquals
import kotlin.test.assertFailsWith
import kotlin.test.assertNotEquals

/**
 * Reads that lose their connection before the answer (found on the emulator: the new-project
 * folder list failed with the engine's "the connection closed" right after pairing, while the
 * app was reconnecting by itself).
 */
class ReadsTest {
    private val env = TestEngine()
    private val listing = Fixtures.result("fs_list", FsListResult.serializer())
    private val listCalls = AtomicInteger(0)

    @After
    fun tearDown() = env.close()

    /** `fs/list` drops the connection instead of answering its first [drops] requests. */
    private fun dropFirstLists(drops: Int) {
        env.answers[Methods.FsList.name] = { _ ->
            if (listCalls.getAndIncrement() < drops) {
                env.server.lastConnection.kill()
                null
            } else {
                AasJson.encodeToJsonElement(FsListResult.serializer(), listing)
            }
        }
    }

    @Test
    fun aReadWhoseConnectionEndsIsSentAgainOnTheNextConnection() = runBlocking {
        withTimeout(TEST_TIMEOUT_MS) {
            env.connect()
            dropFirstLists(1)
            val result = ProjectRepository(env.engine, env.reads, env.lists).list("C:/work")
            assertEquals(listing, result)
            val sent = env.server.requestsFor(Methods.FsList.name)
            assertEquals(2, sent.size)
            assertNotEquals(sent[0].first, sent[1].first, "the second request goes on the new connection")
        }
    }

    /**
     * The caller notices the loss only after the engine is back online (its thread was busy, as
     * a main thread composing a screen can be): the read goes on that session. Judged from the
     * connection state at that moment, the new session looked like the lost one and the read
     * waited for a third one, then reported the app offline while it was online.
     */
    @Test
    fun aLossNoticedAfterTheEngineIsBackOnlineIsSentOnThatSession() = runBlocking {
        withTimeout(TEST_TIMEOUT_MS) {
            env.connect()
            val reconnectsBefore = env.engine.status.value.reconnects
            val callerThread = Executors.newSingleThreadExecutor { r -> Thread(r, "busy-caller") }
            val caller = callerThread.asCoroutineDispatcher()
            env.answers[Methods.FsList.name] = { _ ->
                if (listCalls.getAndIncrement() == 0) {
                    // Occupies the caller's thread before the loss reaches it: the read's
                    // continuation runs only once the engine has its next session.
                    callerThread.execute {
                        runBlocking { env.engine.status.first { it.isOnline && it.reconnects > reconnectsBefore } }
                    }
                    env.server.lastConnection.kill()
                    null
                } else {
                    AasJson.encodeToJsonElement(FsListResult.serializer(), listing)
                }
            }
            try {
                val result = withContext(caller) { Reads(env.engine, RECONNECT_WAIT_MS).query(Methods.FsList, FsListParams("C:/work")) }
                assertEquals(listing, result)
                val sent = env.server.requestsFor(Methods.FsList.name)
                assertEquals(2, sent.size)
                assertNotEquals(sent[0].first, sent[1].first, "the second request goes on the new connection")
            } finally {
                caller.close()
            }
        }
    }

    @Test
    fun withoutANewConnectionInTimeTheReadIsOffline() = runBlocking {
        withTimeout(TEST_TIMEOUT_MS) {
            env.connect()
            // Every later connection fails its handshake: the engine never gets back online.
            env.server.initializeError = { n -> if (n >= 1) ErrorKind.Internal else null }
            dropFirstLists(1)
            assertFailsWith<NotConnectedException> { Reads(env.engine, RECONNECT_WAIT_MS).query(Methods.FsList, FsListParams("C:/work")) }
            assertEquals(1, env.server.requestsFor(Methods.FsList.name).size)
        }
    }

    @Test
    fun aSecondLossIsReported() = runBlocking {
        withTimeout(TEST_TIMEOUT_MS) {
            env.connect()
            dropFirstLists(2)
            assertFailsWith<ConnectionLostException> { ProjectRepository(env.engine, env.reads, env.lists).list("C:/work") }
            assertEquals(2, env.server.requestsFor(Methods.FsList.name).size)
        }
    }

    @Test
    fun theEnginesOwnFailuresAreSaidInTheAppsWords() {
        assertEquals(UiText.of(R.string.error_connection_lost), requestFailed(ConnectionLostException("the connection closed")))
        assertEquals(UiText.of(R.string.error_no_response), requestFailed(CallTimeoutException("fs/list", 10_000)))
        assertEquals(UiText.of(R.string.error_request, "refused"), requestFailed(IOException("refused")))
    }

    private companion object {
        /**
         * Short: the engine cannot come back in the test of the offline case, and is back
         * already when the late caller notices the loss.
         */
        const val RECONNECT_WAIT_MS = 500L
    }
}
