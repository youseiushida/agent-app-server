package dev.aas.android.security

import androidx.datastore.preferences.core.PreferenceDataStoreFactory
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.SupervisorJob
import kotlinx.coroutines.cancel
import kotlinx.coroutines.flow.first
import kotlinx.coroutines.runBlocking
import kotlinx.coroutines.withTimeout
import org.junit.After
import org.junit.Rule
import org.junit.Test
import org.junit.runner.RunWith
import androidx.test.ext.junit.runners.AndroidJUnit4
import org.junit.rules.TemporaryFolder
import java.io.File
import java.security.ProviderException
import java.util.Base64
import javax.crypto.KeyGenerator
import javax.crypto.SecretKey
import kotlin.test.assertContentEquals
import kotlin.test.assertEquals
import kotlin.test.assertFailsWith
import kotlin.test.assertFalse
import kotlin.test.assertIs
import kotlin.test.assertTrue

/** A software AES key standing in for the Android Keystore (the cipher is plain JCA). */
class FakeKeyProvider : SecretKeyProvider {
    private var key: SecretKey? = null
    var created = 0
        private set

    /** The next calls that fail like a keystore whose service is busy or restarting. */
    @Volatile
    var failures = 0

    override fun getOrCreateKey(): SecretKey {
        if (failures > 0) {
            failures--
            throw ProviderException("keystore2 is not available (test)")
        }
        return key ?: KeyGenerator.getInstance("AES").apply { init(KEY_BITS) }.generateKey().also {
            key = it
            created++
        }
    }

    override fun deleteKey() {
        key = null
    }

    private companion object {
        const val KEY_BITS = 256
    }
}

class TokenCipherTest {
    private val keys = FakeKeyProvider()
    private val cipher = TokenCipher(keys)

    @Test
    fun roundTripsAndUsesAFreshIvEachTime() {
        val token = "dGhpcy1pcy1hLWRldmljZS10b2tlbi0zMi1ieXRlcw"
        val a = cipher.encrypt(token)
        val b = cipher.encrypt(token)
        assertEquals(token, cipher.decrypt(a))
        assertEquals(token, cipher.decrypt(b))
        assertFalse(a.contentEquals(b), "two encryptions of the same token must differ (random IV)")
        assertFalse(String(a, Charsets.ISO_8859_1).contains(token), "the ciphertext must not contain the token")
        assertEquals(1, keys.created)
    }

    @Test
    fun aChangedByteIsDetected() {
        val blob = cipher.encrypt("secret")
        blob[blob.size - 1] = (blob[blob.size - 1].toInt() xor 1).toByte()
        val e = assertFailsWith<TokenCipherException> { cipher.decrypt(blob) }
        assertEquals(TokenCipherException.Reason.AuthenticationFailed, e.reason)
    }

    @Test
    fun anotherKeyCannotDecrypt() {
        val blob = cipher.encrypt("secret")
        keys.deleteKey()
        val e = assertFailsWith<TokenCipherException> { cipher.decrypt(blob) }
        assertEquals(TokenCipherException.Reason.AuthenticationFailed, e.reason)
    }

    @Test
    fun foreignOrTruncatedBlobsAreCorrupt() {
        for (blob in listOf(byteArrayOf(), byteArrayOf(2, 12), byteArrayOf(1, 12, 0, 0, 0), byteArrayOf(1, 99) + ByteArray(40))) {
            val e = assertFailsWith<TokenCipherException> { cipher.decrypt(blob) }
            assertEquals(TokenCipherException.Reason.Corrupt, e.reason, "blob ${blob.toList()}")
        }
    }
}

/**
 * Robolectric: DataStore replaces its file with `Files.move(REPLACE_EXISTING)` on API 26+; on
 * the plain JVM (SDK_INT 0) it falls back to `File.renameTo`, which cannot replace a file on
 * Windows.
 */
@RunWith(AndroidJUnit4::class)
class CredentialStoreTest {
    @get:Rule
    val folder = TemporaryFolder()

    private var scope = CoroutineScope(SupervisorJob() + Dispatchers.IO)
    private val keys = FakeKeyProvider()
    private lateinit var file: File

    private fun store(retryInitialMs: Long = 50, retryMaxMs: Long = 200): CredentialStore {
        file = File(folder.root, "credentials.preferences_pb")
        val dataStore = PreferenceDataStoreFactory.create(scope = scope, produceFile = { file })
        return CredentialStore(dataStore, TokenCipher(keys), Dispatchers.IO, scope, retryInitialMs, retryMaxMs)
    }

    /** The same file read by a new process (a new store; the old one's DataStore released first). */
    private suspend fun reopen(retryInitialMs: Long = 50, retryMaxMs: Long = 200): CredentialStore {
        scope.coroutineContext[kotlinx.coroutines.Job]!!.let { job ->
            job.cancel()
            job.join()
        }
        scope = CoroutineScope(SupervisorJob() + Dispatchers.IO)
        return store(retryInitialMs, retryMaxMs)
    }

