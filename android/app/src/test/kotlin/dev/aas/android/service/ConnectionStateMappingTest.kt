package dev.aas.android.service

import android.app.Application
import androidx.test.core.app.ApplicationProvider
import androidx.test.ext.junit.runners.AndroidJUnit4
import dev.aas.android.protocol.ShutdownReason
import dev.aas.android.security.PairingInfo
import dev.aas.android.security.PairingState
import dev.aas.android.security.TokenCipherException
import dev.aas.android.sync.ConnectionState
import dev.aas.android.sync.DisconnectCause
import dev.aas.android.sync.SuspendReason
import dev.aas.android.sync.SyncStatus
import org.junit.Test
import org.junit.runner.RunWith
import kotlin.test.assertEquals
import kotlin.test.assertFalse
import kotlin.test.assertNull
import kotlin.test.assertTrue

/** Every engine connection state maps to what the status bar and the notification say. */
class ConnectionPresentationTest {
    @Test
    fun everyStateHasASummary() {
        val cases = mapOf(
            ConnectionState.Stopped to ConnectionSummary.Stopped,
            ConnectionState.NotPaired to ConnectionSummary.NeedsPairing(PairingNeed.NotPaired),
            ConnectionState.Offline to ConnectionSummary.PhoneOffline,
            ConnectionState.Connecting(0, reconnecting = false) to ConnectionSummary.Connecting(0, false),
            ConnectionState.Connecting(2, reconnecting = true) to ConnectionSummary.Connecting(2, true),
            ConnectionState.Online(sinceMs = 5) to ConnectionSummary.Connected(5),
            ConnectionState.Reconnecting(3, 99, DisconnectCause.Network("refused")) to ConnectionSummary.WaitingToRetry(3, 99, RetryCause.Unreachable),
            ConnectionState.Suspended(SuspendReason.Replaced) to ConnectionSummary.ConnectedElsewhere,
            ConnectionState.Suspended(SuspendReason.Revoked) to ConnectionSummary.NeedsPairing(PairingNeed.Revoked),
            ConnectionState.Suspended(SuspendReason.Unauthorized(401)) to ConnectionSummary.NeedsPairing(PairingNeed.TokenRejected),
            ConnectionState.Suspended(SuspendReason.InvalidServerUrl("bad")) to ConnectionSummary.NeedsPairing(PairingNeed.InvalidServerUrl),
            ConnectionState.Suspended(SuspendReason.Incompatible("v2")) to ConnectionSummary.Incompatible("v2"),
        )
        for ((state, summary) in cases) assertEquals(summary, ConnectionPresentation.summarize(state), "for $state")
    }

    @Test
    fun disconnectCausesReduceToWhatTheUserCanActOn() {
        assertEquals(RetryCause.Unreachable, ConnectionPresentation.retryCause(DisconnectCause.Network("timeout")))
        assertEquals(RetryCause.Timeout, ConnectionPresentation.retryCause(DisconnectCause.Watchdog(45_000)))
        assertEquals(RetryCause.Timeout, ConnectionPresentation.retryCause(DisconnectCause.ServerTimeout))
        assertEquals(RetryCause.ServerRestarting, ConnectionPresentation.retryCause(DisconnectCause.ServerShutdown))
        assertEquals(RetryCause.NetworkChanged, ConnectionPresentation.retryCause(DisconnectCause.NetworkChanged))
        assertEquals(RetryCause.ProtocolError, ConnectionPresentation.retryCause(DisconnectCause.ProtocolViolation("binary")))
        for (other in listOf(DisconnectCause.MessageTooBig, DisconnectCause.Closed(1011, ""), DisconnectCause.SetupFailed("x"), DisconnectCause.ClientError("x"), DisconnectCause.Reset)) {
            assertEquals(RetryCause.Other, ConnectionPresentation.retryCause(other))
        }
    }

