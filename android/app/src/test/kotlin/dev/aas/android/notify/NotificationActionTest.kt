package dev.aas.android.notify

import android.app.Application
import android.content.Intent
import android.os.Looper
import androidx.test.core.app.ApplicationProvider
import androidx.test.ext.junit.runners.AndroidJUnit4
import dev.aas.android.appContainer
import dev.aas.android.pairing.FakeStarter
import dev.aas.android.protocol.AasJson
import dev.aas.android.protocol.ClientInfo
import dev.aas.android.protocol.InteractionRespondParams
import dev.aas.android.protocol.InteractionResolution
import dev.aas.android.protocol.Methods
import dev.aas.android.security.Pairing
import dev.aas.android.security.PairingInfo
import dev.aas.android.security.PairingState
import dev.aas.android.service.ConnectionService
import dev.aas.android.service.StartReason
import dev.aas.android.sync.InMemorySyncStore
import dev.aas.android.sync.SyncEngine
import dev.aas.android.sync.eventually
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.SupervisorJob
import kotlinx.coroutines.cancel
import kotlinx.coroutines.runBlocking
import okhttp3.OkHttpClient
import org.junit.Test
import org.junit.runner.RunWith
import org.robolectric.Shadows.shadowOf
import kotlin.test.assertEquals
import kotlin.test.assertFailsWith
import kotlin.test.assertNotNull
import kotlin.test.assertTrue

/** A notification action commits `interaction/respond` to the outbox, even with the engine stopped. */
class InteractionResponderTest {
    private val paired = PairingState.Paired(Pairing(PairingInfo("ws://pc/v1/ws", "pc", "dev_1", "phone", 1), "token"))

    @Test
    fun anActionBecomesADurableOutboxEntryAndStartsTheService() = runBlocking<Unit> {
        val scope = CoroutineScope(SupervisorJob() + Dispatchers.Default)
        try {
            val store = InMemorySyncStore()
            // Not started: the connection service may not be running when the user taps.
            val engine = SyncEngine(store, OkHttpClient(), scope, ClientInfo("test", "0", "android"), newRequestId = { "crid-1" })
            val starter = FakeStarter()

            val crid = InteractionResponder(engine, starter) { paired }.respond("int_1", "allow")

            assertEquals("crid-1", crid)
            val entry = store.state.value.outbox.single()
            assertEquals(Methods.InteractionRespond.name, entry.method)
            val params = AasJson.decodeFromJsonElement(InteractionRespondParams.serializer(), entry.params)
            assertEquals(InteractionRespondParams("crid-1", "int_1", InteractionResolution.Approval("allow")), params)
            assertEquals(listOf(StartReason.NotificationAction), starter.starts)
            assertEquals(1L, store.commits)
        } finally {
            scope.cancel()
        }
    }

    @Test
    fun withoutAPairingNothingIsQueued() = runBlocking<Unit> {
        val scope = CoroutineScope(SupervisorJob() + Dispatchers.Default)
        try {
            val store = InMemorySyncStore()
            val engine = SyncEngine(store, OkHttpClient(), scope, ClientInfo("test", "0", "android"))
            val starter = FakeStarter()
            // A notification left from before unpairing: its answer must not wait for the next pairing.
            assertFailsWith<NotPairedException> { InteractionResponder(engine, starter) { PairingState.NotPaired }.respond("int_1", "allow") }
            assertTrue(store.state.value.outbox.isEmpty())
            assertTrue(starter.starts.isEmpty())
        } finally {
            scope.cancel()
        }
    }
}

/**
 * The real receiver in the app process (Robolectric): the broadcast of a notification button
 * ends as an entry in the Room outbox, and the connection service is started.
 */
@RunWith(AndroidJUnit4::class)
@org.robolectric.annotation.Config(application = dev.aas.android.ui.TestAasApplication::class)
class InteractionActionReceiverTest {
    @Test
    fun theDenyButtonQueuesTheAnswerAndStartsTheService() = runBlocking<Unit> {
        val app = ApplicationProvider.getApplicationContext<Application>()
        val container = app.appContainer
        container.credentialStore.save(PairingInfo("ws://127.0.0.1:9/v1/ws", "home pc", "dev_1", "Pixel", 1), "token")
        val intent = InteractionActionReceiver.intent(app, interactionId = "int_7", threadId = "thr_1", optionId = "deny")

        app.sendBroadcast(intent)
        shadowOf(Looper.getMainLooper()).idle()

        val entry = eventually(what = "the outbox entry") { container.engine.outbox.value.firstOrNull() }
        assertEquals(Methods.InteractionRespond.name, entry.method)
        val params = AasJson.decodeFromJsonElement(InteractionRespondParams.serializer(), entry.params)
        assertEquals("int_7", params.interactionId)
        assertEquals(InteractionResolution.Approval("deny"), params.resolution)
        // Durable: it is in the database, not only in memory.
        val stored = container.syncStore.transaction { it.outbox() }
        assertEquals(listOf(entry.clientRequestId), stored.map { it.clientRequestId })

        val started = eventually(what = "the service start") { shadowOf(app).nextStartedService }
        assertEquals(ConnectionService::class.java.name, started.component?.className)
        assertEquals(ConnectionService.ACTION_START, started.action)
    }

    @Test
    fun anActionWithoutItsExtrasIsIgnored() = runBlocking<Unit> {
        val app = ApplicationProvider.getApplicationContext<Application>()
        app.sendBroadcast(Intent(app, InteractionActionReceiver::class.java).setAction(InteractionActionReceiver.ACTION_RESPOND))
        shadowOf(Looper.getMainLooper()).idle()
        val log = eventually(what = "the log line") {
            app.appContainer.connectionLog.entries.value.firstOrNull { it.message.startsWith("notification action without") }
        }
        assertNotNull(log)
        assertEquals(emptyList(), app.appContainer.syncStore.transaction { it.outbox() })
    }
}