    @After
    fun cancel() = scope.cancel()

    private val info = PairingInfo("wss://pc.tail-x.ts.net/v1/ws", "home pc", "dev_1", "Pixel 9", pairedAtMs = 1_790_000_000_000)

    @Test
    fun savesReadsAndClearsWithoutStoringTheTokenInPlain() = runBlocking<Unit> {
        val store = store()
        assertEquals(PairingState.NotPaired, store.current())
        store.save(info, "the-device-token")
        val state = assertIs<PairingState.Paired>(store.current())
        assertEquals(info, state.pairing.info)
        assertEquals("the-device-token", state.pairing.token)
        assertEquals("wss://pc.tail-x.ts.net/v1/ws", state.pairing.credentials.wsUrl)
        assertFalse(state.pairing.toString().contains("the-device-token"))
        val raw = file.readBytes()
        assertFalse(String(raw, Charsets.ISO_8859_1).contains("the-device-token"), "the file must not hold the token in plain text")
        assertTrue(String(raw, Charsets.UTF_8).contains("pc.tail-x.ts.net"))
        store.clear()
        assertEquals(PairingState.NotPaired, store.current())
    }

    @Test
    fun aLostKeyMakesThePairingUnreadableNotAbsent() = runBlocking<Unit> {
        store().save(info, "the-device-token")
        keys.deleteKey()
        val state = assertIs<PairingState.Unreadable>(reopen().current())
        assertEquals(info, state.info)
        assertEquals(TokenCipherException.Reason.AuthenticationFailed, state.error.reason)
    }

    @Test
    fun aKeystoreFailureIsRetriedNotTakenForAnUnreadablePairing() = runBlocking<Unit> {
        store().save(info, "the-device-token")
        // After a reboot the keystore fails twice before it works.
        keys.failures = 2
        val store = reopen(retryInitialMs = 100, retryMaxMs = 1_000)
        val waiting = assertIs<PairingState.KeystoreUnavailable>(store.current())
        assertEquals(info, waiting.info)
        assertEquals(TokenCipherException.Reason.KeystoreFailure, waiting.error.reason)
        assertEquals(1, waiting.attempt)
        assertTrue(waiting.hasPairing, "the connection service keeps running for it")
        // Retried by itself, with a doubling wait, until it works.
        val paired = withTimeout(5_000) { store.state.first { it is PairingState.Paired } }
        assertEquals("the-device-token", assertIs<PairingState.Paired>(paired).pairing.token)
        assertEquals(0, keys.failures)
    }

    @Test
    fun aFileThatCannotBeReadFailsTheCallersInsteadOfLeavingThemWaiting() = runBlocking<Unit> {
        file = File(folder.root, "credentials.preferences_pb")
        file.writeBytes(byteArrayOf(0x7f, 0x13, 0x00, 0x42, 0x01))
        val failures = java.util.concurrent.CopyOnWriteArrayList<Throwable>()
        val failing = CoroutineScope(SupervisorJob() + Dispatchers.IO + kotlinx.coroutines.CoroutineExceptionHandler { _, e -> failures += e })
        try {
            val dataStore = PreferenceDataStoreFactory.create(scope = failing, produceFile = { file })
            val store = CredentialStore(dataStore, TokenCipher(keys), Dispatchers.IO, failing, 50, 200)
            val e = assertFailsWith<CredentialsUnreadableException> { withTimeout(5_000) { store.current() } }
            assertIs<java.io.IOException>(e.cause)
            // Also reported to the scope's handler (the app logs it).
            dev.aas.android.sync.eventually(what = "the failure reported to the handler") { failures.firstOrNull() }
        } finally {
            failing.cancel()
        }
    }

    @Test
    fun aKeystoreRetryCanBeAskedForAtOnce() = runBlocking<Unit> {
        store().save(info, "the-device-token")
        keys.failures = 1
        // A wait far longer than the test: only retryNow() can end it.
        val store = reopen(retryInitialMs = 60_000, retryMaxMs = 60_000)
        assertIs<PairingState.KeystoreUnavailable>(store.current())
        store.retryNow()
        assertIs<PairingState.Paired>(withTimeout(5_000) { store.state.first { it is PairingState.Paired } })
    }

    @Test
    fun tokensAreStoredAsVersionedCiphertext() = runBlocking<Unit> {
        val cipher = TokenCipher(keys)
        val blob = cipher.encrypt("t")
        // [version][ivLength][iv 12][ciphertext 1 + tag 16]
        assertEquals(1, blob[0].toInt())
        assertEquals(12, blob[1].toInt())
        assertEquals(2 + 12 + 1 + 16, blob.size)
        assertContentEquals(blob, Base64.getDecoder().decode(Base64.getEncoder().encodeToString(blob)))
    }
}