    @Test
    fun onlyStatesTheUserMustResolveNeedAttention() {
        fun attention(state: ConnectionState) = ConnectionPresentation.of(SyncStatus(connection = state), null).needsAttention
        assertTrue(attention(ConnectionState.Suspended(SuspendReason.Replaced)))
        assertTrue(attention(ConnectionState.Suspended(SuspendReason.Revoked)))
        assertTrue(attention(ConnectionState.Suspended(SuspendReason.Incompatible("v"))))
        assertFalse(attention(ConnectionState.Offline))
        assertFalse(attention(ConnectionState.Reconnecting(1, 1, DisconnectCause.ServerShutdown)))
        assertFalse(attention(ConnectionState.Online(1)))
    }

    @Test
    fun presentationCarriesOutboxAndLastSync() {
        val p = ConnectionPresentation.of(SyncStatus(connection = ConnectionState.Offline, pendingOutbox = 2, lastSyncAtMs = 77, serverShuttingDown = true), null)
        assertEquals(ConnectionPresentation(ConnectionSummary.PhoneOffline, 2, 77, ShutdownReason.Unknown), p)
        val storage = SyncStatus(connection = ConnectionState.Offline, serverShuttingDown = true, serverShutdownReason = ShutdownReason.StorageFailure)
        assertEquals(ShutdownReason.StorageFailure, ConnectionPresentation.of(storage, null).serverShutdown)
    }

    @Test
    fun aPairingWaitingForTheKeystoreIsNotNotPaired() {
        val info = PairingInfo("wss://pc/v1/ws", "pc", "dev_1", "phone", 1)
        val waiting = PairingState.KeystoreUnavailable(info, TokenCipherException(TokenCipherException.Reason.KeystoreFailure, "busy"), attempt = 2, retryAtMs = 500)
        // The engine has no credentials then: it says NotPaired, the app says why.
        val p = ConnectionPresentation.of(SyncStatus(connection = ConnectionState.NotPaired), waiting)
        assertEquals(ConnectionSummary.KeystoreUnavailable(500), p.summary)
        assertTrue(p.needsAttention)
        assertEquals(ConnectionSummary.NeedsPairing(PairingNeed.NotPaired), ConnectionPresentation.of(SyncStatus(connection = ConnectionState.NotPaired), PairingState.NotPaired).summary)
        // Once connected, the engine's state is what counts.
        assertEquals(ConnectionSummary.Connected(1), ConnectionPresentation.of(SyncStatus(connection = ConnectionState.Online(1)), waiting).summary)
    }

    @Test
    fun whileConnectedTheAdvancingSyncTimeDoesNotChangeThePresentation() {
        val online = ConnectionState.Online(sinceMs = 1)
        // Every batch and heartbeat moves lastSyncAtMs; the notification must not be re-posted for it.
        assertEquals(
            ConnectionPresentation.of(SyncStatus(connection = online, lastSyncAtMs = 100), null),
            ConnectionPresentation.of(SyncStatus(connection = online, lastSyncAtMs = 200), null),
        )
    }
}

/** The Japanese words of the status bar / notification. */
@RunWith(AndroidJUnit4::class)
class ConnectionTextsTest {
    private val res = ApplicationProvider.getApplicationContext<Application>().resources

    @Test
    fun wordsDistinguishPhoneOfflineFromPcUnreachable() {
        assertEquals("スマホがオフラインです", ConnectionTexts.title(res, ConnectionSummary.PhoneOffline))
        assertEquals("PC に接続できません", ConnectionTexts.title(res, ConnectionSummary.WaitingToRetry(1, 0, RetryCause.Unreachable)))
        assertEquals("別の場所で接続中", ConnectionTexts.title(res, ConnectionSummary.ConnectedElsewhere))
        assertEquals("アクセスが取り消されました", ConnectionTexts.title(res, ConnectionSummary.NeedsPairing(PairingNeed.Revoked)))
        assertEquals("接続済み", ConnectionTexts.title(res, ConnectionSummary.Connected(0)))
    }

