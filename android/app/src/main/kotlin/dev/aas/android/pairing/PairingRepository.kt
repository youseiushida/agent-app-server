package dev.aas.android.pairing

import dev.aas.android.AppPolicy
import dev.aas.android.diagnostics.ConnectionLog
import dev.aas.android.protocol.DeviceRevokeParams
import dev.aas.android.protocol.ErrorKind
import dev.aas.android.protocol.Methods
import dev.aas.android.protocol.PairResponse
import dev.aas.android.protocol.RpcException
import dev.aas.android.security.CredentialStore
import dev.aas.android.security.PairingInfo
import dev.aas.android.security.SecretKeyProvider
import dev.aas.android.security.info
import dev.aas.android.service.ServiceStarter
import dev.aas.android.service.StartReason
import dev.aas.android.sync.AasHttp
import dev.aas.android.sync.Clock
import dev.aas.android.sync.ConnectionState
import dev.aas.android.sync.HttpApiException
import dev.aas.android.sync.OutboxEntry
import dev.aas.android.sync.SuspendReason
import dev.aas.android.sync.SyncEngine
import dev.aas.android.sync.SyncStore
import kotlinx.coroutines.CancellationException
import kotlinx.coroutines.flow.filter
import kotlinx.coroutines.flow.first
import kotlinx.coroutines.flow.flow
import kotlinx.coroutines.flow.map
import kotlinx.coroutines.flow.merge
import kotlinx.coroutines.withTimeoutOrNull
import kotlinx.serialization.SerializationException
import java.io.IOException
import java.io.InterruptedIOException
import java.net.UnknownHostException
import java.net.UnknownServiceException
import javax.net.ssl.SSLException

/** Why `POST /v1/pair` failed, in terms the pairing screen explains. */
sealed interface PairingError {
    /** 400 `invalidCode`: wrong, used or expired code. */
    data object InvalidCode : PairingError

    /** 429 `rateLimited`: too many attempts on the daemon; wait a minute. */
    data object RateLimited : PairingError

    /** The host name did not resolve (Tailscale off, MagicDNS name mistyped). */
    data class UnknownHost(val host: String) : PairingError

    /** No connection (refused, timed out, no route). */
    data class Unreachable(val message: String) : PairingError

    /** TLS failed (certificate, handshake). */
    data class Tls(val message: String) : PairingError

    /** This build may not use cleartext to the host (release: `wss://` only). */
    data class CleartextBlocked(val message: String) : PairingError

    /** Any other answer of the server (`{kind, message}` or a non-protocol body). */
    data class Rejected(val status: Int, val kind: String, val message: String) : PairingError

    /** The server answered 200 with a body that is not a pairing response. */
    data class InvalidResponse(val message: String) : PairingError

    /** The pairing succeeded but storing it on this device failed (keystore, disk). */
    data class LocalStorage(val message: String) : PairingError
}

class PairingException(val error: PairingError) : Exception(error.toString())

/** A completed `POST /v1/pair` whose credentials are not stored yet. */
data class PairedDevice(val target: PairingTarget, val deviceName: String, val response: PairResponse)

/**
 * What to do with requests still in the outbox when a new pairing replaces the old one. A new
 * pairing is always a new device id, and the server keys idempotency by `(deviceId,
 * clientRequestId)`: resending an entry the old device's connection may already have delivered
 * could run it twice, and after a daemon reset (new epoch) the entries refer to things that no
 * longer exist. So the user decides.
 */
sealed interface OutboxPlan {
    /** Nothing waits in the outbox: apply the pairing. */
    data object NothingPending : OutboxPlan

    /** Ask whether to send or discard [entries]. [sameServer]: the stored data comes from the same epoch. */
    data class Ask(val entries: List<OutboxEntry>, val sameServer: Boolean) : OutboxPlan
}

/** Result of [PairingRepository.unpair]. */
data class UnpairResult(val deviceId: String?, val revokedOnServer: Boolean)

/**
 * Pairing and unpairing: `POST /v1/pair`, storing the credentials (token encrypted), telling the
 * engine and starting the connection service. [http] must bound the whole call (OkHttp
 * `callTimeout`), since a blocking call cannot be cancelled from a coroutine.
 */
