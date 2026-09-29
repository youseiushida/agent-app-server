package dev.aas.android.sync

import dev.aas.android.protocol.Event
import dev.aas.android.protocol.ThreadReadResult
import dev.aas.android.protocol.WorkspaceSnapshotResult
import dev.aas.android.protocol.threadStream
import kotlin.test.Test
import kotlin.test.assertEquals

/**
 * What the harness relays to an open thread without it being stored (`composer/insert`,
 * `thread/nativeSessionChanged`): text for the composer says whether it happened after the
 * connection subscribed (the subscription's head) or is the catch-up from the stored cursor
 * after a reconnect (protocol.md §5: the client may offer the latter instead of putting it in).
 */
class SyncEngineRelayedEventsTest {
    private val stream = threadStream("thr_1")

    private fun threadRead(head: Long) = ThreadReadResult(
        thread = Samples.thread("thr_1", head = head),
        turns = listOf(Samples.turn("trn_1", threadId = "thr_1")),
        items = emptyList(),
        interactions = emptyList(),
        queued = emptyList(),
        head = head,
        hasMoreBefore = false,
    )

    @Test
    fun composerTextOfTheCatchUpIsNotLiveAndNewTextIs() = withFixture { f ->
        f.server.snapshot = WorkspaceSnapshotResult(emptyList(), emptyList(), listOf(Samples.thread("thr_1", head = 10)), emptyList(), emptyList(), 0)
        f.server.threadReads["thr_1"] = threadRead(10)
        f.connect()
        f.awaitOnline()
        val state = f.engine.openThread("thr_1")
        eventually(what = "live") { state.value.takeIf { it.sync == ThreadSync.Live } }

        // While the phone is away, the harness asks for composer text and moves the agent.
        f.server.append(stream, Event.ComposerInsert("while you were away"), seq = 11)
        f.server.append(stream, Event.NativeSessionChanged("ses-a", "ses-b"), seq = 12)
        f.server.lastConnection.kill()
        eventually(what = "reconnected") { f.server.connections.size.takeIf { it == 2 && f.engine.status.value.isOnline } }
        val caughtUp = eventually(what = "the catch-up's text") { f.signals.filterIsInstance<SyncSignal.ComposerInsert>().firstOrNull() }
        assertEquals(SyncSignal.ComposerInsert("thr_1", "while you were away", live = false), caughtUp)
        eventually(what = "the session change") { f.signals.filterIsInstance<SyncSignal.NativeSessionChanged>().firstOrNull() }

        // Now, on the live subscription.
        f.server.append(stream, Event.ComposerInsert("right now"), seq = 13)
        f.server.lastConnection.pushNew(stream)
        val live = eventually(what = "the live text") { f.signals.filterIsInstance<SyncSignal.ComposerInsert>().firstOrNull { it.text == "right now" } }
        assertEquals(true, live.live)
        // Nothing of either is stored: only the cursor moved.
        assertEquals(13L, f.store.state.value.cursors[stream])
    }
}
