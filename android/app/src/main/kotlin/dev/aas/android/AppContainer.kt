package dev.aas.android

import android.app.Application
import android.content.Context
import android.security.NetworkSecurityPolicy
import androidx.datastore.core.DataStore
import androidx.datastore.preferences.core.PreferenceDataStoreFactory
import androidx.datastore.preferences.core.Preferences
import androidx.datastore.preferences.preferencesDataStoreFile
import dev.aas.android.data.BlobCache
import dev.aas.android.data.BlobImages
import dev.aas.android.data.BlobRepository
import dev.aas.android.data.BlobUploader
import dev.aas.android.data.ComposerDrafts
import dev.aas.android.data.HarnessRepository
import dev.aas.android.data.InteractionRepository
import dev.aas.android.data.ProjectRepository
import dev.aas.android.data.Reads
import dev.aas.android.data.SentDrafts
import dev.aas.android.data.ServerLists
import dev.aas.android.data.ServerRepository
import dev.aas.android.data.ThreadRepository
import dev.aas.android.data.WorkspaceRepository
import dev.aas.android.data.db.AasDatabase
import dev.aas.android.data.db.RoomSyncStore
import dev.aas.android.diagnostics.ConnectionLog
import dev.aas.android.net.HttpClients
import dev.aas.android.notify.AppVisibility
import dev.aas.android.notify.InteractionResponder
import dev.aas.android.notify.Notifier
import dev.aas.android.pairing.PairingParser
import dev.aas.android.pairing.PairingRepository
import dev.aas.android.protocol.ClientInfo
import dev.aas.android.security.AndroidKeystoreKeyProvider
import dev.aas.android.security.CredentialStore
import dev.aas.android.security.PairingState
import dev.aas.android.security.SecretKeyProvider
import dev.aas.android.security.TokenCipher
import dev.aas.android.security.hasPairing
import dev.aas.android.security.info
import dev.aas.android.service.ConnectionController
import dev.aas.android.service.StartReason
import dev.aas.android.settings.SettingsRepository
import dev.aas.android.sync.AasHttp
import dev.aas.android.sync.Clock
import dev.aas.android.sync.SyncConfig
import dev.aas.android.sync.SyncEngine
import dev.aas.android.sync.SyncLogger
import dev.aas.android.sync.SyncStore
import dev.aas.android.ui.common.UserMessages
import dev.aas.android.ui.composer.CaptureFiles
import kotlinx.coroutines.CoroutineExceptionHandler
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.SupervisorJob
import kotlinx.coroutines.cancel
import kotlinx.coroutines.flow.SharingStarted
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.flow.distinctUntilChanged
import kotlinx.coroutines.flow.map
import kotlinx.coroutines.flow.stateIn
import kotlinx.coroutines.launch
import okhttp3.OkHttpClient
import java.io.File
import java.util.concurrent.TimeUnit

/**
 * The app's object graph (manual dependency injection; one instance per process, owned by
 * [AasApplication]). Everything is created lazily on first use.
 *
 * View models get their dependencies from here through `aasViewModel { container, handle -> … }`
 * (ui/common/ViewModels.kt); services and receivers through [Context.appContainer].
 */