class PairingRepository(
    private val http: AasHttp,
    private val credentials: CredentialStore,
    private val keys: SecretKeyProvider,
    private val engine: SyncEngine,
    private val store: SyncStore,
    private val starter: ServiceStarter,
    private val policy: AppPolicy,
    private val log: ConnectionLog,
    /** Removes the app's notifications (approvals with working buttons among them) when unpairing. */
    private val clearNotifications: () -> Unit,
    private val clock: Clock = Clock.System,
) {
    /** Exchanges the code for a device token. Nothing is stored yet (see [plan] and [apply]). */
    suspend fun pair(target: PairingTarget, deviceName: String): PairedDevice {
        log.info(ConnectionLog.SOURCE_PAIRING, "pairing with ${target.host}")
        val response = try {
            // [http] is built with an OkHttp call timeout of AppPolicy.pairCallTimeoutMs.
            http.pair(target.wsUrl, target.code, deviceName)
        } catch (e: HttpApiException) {
            throw PairingException(
                when {
                    e.kind == KIND_INVALID_CODE -> PairingError.InvalidCode
                    e.kind == ErrorKind.RateLimited.wire || e.status == HTTP_TOO_MANY_REQUESTS -> PairingError.RateLimited
                    else -> PairingError.Rejected(e.status, e.kind, e.detail)
                },
            )
        } catch (e: CancellationException) {
            throw e
        } catch (e: SerializationException) {
            throw PairingException(PairingError.InvalidResponse(e.message ?: "unreadable response"))
        } catch (e: IllegalArgumentException) {
            // AasJson reports a wrong shape as IllegalArgumentException, OkHttp an unusable URL.
            throw PairingException(PairingError.InvalidResponse(e.message ?: "unreadable response"))
        } catch (e: IOException) {
            throw PairingException(ioError(e, target))
        }
        log.info(ConnectionLog.SOURCE_PAIRING, "paired as device ${response.deviceId} with ${response.server.name}")
        return PairedDevice(target, deviceName, response)
    }

    /** Whether the user must decide about the outbox before [apply]. */
    suspend fun plan(device: PairedDevice): OutboxPlan {
        val (entries, epoch) = store.transaction { it.outbox() to it.epoch() }
        if (entries.isEmpty()) return OutboxPlan.NothingPending
        return OutboxPlan.Ask(entries, sameServer = epoch == device.response.server.epoch)
    }

    /**
     * Stores the pairing and connects. [discardLocal] forgets the local data and the outbox first
     * (the user chose to discard pending requests).
     */
    suspend fun apply(device: PairedDevice, discardLocal: Boolean) {
        if (discardLocal) engine.resetLocalData()
        val info = PairingInfo(
            wsUrl = device.target.wsUrl,
            serverName = device.response.server.name,
            deviceId = device.response.deviceId,
            deviceName = device.deviceName,
            pairedAtMs = clock.nowMs(),
        )
        credentials.save(info, device.response.token)
        starter.requestStart(StartReason.Paired)
    }

    /**
     * Unpairs: revokes this device on the server when connected (so its token stops working),
     * then forgets everything local — synced data, outbox, the token and its keystore key — and
     * the notifications, and stops the connection service.
     */
    suspend fun unpair(): UnpairResult {
        val deviceId = credentials.current().info?.deviceId
        val revoked = if (deviceId != null && engine.status.value.isOnline) revokeSelf(deviceId) else false
        engine.resetLocalData()
        credentials.clear()
        keys.deleteKey()
        // No pairing now, so nothing new is posted; what was posted goes (an approval's buttons
        // would otherwise queue answers for whatever server the device pairs with next).
        clearNotifications()
        starter.stop()
        log.info(ConnectionLog.SOURCE_PAIRING, "unpaired (revoked on the server: $revoked)")
        return UnpairResult(deviceId, revoked)
    }

    /**
     * `device/revoke` for this device. The server may close the connection (4001) before its
     * answer arrives, so the revocation counts as done on either signal.
     */
    private suspend fun revokeSelf(deviceId: String): Boolean {
        val answered = flow {
            val ok = try {
                engine.mutate(Methods.DeviceRevoke) { DeviceRevokeParams(it, deviceId) }
                true
            } catch (e: RpcException) {
                // notFound: the device was already revoked.
                e.kind == ErrorKind.NotFound
            }
            emit(ok)
        }
        val closed = engine.status
            .filter { (it.connection as? ConnectionState.Suspended)?.reason == SuspendReason.Revoked }
            .map { true }
        return try {
            withTimeoutOrNull(policy.unpairRevokeTimeoutMs) { merge(answered, closed).first() } ?: false
        } catch (e: CancellationException) {
            throw e
        } catch (e: Exception) {
            log.warn(ConnectionLog.SOURCE_PAIRING, "revoking this device failed", e)
            false
        }
    }

    private fun ioError(e: IOException, target: PairingTarget): PairingError = when (e) {
        is UnknownHostException -> PairingError.UnknownHost(target.host)
        is SSLException -> PairingError.Tls(e.message ?: e.javaClass.simpleName)
        // OkHttp: "CLEARTEXT communication to <host> not permitted by network security policy".
        is UnknownServiceException -> PairingError.CleartextBlocked(e.message ?: e.javaClass.simpleName)
        is InterruptedIOException -> PairingError.Unreachable(e.message ?: "timeout")
        else -> PairingError.Unreachable(e.message ?: e.javaClass.simpleName)
    }

    private companion object {
        /** The `kind` of `POST /v1/pair`'s 400 for a wrong, used or expired code (protocol.md §6). */
        const val KIND_INVALID_CODE = "invalidCode"
        const val HTTP_TOO_MANY_REQUESTS = 429
    }
}
