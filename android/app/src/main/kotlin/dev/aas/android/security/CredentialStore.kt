package dev.aas.android.security

import androidx.datastore.core.DataStore
import androidx.datastore.preferences.core.Preferences
import androidx.datastore.preferences.core.edit
import androidx.datastore.preferences.core.longPreferencesKey
import androidx.datastore.preferences.core.stringPreferencesKey
import dev.aas.android.sync.Clock
import dev.aas.android.sync.Credentials
import kotlinx.coroutines.CoroutineDispatcher
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.ExperimentalCoroutinesApi
import kotlinx.coroutines.channels.Channel
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.SharingStarted
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.flow.catch
import kotlinx.coroutines.flow.combine
import kotlinx.coroutines.flow.first
import kotlinx.coroutines.flow.stateIn
import kotlinx.coroutines.flow.transformLatest
import kotlinx.coroutines.withContext
import kotlinx.coroutines.withTimeoutOrNull
import java.util.Base64

/** What is known about the pairing, without the token. */
data class PairingInfo(
    /** The daemon's WebSocket URL (`wss://<pc>.<tailnet>.ts.net/v1/ws`). */
    val wsUrl: String,
    /** The server name from the pairing QR / `POST /v1/pair`. */
    val serverName: String,
    /** This device's id on the server. */
    val deviceId: String,
    /** The name this device registered with. */
    val deviceName: String,
    val pairedAtMs: Long,
)

/** A usable pairing: the info and the decrypted device token. */
data class Pairing(val info: PairingInfo, val token: String) {
    val credentials: Credentials get() = Credentials(info.wsUrl, token)

    override fun toString(): String = "Pairing(info=$info, token=<redacted>)"
}

/** The stored pairing as the app sees it. */
sealed interface PairingState {
    /** Never paired, or unpaired. */
    data object NotPaired : PairingState

    data class Paired(val pairing: Pairing) : PairingState

    /**
     * A pairing is stored but its token can never be decrypted: the keystore key is gone (e.g.
     * the app's data was restored on another device) or the stored blob is damaged. The user
     * must pair again; [info] tells which server it was.
     */
    data class Unreadable(val info: PairingInfo, val error: TokenCipherException) : PairingState

    /**
     * A pairing is stored, but the keystore could not run the cipher just now
     * ([TokenCipherException.Reason.KeystoreFailure]: its service busy or still starting after a
     * reboot, a provider error). Nothing is wrong with the pairing: the store decrypts again at
     * [retryAtMs] (wall clock; [attempt] failures so far) or at once on
     * [CredentialStore.retryNow], and the state becomes [Paired].
     */
    data class KeystoreUnavailable(val info: PairingInfo, val error: TokenCipherException, val attempt: Int, val retryAtMs: Long) : PairingState
}

/** The stored pairing's info, whether or not its token can be used. */
val PairingState.info: PairingInfo?
    get() = when (this) {
        is PairingState.Paired -> pairing.info
        is PairingState.Unreadable -> info
        is PairingState.KeystoreUnavailable -> info
        PairingState.NotPaired -> null
    }

/** The credentials file could not be read ([cause]: DataStore's I/O or corruption error). */
class CredentialsUnreadableException(cause: Throwable) : Exception("the stored pairing cannot be read: ${cause.message ?: cause.javaClass.simpleName}", cause)

/**
 * A pairing is stored that is, or may become, usable: [PairingState.Paired], or waiting for the
 * keystore. The connection service runs for it; only [PairingState.NotPaired] and
 * [PairingState.Unreadable] end it.
 */
val PairingState.hasPairing: Boolean
    get() = this is PairingState.Paired || this is PairingState.KeystoreUnavailable

/**
 * Stores the pairing in a DataStore file; the device token only encrypted by [TokenCipher]
 * (Android Keystore AES-GCM). The file is excluded from backups (res/xml/data_extraction_rules.xml):
 * a token restored elsewhere could not be decrypted anyway, and a device identity must not be
 * cloned.
 *
 * [state] is the one decoded view of the stored pairing that every part of the app uses (the
 * engine's credentials, the connection service, the boot receiver, the screens), so they never
 * disagree. A keystore failure is not taken for an unreadable pairing: the token is decrypted
 * again with a doubling wait from [retryInitialMs] up to [retryMaxMs], or at once on [retryNow].
 */