class AppContainer(
    private val app: Application,
    val policy: AppPolicy = AppPolicy(),
    private val syncConfig: SyncConfig = SyncConfig(),
    /** The token key's source: the Android Keystore; tests substitute a software key. */
    keyProviderFactory: () -> SecretKeyProvider = { AndroidKeystoreKeyProvider() },
) {
    /** elapsedRealtime for durations: it counts deep sleep, which the engine's watchdog must see (AndroidClock). */
    val clock: Clock = AndroidClock

    val connectionLog = ConnectionLog(policy.connectionLogCapacity, clock)

    /**
     * Lives as long as the process. A failure in one of its jobs is logged, never silently lost,
     * and does not cancel the others.
     */
    val applicationScope: CoroutineScope = CoroutineScope(
        SupervisorJob() + Dispatchers.Default + CoroutineExceptionHandler { _, e ->
            connectionLog.add(SyncLogger.Level.Error, "app", "uncaught failure in a background job", e)
        },
    )

    private val dataStoreScope = CoroutineScope(SupervisorJob() + Dispatchers.IO)

    private val databaseLazy = lazy { AasDatabase.open(app) }
    val database: AasDatabase by databaseLazy

    val syncStore: SyncStore by lazy { RoomSyncStore(database) }

    val httpClient: OkHttpClient by lazy { HttpClients.base(policy) }

    /** Blob uploads and downloads. */
    val aasHttp: AasHttp by lazy { AasHttp(httpClient) }

    /** `POST /v1/pair`, bounded as a whole by [AppPolicy.pairCallTimeoutMs]. */
    private val pairingHttp: AasHttp by lazy {
        AasHttp(httpClient.newBuilder().callTimeout(policy.pairCallTimeoutMs, TimeUnit.MILLISECONDS).build())
    }

    val engine: SyncEngine by lazy {
        SyncEngine(
            store = syncStore,
            http = httpClient,
            scope = applicationScope,
            clientInfo = ClientInfo(CLIENT_NAME, BuildConfig.VERSION_NAME, CLIENT_PLATFORM),
            config = syncConfig,
            clock = clock,
            logger = connectionLog.syncLogger,
        )
    }

    val keyProvider: SecretKeyProvider by lazy(keyProviderFactory)

    val credentialStore: CredentialStore by lazy {
        CredentialStore(
            dataStore(CREDENTIALS_FILE),
            TokenCipher(keyProvider),
            Dispatchers.IO,
            applicationScope,
            policy.keystoreRetryInitialMs,
            policy.keystoreRetryMaxMs,
            clock,
        )
    }

    val settings: SettingsRepository by lazy { SettingsRepository(dataStore(SETTINGS_FILE)) }

    val visibility = AppVisibility { threadId -> notifier.threadShown(threadId) }

    val userMessages = UserMessages()

    val connectionController = ConnectionController(app, connectionLog, clock)

    val notifier: Notifier by lazy {
        Notifier(app, settings, visibility, connectionLog, { engine.workspace.value }, policy, { pairingState.value?.hasPairing == true }, clock)
    }

    val interactionResponder: InteractionResponder by lazy { InteractionResponder(engine, connectionController) { credentialStore.current() } }

    /** `ws://` is allowed exactly where the platform's network security config allows cleartext. */
    val pairingParser = PairingParser { host -> NetworkSecurityPolicy.getInstance().isCleartextTrafficPermitted(host) }

    val pairingRepository: PairingRepository by lazy {
        PairingRepository(pairingHttp, credentialStore, keyProvider, engine, syncStore, connectionController, policy, connectionLog, { notifier.clearAll() }, clock)
    }

    val workspaceRepository: WorkspaceRepository by lazy { WorkspaceRepository(engine) }

    /** Read-only calls of the repositories, sent again after a reconnect (data/Reads.kt). */
    val reads: Reads by lazy { Reads(engine, policy.readReconnectWaitMs) }

    /** Server lists made unique by the ids the screens key them by (data/ServerLists.kt). */
    val serverLists: ServerLists by lazy { ServerLists(connectionLog.logger(ConnectionLog.SOURCE_DATA)) }

    val threadRepository: ThreadRepository by lazy { ThreadRepository(engine, reads, serverLists) }

    val interactionRepository: InteractionRepository by lazy { InteractionRepository(engine) }

    val serverRepository: ServerRepository by lazy { ServerRepository(engine, reads, serverLists) }

    val projectRepository: ProjectRepository by lazy { ProjectRepository(engine, reads, serverLists) }

    val harnessRepository: HarnessRepository by lazy { HarnessRepository(engine) }

    /** Unsent composer content per thread (process lifetime). */
    val composerDrafts = ComposerDrafts()

    /** Sent messages until the daemon's answer: a refused one comes back to its composer. */
    val sentDrafts: SentDrafts by lazy { SentDrafts(applicationScope, composerDrafts) }

    /** Downloaded blobs on disk (immutable, content-addressed; readable offline once seen). */
    val blobCache: BlobCache by lazy { BlobCache(File(app.cacheDir, BLOB_CACHE_DIR), policy.blobDiskCacheBytes, Dispatchers.IO) }

    val blobRepository: BlobRepository by lazy {
        BlobRepository(
            http = aasHttp,
            credentials = { (credentialStore.current() as? PairingState.Paired)?.pairing?.credentials },
            cache = blobCache,
            onCacheFailure = { id, e -> connectionLog.warn(SOURCE_BLOBS, "could not cache blob $id", e) },
        )
    }

    /**
     * Images of blobs (attachments), decoded and kept in memory. Any image the daemon stored is
     * within its `maxBlobBytes`, which is therefore the download limit.
     */
    val blobImages: BlobImages by lazy {
        BlobImages(
            blobs = blobRepository,
            memoryBytes = policy.imageMemoryCacheBytes,
            maxImageBytes = { engine.status.value.policy?.maxBlobBytes ?: policy.imageUpload.fallbackMaxBlobBytes },
            decodeDispatcher = Dispatchers.Default,
        )
    }

    val blobUploader: BlobUploader by lazy {
        BlobUploader(app.contentResolver, aasHttp, credentialStore, engine, policy.imageUpload, Dispatchers.IO)
    }

    /**
     * The stored pairing (null until read from disk): the credential store's one decoded view,
     * which every consumer uses (engine credentials, service, boot receiver, screens).
     */
    val pairingState: StateFlow<PairingState?> get() = credentialStore.state

    /** The paired server's name for notifications and the status bar. */
    val pairedServerName: StateFlow<String?> by lazy {
        pairingState.map { it?.info?.serverName }.stateIn(applicationScope, SharingStarted.Eagerly, null)
    }

    /** Wires the long-lived flows; called once from [AasApplication.onCreate]. */
    fun start() {
        // The engine always uses the stored credentials (none when not paired, unreadable, or
        // while the keystore cannot decrypt them). When the pairing is lost, its notifications
        // go too (unpairing also clears them itself, before the service stops); a token found
        // unreadable at start takes along what an earlier process posted.
        applicationScope.launch {
            var previous: PairingState? = null
            pairingState.collect { state ->
                if (state == null) return@collect
                engine.setCredentials((state as? PairingState.Paired)?.pairing?.credentials)
                val lost = !state.hasPairing && (previous?.hasPairing == true || (previous == null && state is PairingState.Unreadable))
                if (lost) notifier.clearAll()
                previous = state
            }
        }
        // A thread that became read keeps no finished-turn and error notifications (a shown
        // thread: see AppVisibility).
        applicationScope.launch {
            var previous: Set<String>? = null
            engine.workspace
                .map { ws -> if (ws.synced) ws.threads.filterNot { it.unread }.map { it.thread.id }.toSet() else null }
                .distinctUntilChanged()
                .collect { read ->
                    if (read == null) return@collect
                    notifier.threadsRead(read, previous)
                    previous = read
                }
        }
        // Photos taken for drafts of an earlier process are not needed any more (drafts do not
        // outlive the process).
        applicationScope.launch(Dispatchers.IO) {
            CaptureFiles.clear(app).forEach { connectionLog.warn(SOURCE_BLOBS, "could not delete the old capture ${it.name}") }
        }
        // Connection state changes go to the diagnostics log.
        applicationScope.launch {
            engine.status.map { it.connection }.distinctUntilChanged().collect {
                connectionLog.add(SyncLogger.Level.Debug, "connection", it.toString())
            }
        }
    }

    /** The app came to the foreground: make sure the connection runs and skip any backoff. */
    fun onAppForeground() {
        visibility.setAppInForeground(true)
        credentialStore.retryNow()
        applicationScope.launch {
            if (credentialStore.current().hasPairing) {
                connectionController.requestStart(StartReason.AppForeground)
                engine.onAppForeground()
            }
        }
    }

    /** The user asked to try the keystore again (it failed to decrypt the token). */
    fun retryCredentials() {
        credentialStore.retryNow()
    }

    fun onAppBackground() {
        visibility.setAppInForeground(false)
    }

    /** The user pressed 再接続: skip the wait (also after "connected elsewhere"). */
    fun reconnectNow() {
        engine.reconnectNow()
        credentialStore.retryNow()
        applicationScope.launch {
            if (credentialStore.current().hasPairing) connectionController.requestStart(StartReason.UserAction)
        }
    }

    /** Releases the process-wide resources (only called by test runners: `Application.onTerminate`). */
    fun close() {
        applicationScope.cancel()
        dataStoreScope.cancel()
        if (databaseLazy.isInitialized()) database.close()
    }

    private fun dataStore(name: String): DataStore<Preferences> =
        PreferenceDataStoreFactory.create(scope = dataStoreScope, produceFile = { app.preferencesDataStoreFile(name) })

    companion object {
        const val CLIENT_NAME = "aas-android"
        const val CLIENT_PLATFORM = "android"
        const val CREDENTIALS_FILE = "credentials"
        const val SETTINGS_FILE = "settings"
        const val BLOB_CACHE_DIR = "blobs"
        const val SOURCE_BLOBS = "blobs"
    }
}

/** The process's [AppContainer]. */
val Context.appContainer: AppContainer get() = (applicationContext as AasApplication).container
