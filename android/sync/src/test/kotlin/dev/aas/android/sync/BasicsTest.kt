package dev.aas.android.sync

import dev.aas.android.protocol.AasJson
import dev.aas.android.protocol.BlobUploadResponse
import dev.aas.android.protocol.PairResponse
import dev.aas.android.protocol.PairServerInfo
import kotlinx.coroutines.runBlocking
import mockwebserver3.MockResponse
import mockwebserver3.MockWebServer
import okhttp3.OkHttpClient
import kotlin.random.Random
import kotlin.test.Test
import kotlin.test.assertContentEquals
import kotlin.test.assertEquals
import kotlin.test.assertFailsWith
import kotlin.test.assertTrue

class BackoffTest {
    @Test
    fun delaysAreFullJitterAndCapped() {
        val b = Backoff(baseMs = 500, capMs = 30_000, random = Random(7))
        repeat(200) { attempt ->
            val d = b.delayMs(attempt)
            assertTrue(d in 0..b.maxDelayMs(attempt), "attempt $attempt: $d")
        }
        assertEquals(500, b.maxDelayMs(0))
        assertEquals(1_000, b.maxDelayMs(1))
        assertEquals(16_000, b.maxDelayMs(5))
        assertEquals(30_000, b.maxDelayMs(6))
        assertEquals(30_000, b.maxDelayMs(1_000), "no overflow for large attempts")
        // Full jitter: the whole window is used, including very short waits.
        val samples = (0 until 2_000).map { b.delayMs(10) }
        assertTrue(samples.min() < 3_000 && samples.max() > 27_000, "spread ${samples.min()}..${samples.max()}")
    }
}

class SyncConfigTest {
    @Test
    fun outboxRetryDelaysDoubleUpToTheCap() {
        val c = SyncConfig()
        assertEquals(listOf(2_000L, 4_000L, 8_000L, 16_000L, 32_000L, 60_000L, 60_000L), (1..7).map { c.outboxRetryDelayMs(it) })
        assertEquals(60_000L, c.outboxRetryDelayMs(500))
    }

    @Test
    fun invalidPoliciesAreRejected() {
        assertFailsWith<IllegalArgumentException> { SyncConfig(backoffBaseMs = 100, backoffCapMs = 50) }
        assertFailsWith<IllegalArgumentException> { SyncConfig(threadPageTurns = 0) }
        assertFailsWith<IllegalArgumentException> { SyncConfig(callTimeoutMs = 0) }
        assertFailsWith<IllegalArgumentException> { SyncConfig(socketReleaseTimeoutMs = 0) }
    }
}

class InMemorySyncStoreTest : SyncStoreContract() {
    override fun newStore(): SyncStore = InMemorySyncStore()

    @Test
    fun theTransactionObjectIsUnusableAfterwards() = runBlocking<Unit> {
        val store = InMemorySyncStore()
        val leaked = store.transaction { it }
        assertFailsWith<IllegalStateException> { leaked.setEpoch("e") }
    }
}

class AasHttpTest {
    @Test
    fun pairingUploadAndDownload() = runBlocking<Unit> {
        MockWebServer().use { server ->
            server.start()
            val ws = server.url("/v1/ws").toString().replaceFirst("http", "ws")
            val http = AasHttp(OkHttpClient())
            server.enqueue(
                MockResponse.Builder().body(
                    AasJson.encodeToString(PairResponse.serializer(), PairResponse("dev_1", "secret", PairServerInfo("pc", "e1"))),
                ).build(),
            )
            val paired = http.pair(ws, "ABCD-1234", "Pixel")
            assertEquals("secret", paired.token)
            val pairReq = server.takeRequest()
            assertEquals("/v1/pair", pairReq.url.encodedPath)
            assertTrue(pairReq.body!!.utf8().contains("\"code\":\"ABCD-1234\""))

            server.enqueue(
                MockResponse.Builder().body(
                    AasJson.encodeToString(BlobUploadResponse.serializer(), BlobUploadResponse("blb_1", "image/png", 3)),
                ).build(),
            )
            val creds = Credentials(ws, "secret")
            val up = http.uploadBlob(creds, byteArrayOf(1, 2, 3), "image/png")
            assertEquals("blb_1", up.blobId)
            val upReq = server.takeRequest()
            assertEquals("Bearer secret", upReq.headers["Authorization"])
            assertEquals("image/png", upReq.headers["Content-Type"])

            server.enqueue(MockResponse.Builder().body(okio.Buffer().write(byteArrayOf(9, 8, 7))).build())
            assertContentEquals(byteArrayOf(9, 8, 7), http.downloadBlob(creds, "blb_1"))
            assertEquals("/v1/blobs/blb_1", server.takeRequest().url.encodedPath)

            server.enqueue(MockResponse.Builder().body(okio.Buffer().write(ByteArray(100))).build())
            val tooBig = assertFailsWith<HttpApiException> { http.downloadBlob(creds, "blb_2", maxBytes = 10) }
            assertEquals("payloadTooLarge", tooBig.kind)

            server.enqueue(MockResponse.Builder().code(400).body("""{"kind":"invalidCode","message":"expired"}""").build())
            val err = assertFailsWith<HttpApiException> { http.pair(ws, "x", "y") }
            assertEquals("invalidCode", err.kind)
            assertEquals(400, err.status)

            server.enqueue(MockResponse.Builder().code(502).body("<html>bad gateway</html>").build())
            val proxy = assertFailsWith<HttpApiException> { http.pair(ws, "x", "y") }
            assertEquals("http502", proxy.kind)
            assertTrue(proxy.message!!.contains("bad gateway"))
        }
    }

    @Test
    fun credentialsNeverPrintTheToken() {
        assertTrue("secret-token" !in Credentials("ws://h/v1/ws", "secret-token").toString())
    }
}
