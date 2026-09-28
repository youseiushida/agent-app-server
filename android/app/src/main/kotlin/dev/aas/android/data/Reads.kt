package dev.aas.android.data

import dev.aas.android.protocol.RpcMethod
import dev.aas.android.sync.ConnectionLostException
import dev.aas.android.sync.NotConnectedException
import dev.aas.android.sync.SyncEngine

/**
 * The repositories' read-only calls ([SyncEngine.query]; it refuses methods that change state).
 *
 * When the connection ends before the answer ([ConnectionLostException]: the engine's watchdog
 * closed a silent socket, the network changed, the daemon restarted), the engine reconnects by
 * itself. A read has no effect on the server, so it is sent again on the next connection instead
 * of failing the screen with the engine's "the connection closed" while the app is already back
 * online (found on the emulator: the new-project folder list failed that way right after pairing).
 * It waits at most [reconnectWaitMs] (AppPolicy.readReconnectWaitMs) for a new session, then
 * reports [NotConnectedException] (the screens' offline state); a second loss is reported as it is.
 *
 * The engine decides what "a new session" is ([SyncEngine.queryAcrossReconnect]): one other than
 * the session the call went on. Judging it here from the connection state when the failure is
 * seen took the new session for the lost one when the caller resumed only after the engine was
 * back online, and then waited for a third session that never came.
 */
class Reads(private val engine: SyncEngine, private val reconnectWaitMs: Long) {
    suspend fun <P, R> query(method: RpcMethod<P, R>, params: P): R = engine.queryAcrossReconnect(method, params, reconnectWaitMs)
}