class CredentialStore(
    private val dataStore: DataStore<Preferences>,
    private val cipher: TokenCipher,
    private val cryptoDispatcher: CoroutineDispatcher,
    scope: CoroutineScope,
    private val retryInitialMs: Long,
    private val retryMaxMs: Long,
    private val clock: Clock = Clock.System,
) {
    init {
        require(retryInitialMs > 0 && retryMaxMs >= retryInitialMs) { "0 < retryInitialMs <= retryMaxMs" }
    }

    private val retryKick = Channel<Unit>(Channel.CONFLATED)

    /** Why the file cannot be read (then [state] stops changing); callers of [current] get it. */
    private val readFailure = MutableStateFlow<Throwable?>(null)

    /** The stored pairing, decoded once per change of the file (`null` until first read). */
    @OptIn(ExperimentalCoroutinesApi::class)
    val state: StateFlow<PairingState?> = dataStore.data
        .transformLatest { prefs ->
            var attempt = 0
            // A kick left over from an earlier wait must not skip this one.
            retryKick.tryReceive()
            while (true) {
                val decoded = withContext(cryptoDispatcher) { decode(prefs) }
                if (decoded !is PairingState.KeystoreUnavailable) {
                    emit(decoded)
                    break
                }
                attempt++
                val wait = retryDelayMs(attempt)
                emit(decoded.copy(attempt = attempt, retryAtMs = clock.nowMs() + wait))
                withTimeoutOrNull(wait) { retryKick.receive() }
            }
        }
        // Recorded for the callers waiting on [state], then rethrown to the scope's handler (logged).
        .catch { e ->
            readFailure.value = e
            throw e
        }
        .stateIn(scope, SharingStarted.Eagerly, null)

    /**
     * The current pairing (waits for the first read of the file).
     *
     * @throws CredentialsUnreadableException the file cannot be read.
     */
    suspend fun current(): PairingState = awaitState { true }

    /** Decrypts again now when the last attempt failed in the keystore (e.g. the app came to the foreground). */
    fun retryNow() {
        if (state.value is PairingState.KeystoreUnavailable) retryKick.trySend(Unit)
    }

    /** Stores a new pairing (replacing any previous one); returns once [state] shows it. */
    suspend fun save(info: PairingInfo, token: String) {
        val sealed = withContext(cryptoDispatcher) { cipher.encrypt(token) }
        dataStore.edit {
            it[Keys.WS_URL] = info.wsUrl
            it[Keys.SERVER_NAME] = info.serverName
            it[Keys.DEVICE_ID] = info.deviceId
            it[Keys.DEVICE_NAME] = info.deviceName
            it[Keys.PAIRED_AT] = info.pairedAtMs
            it[Keys.TOKEN] = Base64.getEncoder().encodeToString(sealed)
        }
        // Whoever reads [state] next (the service this pairing starts) must see this pairing.
        awaitState { it.info == info }
    }

    /** Forgets the pairing; returns once [state] shows it. */
    suspend fun clear() {
        dataStore.edit { it.clear() }
        awaitState { it == PairingState.NotPaired }
    }

    /** Waits until [state] satisfies [predicate]; a file that cannot be read ends the wait with its failure. */
    private suspend fun awaitState(predicate: (PairingState) -> Boolean): PairingState {
        val (value, failure) = combine(state, readFailure) { value, failure -> value to failure }
            .first { (value, failure) -> failure != null || (value != null && predicate(value)) }
        if (failure != null) throw CredentialsUnreadableException(failure)
        return checkNotNull(value)
    }

    private fun retryDelayMs(attempt: Int): Long {
        val shift = (attempt - 1).coerceIn(0, MAX_SHIFT)
        val delay = retryInitialMs shl shift
        return if (delay <= 0 || delay > retryMaxMs) retryMaxMs else delay
    }

    private fun decode(prefs: Preferences): PairingState {
        val info = PairingInfo(
            wsUrl = prefs[Keys.WS_URL] ?: return PairingState.NotPaired,
            serverName = prefs[Keys.SERVER_NAME].orEmpty(),
            deviceId = prefs[Keys.DEVICE_ID] ?: return PairingState.NotPaired,
            deviceName = prefs[Keys.DEVICE_NAME].orEmpty(),
            pairedAtMs = prefs[Keys.PAIRED_AT] ?: 0L,
        )
        val encoded = prefs[Keys.TOKEN] ?: return PairingState.NotPaired
        val blob = try {
            Base64.getDecoder().decode(encoded)
        } catch (e: IllegalArgumentException) {
            return PairingState.Unreadable(info, TokenCipherException(TokenCipherException.Reason.Corrupt, "the stored token is not base64", e))
        }
        return try {
            PairingState.Paired(Pairing(info, cipher.decrypt(blob)))
        } catch (e: TokenCipherException) {
            when (e.reason) {
                // The keystore could not run the cipher: nothing says the token is lost.
                TokenCipherException.Reason.KeystoreFailure -> PairingState.KeystoreUnavailable(info, e, attempt = 0, retryAtMs = 0)
                TokenCipherException.Reason.Corrupt, TokenCipherException.Reason.AuthenticationFailed -> PairingState.Unreadable(info, e)
            }
        }
    }

    private object Keys {
        val WS_URL = stringPreferencesKey("wsUrl")
        val SERVER_NAME = stringPreferencesKey("serverName")
        val DEVICE_ID = stringPreferencesKey("deviceId")
        val DEVICE_NAME = stringPreferencesKey("deviceName")
        val PAIRED_AT = longPreferencesKey("pairedAt")
        val TOKEN = stringPreferencesKey("tokenCiphertext")
    }

    private companion object {
        /** Doubling stops here (2^30 × the first wait is far beyond any cap); larger shifts would overflow. */
        const val MAX_SHIFT = 30
    }
}