    @Test
    fun detailListsAttemptPendingAndLastSyncWhenNotConnected() {
        val now = 10 * 60_000L
        val waiting = ConnectionPresentation(ConnectionSummary.WaitingToRetry(3, now + 5_000, RetryCause.Timeout), 2, now - 3 * 60_000, null)
        val detail = ConnectionTexts.detail(res, waiting, now)!!
        assertTrue(detail.startsWith("3 回目の再試行 · 送信待ち 2 件 · 最終同期 "), detail)
        // Connected with nothing pending: nothing to add.
        assertNull(ConnectionTexts.detail(res, ConnectionPresentation(ConnectionSummary.Connected(0), 0, now, null), now))
        assertEquals("送信待ち 1 件", ConnectionTexts.detail(res, ConnectionPresentation(ConnectionSummary.Connected(0), 1, now, null), now))
    }

    @Test
    fun aTimeAheadOfTheScreensClockIsSaidAsNow() {
        // The status bar's clock ticks every relativeTimeRefreshMs: the last sync can be after it
        // (and a PC's clock ahead of the phone's). It happened, so it is "0 分前", never "0 分後"
        // (the emulator showed "最終同期 In 0 min." while reconnecting).
        val now = 10 * 60_000L
        assertEquals(ConnectionTexts.relative(now, now), ConnectionTexts.relative(now + 20_000, now))
        assertEquals(ConnectionTexts.relative(now, now), ConnectionTexts.relative(now + 5 * 60_000, now))
        val waiting = ConnectionPresentation(ConnectionSummary.WaitingToRetry(1, now + 5_000, RetryCause.Timeout), 0, now + 20_000, null)
        assertEquals("1 回目の再試行 · 最終同期 ${ConnectionTexts.relative(now, now)}", ConnectionTexts.detail(res, waiting, now))
    }

    @Test
    fun aStorageFailStopIsSaidAsSuch() {
        val waiting = ConnectionPresentation(ConnectionSummary.WaitingToRetry(1, 0, RetryCause.ServerRestarting), 0, null, ShutdownReason.StorageFailure)
        assertEquals("1 回目の再試行 · サーバがデータを保存できなくなったため再起動しています", ConnectionTexts.detail(res, waiting, 0))
        val stopped = waiting.copy(serverShutdown = ShutdownReason.Shutdown)
        assertEquals("1 回目の再試行 · サーバが停止しました", ConnectionTexts.detail(res, stopped, 0))
        // A stop that nothing restarts (restartExpected: false) is not "restarting".
        val status = SyncStatus(
            connection = ConnectionState.Reconnecting(2, 0, DisconnectCause.ServerShutdown),
            serverShuttingDown = true,
            serverShutdownReason = ShutdownReason.Shutdown,
            serverRestartExpected = false,
        )
        val down = ConnectionPresentation.of(status, null)
        assertEquals(ConnectionSummary.WaitingToRetry(2, 0, RetryCause.ServerStopped), down.summary)
        assertEquals("サーバが停止しています", ConnectionTexts.title(res, down.summary))
        assertEquals("2 回目の再試行 · PC でサーバが起動されたら再接続します", ConnectionTexts.detail(res, down, 0))
        // The storage fail-stop under the watchdog restarts.
        val restarting = ConnectionPresentation.of(status.copy(serverShutdownReason = ShutdownReason.StorageFailure, serverRestartExpected = true), null)
        assertEquals(RetryCause.ServerRestarting, (restarting.summary as ConnectionSummary.WaitingToRetry).cause)
        assertEquals("サーバが再起動しています", ConnectionTexts.title(res, restarting.summary))
        assertEquals("端末のキーストアを使えません（再試行します）", ConnectionTexts.title(res, ConnectionSummary.KeystoreUnavailable(0)))
    }
}
