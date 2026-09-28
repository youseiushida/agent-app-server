package dev.aas.android.sync

import dev.aas.android.protocol.AasJson
import dev.aas.android.protocol.BackgroundTask
import dev.aas.android.protocol.BackgroundTaskId
import dev.aas.android.protocol.ClientInfo
import dev.aas.android.protocol.CommandAction
import dev.aas.android.protocol.Empty
import dev.aas.android.protocol.ErrorKind
import dev.aas.android.protocol.Harness
import dev.aas.android.protocol.HarnessRefreshParams
import dev.aas.android.protocol.InitializeParams
import dev.aas.android.protocol.InitializeResult
import dev.aas.android.protocol.JsonKeys
import dev.aas.android.protocol.Methods
import dev.aas.android.protocol.PROTOCOL_VERSION
import dev.aas.android.protocol.RpcError
import dev.aas.android.protocol.RpcException
import dev.aas.android.protocol.RpcMessage
import dev.aas.android.protocol.RpcMethod
import dev.aas.android.protocol.ServerNotification
import dev.aas.android.protocol.StreamBatch
import dev.aas.android.protocol.SubscribeParams
import dev.aas.android.protocol.SubscribeResult
import dev.aas.android.protocol.Subscription
import dev.aas.android.protocol.SubscriptionState
import dev.aas.android.protocol.ThreadId
import dev.aas.android.protocol.ThreadReadParams
import dev.aas.android.protocol.UnsubscribeParams
import dev.aas.android.protocol.WORKSPACE_STREAM
import dev.aas.android.protocol.threadIdOfStream
import dev.aas.android.protocol.threadStream
import kotlinx.coroutines.CancellationException
import kotlinx.coroutines.CompletableDeferred
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Job
import kotlinx.coroutines.NonCancellable
import kotlinx.coroutines.cancelChildren
import kotlinx.coroutines.channels.Channel
import kotlinx.coroutines.coroutineScope
import kotlinx.coroutines.delay
import kotlinx.coroutines.flow.MutableSharedFlow
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.SharedFlow
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.flow.asSharedFlow
import kotlinx.coroutines.flow.asStateFlow
import kotlinx.coroutines.flow.first
import kotlinx.coroutines.flow.mapNotNull
import kotlinx.coroutines.flow.update
import kotlinx.coroutines.launch
import kotlinx.coroutines.sync.Mutex
import kotlinx.coroutines.sync.withLock
import kotlinx.coroutines.withContext
import kotlinx.coroutines.withTimeoutOrNull
import kotlinx.serialization.SerializationException
import kotlinx.serialization.json.JsonElement
import kotlinx.serialization.json.JsonObject
import kotlinx.serialization.json.JsonPrimitive
import kotlinx.serialization.json.contentOrNull
import okhttp3.OkHttpClient
import okhttp3.Request
import okio.utf8Size
import java.util.UUID
import java.util.concurrent.ConcurrentHashMap
import java.util.concurrent.TimeUnit
import kotlin.random.Random

/**
 * The client side of the reliability contract (protocol.md §7), and the data source of the app.
 *
 * ## What it does
 * 1. **Apply + cursor in one transaction.** Every `stream/batch` is applied to the [SyncStore]
 *    together with the stream's cursor; events with `seq <= cursor` are skipped, so replays
 *    never apply twice. A merged delta (`seqFrom..seq`) is applied as one event; one that
 *    overlaps the cursor cannot be split, so the thread is read again. A batch without events
 *    moves the cursor to its `head` (the events up to it were removed by retention).
 * 2. **Initial sync.** `initialize`; on the first run or `epochChanged`: wipe (the outbox
 *    stays), `workspace/snapshot`, subscribe from its head. Otherwise the workspace and every
 *    open thread are resubscribed from their stored cursors. Open threads without a cursor
 *    are loaded with `thread/read` and subscribed after its head. A thread that cannot be
 *    loaded fails alone ([ThreadSync.Failed]); only the workspace and the connection itself
 *    can fail the setup.
 * 3. **Outbox.** Mutations are committed to the store before their frame is sent, sent after
 *    the setup of each connection in the order they were made (one at a time per thread,
 *    project or interaction; others in parallel), and always resent with the same
 *    `clientRequestId`. They leave the outbox on a result, a definitive error, or when the
 *    user discards them ([discardOutbox]); any other error is retried after
 *    [SyncConfig.outboxRetryDelayMs] — except `harnessUnavailable`: such an entry waits,
 *    showing the server's reason, until the synced workspace lists its harness as available
 *    (`harness/updated`), and is then sent at once ([OutboxEntry.waitingForHarness]).
 *    `turn/interrupt` and `thread/stop` do not wait behind an entry of their thread that only
 *    waits for its retry.
 * 4. **Liveness.** A watchdog closes the socket when no frame arrived within the server's
 *    `clientTimeoutMs`, measured with [Clock.monotonicMs] (which counts deep sleep). Its timer
 *    does not count deep sleep, so [onAppForeground] and [onNetworkAvailable] measure again at
 *    once and replace a connection that went silent while the phone slept. A stream whose
 *    heartbeat head stays ahead of its cursor for `clientTimeoutMs` without progress is
 *    resubscribed from the cursor.
 * 5. **Reconnection.** Full-jitter backoff capped at [SyncConfig.backoffCapMs]; the attempt
 *    counter resets only once `initialize` and the resubscription completed. [reconnectNow],
 *    [onAppForeground] and [onNetworkAvailable]/[onNetworkChanged] skip the wait. Close codes
 *    follow protocol.md §2.3: 4001 and HTTP 401/403 stop until new credentials, 4000 stops
 *    until a user action or the app returning to the foreground, 4003 waits the maximum
 *    backoff, and 1009 fails the request that was too large instead of resending it.
 * 6. **One socket at a time.** The next socket is opened only after the previous one was
 *    dropped and OkHttp reported its end, and every socket's reports are read only by the
 *    session that opened it: a close code sent to an old socket (the server replacing a
 *    connection this client already gave up, e.g. after a network change) never reaches the
 *    engine. A 4000 on the current socket is a genuine second holder of this device's token.
 *
 * ## Threads of execution
 * Everything runs in [scope]. Frames of one connection are processed in arrival order on one
 * coroutine; it never waits for a response, so requests of the setup, of the outbox and of the
 * UI run on their own coroutines.
 */
class SyncEngine(
    private val store: SyncStore,
    http: OkHttpClient,
    private val scope: CoroutineScope,
    private val clientInfo: ClientInfo,
    private val config: SyncConfig = SyncConfig(),
    private val clock: Clock = Clock.System,
    random: Random = Random.Default,
    private val newRequestId: () -> String = { UUID.randomUUID().toString() },
    private val logger: SyncLogger = SyncLogger.None,
    private val wireTap: WireTap? = null,
) {
    private val wsHttp: OkHttpClient = http.newBuilder()
        .connectTimeout(config.connectTimeoutMs, TimeUnit.MILLISECONDS)
        // Silence and stalled writes are detected by the watchdog (protocol.md §2.2), not by
        // socket timeouts that know nothing about heartbeats.
        .readTimeout(0, TimeUnit.MILLISECONDS)
        .writeTimeout(0, TimeUnit.MILLISECONDS)
        .pingInterval(0, TimeUnit.MILLISECONDS)
        .build()
    private val backoff = Backoff(config.backoffBaseMs, config.backoffCapMs, random)
    private val views = Views()

    /** Serializes store writes with the view updates that mirror them (commit order). */
    private val writeLock = Mutex()

    /**
     * Orders subscription changes: connection setup, loading, unsubscribing and reloading
     * threads. Held across network calls, so [openThread] and [closeThread] never wait for it.
     */
    private val followLock = Mutex()

    /** Guards [openCounts] together with registering or dropping a thread's view (local work only). */
    private val openLock = Mutex()

    /** Open threads with the number of times each was opened. */
    private val openCounts = ConcurrentHashMap<ThreadId, Int>()

    /** Outbox entries on the wire, and entries being discarded (see [discardOutbox]). */
    private val outboxGuard = OutboxGuard()
    private val waiters = ConcurrentHashMap<String, CompletableDeferred<JsonElement>>()
    private val triggers = Channel<Trigger>(Channel.UNLIMITED)
    private val outboxKick = Channel<Unit>(Channel.CONFLATED)

    @Volatile
    private var credentials: Credentials? = null

    @Volatile
    private var suspendReason: SuspendReason? = null

    @Volatile
    private var networkAvailable = true

    @Volatile
    private var session: Session? = null

    @Volatile
    private var everEstablished = false
    private var loopJob: Job? = null

    private val _status = MutableStateFlow(SyncStatus())

    /** Connection and synchronisation status. */
    val status: StateFlow<SyncStatus> = _status.asStateFlow()

    /** The synced workspace: harnesses, projects, threads (with unread), pending interactions, operations. */
    val workspace: StateFlow<WorkspaceState> = views.workspace

    /** Requests not answered definitively yet, oldest first. */
    val outbox: StateFlow<List<OutboxEntry>> = views.outbox

    private val _signals = MutableSharedFlow<SyncSignal>(extraBufferCapacity = EVENT_BUFFER)

    /**
     * New pending interactions, finished turns, finished operations, removed threads — for
     * notifications. Emitted after the change is committed. Hot: collect it for as long as
     * notifications matter (e.g. in the foreground service); the buffer holds [EVENT_BUFFER]
     * signals for a slow collector, beyond that they are counted in [SyncStatus.droppedSignals].
     */
    val signals: SharedFlow<SyncSignal> = _signals.asSharedFlow()

    private val _results = MutableSharedFlow<OutboxResult>(extraBufferCapacity = EVENT_BUFFER)

    /** Harness ids with a `harness/refresh` of this client in flight, and how many for each. */
    private val refreshCounts = HashMap<String, Int>()
    private val _refreshingHarnesses = MutableStateFlow<Set<String>>(emptySet())

    /**
     * Harnesses this client is probing right now ([refreshHarnesses] in flight): the UI shows
     * them as "確認中". The protocol has no probing state of its own; this is only this client's
     * own request.
     */
    val refreshingHarnesses: StateFlow<Set<String>> = _refreshingHarnesses.asStateFlow()

    /**
     * Final outcomes of outbox entries (also of those whose caller is gone, e.g. after the app
     * restarted), for showing failures of requests made with [enqueue].
     */
    val results: SharedFlow<OutboxResult> = _results.asSharedFlow()

    // ----- lifecycle ------------------------------------------------------------------------------

    /** Loads the stored state into the flows and starts connecting (idempotent). */
    fun start() {
        synchronized(this) {
            if (loopJob?.isActive == true) return
            val job = scope.launch {
                try {
                    writeLock.withLock { store.transaction { views.loadWorkspace(it) } }
                } catch (e: CancellationException) {
                    throw e
                } catch (e: Exception) {
                    // Without its store the engine cannot work: it stays stopped and says why.
                    reportError("loading the local store failed: ${e.message ?: e.javaClass.simpleName}", e)
                    return@launch
                }
                _status.update {
                    it.copy(lastSyncAtMs = views.lastSyncAtMs, pendingOutbox = views.outbox.value.size, cursors = views.cursors.toMap())
                }
                runLoop()
            }
            job.invokeOnCompletion { _status.update { it.copy(connection = ConnectionState.Stopped) } }
            loopJob = job
        }
    }

    /**
     * Closes the connection and stops reconnecting; the store, the outbox and open threads stay
     * (a later [start] resumes). Returns the job that completes once everything stopped.
     */
    fun stop(): Job {
        val job = synchronized(this) { loopJob.also { loopJob = null } } ?: return Job().apply { complete() }
        job.cancel()
        return job
    }

    /** Sets (or clears) the credentials. A different value replaces the current connection. */
    fun setCredentials(value: Credentials?) {
        if (value == credentials) return
        credentials = value
        suspendReason = null
        session?.drop(SessionEnd.Retry(DisconnectCause.Reset, RetryDelay.Immediate))
        trigger(TriggerKind.CredentialsChanged)
    }

    /**
     * An explicit user action ("reconnect"): skips any backoff wait (also the long one after a
     * protocol violation) and resumes after "connected elsewhere" or an incompatible server.
     */
    fun reconnectNow() {
        val reason = suspendReason
        if (reason is SuspendReason.Replaced || reason is SuspendReason.Incompatible) suspendReason = null
        trigger(TriggerKind.UserAction)
    }

    /**
     * The app came to the foreground: checks the connection's liveness now ([checkLiveness]: the
     * phone may have slept), skips the backoff wait and resumes after "connected elsewhere".
     */
    fun onAppForeground() {
        if (suspendReason is SuspendReason.Replaced) suspendReason = null
        checkLiveness("the app came to the foreground")
        trigger(TriggerKind.AppForeground)
    }

    /**
     * A network is available (again): checks the liveness of a connection that is still open
     * ([checkLiveness]), leaves [ConnectionState.Offline] and skips the backoff wait.
     */
    fun onNetworkAvailable() {
        networkAvailable = true
        checkLiveness("a network became available")
        trigger(TriggerKind.NetworkAvailable)
    }

    /**
     * The default network changed: a socket bound to the old network would only fail after the
     * watchdog timeout, so it is dropped and a new connection is made at once.
     */
    fun onNetworkChanged() {
        networkAvailable = true
        session?.drop(SessionEnd.Retry(DisconnectCause.NetworkChanged, RetryDelay.Immediate))
        trigger(TriggerKind.NetworkAvailable)
    }

    /** No network: closes the connection and makes no attempts until [onNetworkAvailable]. */
    fun onNetworkLost() {
        networkAvailable = false
        session?.drop(SessionEnd.Offline)
        trigger(TriggerKind.NetworkLost)
    }

    /** Suspends until a session is established. */
    suspend fun awaitOnline() {
        status.first { it.isOnline }
    }

    // ----- reading ------------------------------------------------------------------------------

    /**
     * A read-only call on the current session.
     *
     * @throws NotConnectedException no session is established (see [awaitOnline]).
     * @throws RpcException the server answered with an error, or the request exceeds the
     *   server's `maxClientFrameBytes` (`payloadTooLarge`, not sent).
     * @throws ConnectionLostException the connection ended before the answer.
     */
    suspend fun <P, R> query(method: RpcMethod<P, R>, params: P): R {
        require(!method.mutating) { "${method.name} changes state: use enqueue() or mutate()" }
        val result = queryRaw(method.name, AasJson.encodeToJsonElement(method.params, params))
        return AasJson.decodeFromJsonElement(method.result, result)
    }

    /** Like [query] for a method named at runtime (e.g. a command action this client does not know). */
    suspend fun queryRaw(method: String, params: JsonElement): JsonElement {
        val s = session?.takeIf { it.established } ?: throw NotConnectedException()
        return s.rpc.call(method, params, enforceFrameLimit = true)
    }

    /**
     * Like [query], but a call whose connection ends before the answer ([ConnectionLostException]:
     * the watchdog closed a silent socket, the network changed, the daemon restarted) is sent once
     * more, on the next session established within [reconnectWaitMs]. Only read-only methods get
     * here ([query]'s rule), and they have no effect on the server, so sending one again is safe.
     *
     * "The next session" is any established session other than the one the call went on, however
     * late the caller notices the loss: its continuation may run only after the engine is back
     * online (a busy main thread), and then the session already there is the one to use. The
     * connection state cannot tell this: two sessions' [ConnectionState.Online] may look alike,
     * and a session is usable before the status says [ConnectionState.Online].
     *
     * @throws NotConnectedException no session is established now, or none within
     *   [reconnectWaitMs] after the loss.
     * @throws ConnectionLostException the second connection ended before the answer as well.
     * @throws RpcException as [query].
     */
    suspend fun <P, R> queryAcrossReconnect(method: RpcMethod<P, R>, params: P, reconnectWaitMs: Long): R {
        require(!method.mutating) { "${method.name} changes state: use enqueue() or mutate()" }
        val json = AasJson.encodeToJsonElement(method.params, params)
        val first = session?.takeIf { it.established } ?: throw NotConnectedException()
        val result = try {
            first.rpc.call(method.name, json, enforceFrameLimit = true)
        } catch (e: ConnectionLostException) {
            val next = withTimeoutOrNull(reconnectWaitMs) { establishedSessionOtherThan(first) } ?: throw NotConnectedException()
            next.rpc.call(method.name, json, enforceFrameLimit = true)
        }
        return AasJson.decodeFromJsonElement(method.result, result)
    }

    /**
     * Suspends until an established session other than [previous] exists. The status changes
     * after every session became established (it turns [ConnectionState.Online]), so each
     * change is a moment to look; a session that ended meanwhile is no longer [session].
     */
    private suspend fun establishedSessionOtherThan(previous: Session): Session =
        status.mapNotNull { session?.takeIf { it !== previous && it.established } }.first()

    /**
     * `harness/refresh`: the server probes [harnessId] (every harness when `null`) again, e.g.
     * after the user logged in to the CLI on the PC. While it runs, the harness is in
     * [refreshingHarnesses]. The synced workspace follows the server's `harness/updated`
     * events (the result is not written here, so the stream stays the one source of order);
     * outbox entries waiting for a harness the result reports available are sent at once.
     *
     * @throws NotConnectedException no session is established.
     * @throws RpcException the server refused (e.g. `notFound` for an unknown harness).
     */
    suspend fun refreshHarnesses(harnessId: String? = null): List<Harness> {
        val ids = harnessId?.let { setOf(it) } ?: views.workspace.value.harnesses.map { it.id }.toSet()
        synchronized(refreshCounts) {
            ids.forEach { refreshCounts[it] = (refreshCounts[it] ?: 0) + 1 }
            _refreshingHarnesses.value = refreshCounts.keys.toSet()
        }
        try {
            val harnesses = query(Methods.HarnessRefresh, HarnessRefreshParams(harnessId)).harnesses
            releaseHarnessWaits(harnesses.filter { it.available }.map { it.id }.toSet())
            return harnesses
        } finally {
            synchronized(refreshCounts) {
                ids.forEach { id ->
                    val left = (refreshCounts[id] ?: 1) - 1
                    if (left > 0) refreshCounts[id] = left else refreshCounts.remove(id)
                }
                _refreshingHarnesses.value = refreshCounts.keys.toSet()
            }
        }
    }

    /**
     * Starts following a thread and returns its state. The stored content is shown at once
     * (this never waits for the network, not even for a connection setup in progress); when
     * online, the latest page is then read (`thread/read`) and the stream subscribed after its
     * head (protocol.md §2, step 4). Calls are counted: the thread stays open until
     * [closeThread] was called as often.
     */
    suspend fun openThread(threadId: ThreadId): StateFlow<ThreadState> {
        val (flow, first) = openLock.withLock {
            val flow = writeLock.withLock {
                views.thread(threadId) ?: store.transaction { views.openThread(it, threadId) }
            }
            val count = (openCounts[threadId] ?: 0) + 1
            openCounts[threadId] = count
            flow to (count == 1)
        }
        // Read after the count is registered: a session whose setup reads the open threads later
        // loads this one itself; for one that read them earlier, [follow] does it.
        if (first) session?.let { follow(it, threadId) }
        return flow
    }

    /**
     * Loads and subscribes a thread opened while [s] exists, once its setup is done (the setup
     * holds [followLock] until it is established). Nothing to do when the setup already loaded
     * it after this opening ([closeThread] takes a thread out of [Session.liveThreads], so a
     * subscription from before a close is never trusted).
     */
    private fun follow(s: Session, threadId: ThreadId) {
        s.scope.launch {
            followLock.withLock {
                if (session !== s || !s.established || !openCounts.containsKey(threadId)) return@withLock
                if (threadId in s.liveThreads) {
                    // The setup of this session loaded it after this opening.
                    writeLock.withLock { views.setSync(threadId, ThreadSync.Live) }
                } else {
                    guarded(s) { loadThread(s, threadId) }
                }
            }
        }
    }

    /**
     * Loads an open thread again, e.g. after [ThreadSync.Failed]. Without an established
     * session it does nothing: the next connection loads every open thread anyway.
     */
    fun retryThread(threadId: ThreadId) {
        val s = session?.takeIf { it.established } ?: return
        if (!openCounts.containsKey(threadId)) return
        s.scope.launch {
            followLock.withLock {
                if (session === s && openCounts.containsKey(threadId)) guarded(s) { loadThread(s, threadId) }
            }
        }
    }

    /** The state of an open thread, or `null` when it is not open. */
    fun thread(threadId: ThreadId): StateFlow<ThreadState>? = views.thread(threadId)

    /** Stops following a thread (after as many calls as [openThread]); its stored content stays. */
    suspend fun closeThread(threadId: ThreadId) {
        val stream = threadStream(threadId)
        val (s, wasLive) = openLock.withLock {
            val count = openCounts[threadId] ?: return
            if (count > 1) {
                openCounts[threadId] = count - 1
                return
            }
            openCounts.remove(threadId)
            writeLock.withLock { views.closeThread(threadId) }
            val s = session ?: return
            // Batches of an unfollowed stream are ignored, so its cursor falls behind: reopening
            // must read it again rather than trust this subscription ([follow]), and until then
            // what it delivers has no base.
            val wasLive = s.liveThreads.remove(threadId)
            if (wasLive) s.suspendedStreams += stream
            s to wasLive
        }
        s.scope.launch {
            // Under followLock: a load of this thread in progress finishes first (its subscription
            // is then undone), and a reopening load waits until the unsubscription is through.
            followLock.withLock {
                if (session !== s || openCounts.containsKey(threadId)) return@withLock
                val subscribed = s.liveThreads.remove(threadId) || wasLive
                if (!subscribed) return@withLock
                try {
                    s.rpc.call(Methods.Unsubscribe.name, encode(Methods.Unsubscribe, UnsubscribeParams(listOf(threadStream(threadId)))), true)
                } catch (e: CancellationException) {
                    throw e
                } catch (e: ConnectionLostException) {
                    // The next connection does not subscribe it anyway.
                } catch (e: Exception) {
                    // Batches of an unfollowed stream are ignored; the stale subscription only costs traffic.
                    reportError("unsubscribing thread $threadId failed: ${e.message}")
                }
            }
        }
    }

    /**
     * Loads the page of turns before the oldest loaded one. Returns whether even older turns
     * exist. Requires a session (see [query]).
     */
    suspend fun loadOlder(threadId: ThreadId): Boolean {
        val oldest = views.thread(threadId)?.value?.turns?.minOfOrNull { it.index }
            ?: throw IllegalStateException("thread $threadId is not open or has no turns")
        val read = query(Methods.ThreadRead, ThreadReadParams(threadId, beforeTurnIndex = oldest, limitTurns = config.threadPageTurns))
        write { tx, signals -> EventApplier.applyOlderPage(tx, read, signals) }
        return read.hasMoreBefore
    }

    /**
     * The stored background tasks among [ids] (tasks of threads followed on this device; a task of
     * a thread never opened here is not stored). For showing which task asked an approval outside
     * its thread's screen.
     */
    suspend fun storedBackgroundTasks(ids: Collection<BackgroundTaskId>): Map<BackgroundTaskId, BackgroundTask> {
        if (ids.isEmpty()) return emptyMap()
        return store.transaction { tx -> ids.toSet().mapNotNull { id -> tx.backgroundTask(id)?.let { id to it } }.toMap() }
    }

    /** Marks a thread as read up to its current summary. */
    suspend fun markViewed(threadId: ThreadId) {
        write { tx, _ ->
            val head = tx.thread(threadId)?.head ?: 0L
            val before = tx.viewState(threadId) ?: ThreadViewState()
            tx.setViewState(threadId, ThreadViewState(lastViewedHead = maxOf(head, before.lastViewedHead), markedUnread = false))
        }
    }

    /** Marks a thread as unread (until [markViewed]). */
    suspend fun markUnread(threadId: ThreadId) {
        write { tx, _ ->
            val before = tx.viewState(threadId) ?: ThreadViewState()
            tx.setViewState(threadId, before.copy(markedUnread = true))
        }
    }

    // ----- changing state (outbox) ---------------------------------------------------------------

    /**
     * Commits a mutating request to the outbox and returns its `clientRequestId` without
     * waiting. [build] receives the id and must put it in the params. The outcome arrives on
     * [results] (and the entry leaves [outbox]).
     */
    suspend fun <P, R> enqueue(method: RpcMethod<P, R>, build: (clientRequestId: String) -> P): String {
        require(method.mutating) { "${method.name} does not change state: use query()" }
        return enqueueInternal(method.name, { crid -> AasJson.encodeToJsonElement(method.params, build(crid)) }, null)
    }

    /**
     * Like [enqueue], then waits for the final answer. Cancelling the wait does not cancel the
     * request: it stays in the outbox until the server answered it.
     *
     * @throws RpcException the server answered with a definitive error.
     * @throws OutboxClearedException the entry left the outbox without an answer ([resetLocalData], [discardOutbox]).
     */
    suspend fun <P, R> mutate(method: RpcMethod<P, R>, build: (clientRequestId: String) -> P): R = submit(method, build).await()

    /**
     * Commits a mutating request to the outbox like [enqueue] and returns a handle for its final
     * answer ([mutate] is `submit(...).await()`). For requests that must be committed in order
     * before waiting for the first one's answer (e.g. `thread/create`, then `project/update`).
     */
    suspend fun <P, R> submit(method: RpcMethod<P, R>, build: (clientRequestId: String) -> P): PendingMutation<R> {
        require(method.mutating) { "${method.name} does not change state: use query()" }
        val waiter = CompletableDeferred<JsonElement>()
        val crid = enqueueInternal(method.name, { crid -> AasJson.encodeToJsonElement(method.params, build(crid)) }, waiter)
        // The waiter stays registered until the entry is answered or dropped (finishOutbox,
        // resetLocalData), whether or not anyone still awaits it.
        return PendingMutation(crid, waiter) { AasJson.decodeFromJsonElement(method.result, it) }
    }

    /** [enqueue] for a method named at runtime; `clientRequestId` is added to [params]. */
    suspend fun enqueueRaw(method: String, params: JsonObject): String =
        enqueueInternal(method, { crid -> withRequestId(params, crid) }, null)

    /** [mutate] for a method named at runtime; `clientRequestId` is added to [params]. */
    suspend fun mutateRaw(method: String, params: JsonObject): JsonElement {
        val waiter = CompletableDeferred<JsonElement>()
        enqueueInternal(method, { crid -> withRequestId(params, crid) }, waiter)
        return waiter.await()
    }

    /**
     * Drops one outbox entry without an answer, e.g. a message whose harness keeps failing, or a
     * request the user no longer wants sent. An entry whose frame is on the wire right now stays
     * ([OutboxDiscard.InFlight]): the server may be running it. A caller waiting for it fails
     * with [OutboxClearedException] and [results] reports [OutboxResult.Discarded].
     */
    suspend fun discardOutbox(clientRequestId: String): OutboxDiscard {
        synchronized(outboxGuard) {
            if (clientRequestId in outboxGuard.sending) return OutboxDiscard.InFlight
            outboxGuard.discarding += clientRequestId
        }
        try {
            val entry = views.outbox.value.firstOrNull { it.clientRequestId == clientRequestId } ?: return OutboxDiscard.NotFound
            if (!finishOutbox(OutboxResult.Discarded(entry))) return OutboxDiscard.NotFound
            log(SyncLogger.Level.Info, "${entry.method} ($clientRequestId) discarded by the user")
            return OutboxDiscard.Discarded
        } finally {
            synchronized(outboxGuard) { outboxGuard.discarding -= clientRequestId }
            outboxKick.trySend(Unit)
        }
    }

    /**
     * Runs a command's `method` action for [threadId] (protocol.md §3: the client adds
     * `threadId`, and `clientRequestId` for a state-changing method). A method this client
     * knows as read-only (e.g. `thread/diff`) is a [queryRaw]; any other goes through the outbox.
     */
    suspend fun runCommand(action: CommandAction.Method, threadId: ThreadId): JsonElement {
        val base = action.params as? JsonObject ?: JsonObject(emptyMap())
        val params = JsonObject(base + (JsonKeys.THREAD_ID to JsonPrimitive(threadId)))
        val known = Methods.byName(action.method)
        return if (known != null && !known.mutating) queryRaw(action.method, params) else mutateRaw(action.method, params)
    }

    /**
     * Forgets everything local (unpairing): synced data, cursors, epoch and the outbox. Callers
     * waiting in [mutate] fail with [OutboxClearedException]; dropped entries are reported as
     * [OutboxResult.Discarded]. A connected session is replaced (it resyncs from scratch).
     */
    suspend fun resetLocalData() {
        val dropped = write { tx, _ ->
            val entries = tx.outbox()
            tx.wipeSyncedData()
            tx.clearOutbox()
            entries
        }
        _status.update { it.copy(lastSyncAtMs = null, cursors = emptyMap()) }
        val pending = waiters.values.toList()
        waiters.clear()
        pending.forEach { it.completeExceptionally(OutboxClearedException()) }
        dropped.forEach { emitResult(OutboxResult.Discarded(it)) }
        session?.drop(SessionEnd.Retry(DisconnectCause.Reset, RetryDelay.Immediate))
    }

    private fun withRequestId(params: JsonObject, crid: String): JsonObject =
        JsonObject(params + (JsonKeys.CLIENT_REQUEST_ID to JsonPrimitive(crid)))

    private suspend fun enqueueInternal(
        method: String,
        build: (String) -> JsonElement,
        waiter: CompletableDeferred<JsonElement>?,
    ): String {
        val crid = newRequestId()
        val params = build(crid) as? JsonObject ?: throw IllegalArgumentException("$method params must be a JSON object")
        require(params[JsonKeys.CLIENT_REQUEST_ID] == JsonPrimitive(crid)) { "$method params must carry the clientRequestId" }
        // Registered before the entry exists, so even an immediate answer finds its caller.
        if (waiter != null) waiters[crid] = waiter
        try {
            write { tx, _ -> tx.addOutbox(OutboxEntry(crid, method, params, clock.nowMs())) }
        } catch (e: Throwable) {
            waiters.remove(crid)
            throw e
        }
        outboxKick.trySend(Unit)
        return crid
    }

    // ----- connection loop ------------------------------------------------------------------------

    private enum class TriggerKind { UserAction, AppForeground, NetworkAvailable, NetworkLost, CredentialsChanged }

    private data class Trigger(val kind: TriggerKind, val atMs: Long)

    private fun trigger(kind: TriggerKind) {
        triggers.trySend(Trigger(kind, clock.monotonicMs()))
    }

    private enum class RetryDelay {
        /** Full-jitter backoff; any reconnect trigger skips it. */
        Backoff,

        /** The maximum backoff (protocol violation); only a user action skips it. */
        Maximum,

        /** Reconnect at once (the app replaced the connection on purpose). */
        Immediate,
    }

    private sealed interface SessionEnd {
        data class Retry(val cause: DisconnectCause, val delay: RetryDelay) : SessionEnd

        data class Suspend(val reason: SuspendReason) : SessionEnd

        /** The app reported that the network is gone. */
        data object Offline : SessionEnd
    }

    private fun setConnection(state: ConnectionState) {
        _status.update { it.copy(connection = state) }
    }

    private suspend fun runLoop() {
        var attempt = 0
        while (true) {
            val creds = credentials
            val suspended = suspendReason
            val gate = when {
                creds == null -> ConnectionState.NotPaired
                suspended != null -> ConnectionState.Suspended(suspended)
                !networkAvailable -> ConnectionState.Offline
                else -> null
            }
            if (gate != null || creds == null) {
                setConnection(gate ?: ConnectionState.NotPaired)
                // Any trigger re-evaluates the gate (the public methods changed its inputs).
                triggers.receive()
                continue
            }
            setConnection(ConnectionState.Connecting(attempt, everEstablished))
            val (end, established) = try {
                runSession(creds)
            } catch (e: CancellationException) {
                throw e
            } catch (e: Exception) {
                // A local failure (the store, a bug) ends this session, not the engine.
                val message = "the session failed: ${e.message ?: e.javaClass.simpleName}"
                reportError(message, e)
                SessionEnd.Retry(DisconnectCause.ClientError(message), RetryDelay.Backoff) to false
            }
            if (established) attempt = 0
            when (end) {
                is SessionEnd.Suspend -> if (credentials == creds) suspendReason = end.reason
                SessionEnd.Offline -> Unit
                is SessionEnd.Retry -> when (end.delay) {
                    RetryDelay.Immediate -> Unit
                    RetryDelay.Backoff -> {
                        val wait = backoff.delayMs(attempt)
                        attempt++
                        waitBeforeRetry(wait, attempt, end.cause, userActionOnly = false)
                    }
                    RetryDelay.Maximum -> {
                        attempt++
                        waitBeforeRetry(backoff.capMs, attempt, end.cause, userActionOnly = true)
                    }
                }
            }
        }
    }

    /** Waits [delayMs] unless a trigger that applies arrives first (stale triggers are ignored). */
    private suspend fun waitBeforeRetry(delayMs: Long, attempt: Int, cause: DisconnectCause, userActionOnly: Boolean) {
        val start = clock.monotonicMs()
        setConnection(ConnectionState.Reconnecting(attempt, clock.nowMs() + delayMs, cause))
        val deadline = start + delayMs
        while (true) {
            val left = deadline - clock.monotonicMs()
            if (left <= 0) return
            val t = withTimeoutOrNull(left) { triggers.receive() } ?: return
            if (t.atMs < start) continue
            val ends = when (t.kind) {
                TriggerKind.UserAction, TriggerKind.CredentialsChanged, TriggerKind.NetworkLost -> true
                TriggerKind.AppForeground, TriggerKind.NetworkAvailable -> !userActionOnly
            }
            if (ends) return
        }
    }

    private inner class Session(val ws: WsConnection, val rpc: RpcConnection, val scope: CoroutineScope) {
        @Volatile
        var lastFrameAtMs: Long = clock.monotonicMs()

        @Volatile
        var clientTimeoutMs: Long = config.initialClientTimeoutMs

        @Volatile
        var init: InitializeResult? = null

        /** `initialize` and the resubscription completed. */
        @Volatile
        var established = false

        /** Why the engine dropped this socket on purpose (takes precedence over what the socket reports). */
        @Volatile
        var dropCause: SessionEnd? = null

        /** `connection/replaced` arrived (close code 4000 follows). */
        @Volatile
        var replacedNotice = false

        /** The outbox entry sent alone because its frame exceeds `maxClientFrameBytes`. */
        @Volatile
        var largeInFlight: OutboxEntry? = null

        /** Streams whose batches are ignored until a reload commits (after an overlap). */
        val suspendedStreams: MutableSet<String> = ConcurrentHashMap.newKeySet()

        /** Threads loaded and subscribed on this connection. */
        val liveThreads: MutableSet<ThreadId> = ConcurrentHashMap.newKeySet()

        /** Stall bookkeeping (reader coroutine only): stream → cursor and since when. */
        val stallMarks = HashMap<String, StallMark>()

        private var watchdogJob: Job? = null

        /** The watchdog runs (the socket opened); before that the connect timeout bounds the session. */
        @Volatile
        var watching = false
            private set

        /**
         * (Re)starts the watchdog, e.g. when `initialize` replaced the timeout it sleeps on, or
         * when the time left must be measured again ([checkLiveness]). Called from the session's
         * coroutines and from the app's callbacks, hence synchronized.
         */
        @Synchronized
        fun startWatchdog() {
            watchdogJob?.cancel()
            watchdogJob = scope.launch { watchdog(this@Session) }
            watching = true
        }

        fun drop(end: SessionEnd) {
            if (dropCause == null) dropCause = end
            ws.cancel()
        }
    }

    private data class StallMark(val cursor: Long, val sinceMs: Long)

    /**
     * Client request ids of outbox entries whose frame is on the wire ([sending]) and of entries
     * being discarded ([discarding]). Guarded by synchronizing on the instance: [discardOutbox]
     * refuses an entry in flight, and a send never starts for an entry being discarded.
     */
    private class OutboxGuard {
        val sending = HashSet<String>()
        val discarding = HashSet<String>()
    }

    private suspend fun runSession(creds: Credentials): Pair<SessionEnd, Boolean> {
        val request = try {
            Request.Builder().url(creds.wsUrl).header("Authorization", "Bearer ${creds.token}").build()
        } catch (e: IllegalArgumentException) {
            return SessionEnd.Suspend(SuspendReason.InvalidServerUrl(e.message ?: creds.wsUrl)) to false
        }
        val ws = WsConnection.open(wsHttp, request, wireTap)
        val rpc = RpcConnection(ws, config.callTimeoutMs)
        return coroutineScope {
            val s = Session(ws, rpc, this)
            session = s
            try {
                val end = runSocket(s)
                if (end is SessionEnd.Retry && end.cause == DisconnectCause.MessageTooBig) {
                    s.largeInFlight?.let { entry ->
                        failOutbox(entry, RpcConnection.localPayloadTooLarge(entry.method, frameSize(entry), rpc.maxClientFrameBytes))
                    }
                }
                end to s.established
            } finally {
                if (session === s) session = null
                rpc.close("the connection closed")
                ws.cancel()
                coroutineContext.cancelChildren()
                withContext(NonCancellable) {
                    if (s.established) writeLock.withLock { views.allCached() }
                    awaitReleased(ws)
                }
            }
        }
    }

    /**
     * Waits until OkHttp reported the end of a dropped socket, so the next one is never opened
     * while this one could still hold a connection (the engine's "one socket at a time").
     */
    private suspend fun awaitReleased(ws: WsConnection) {
        val released = withTimeoutOrNull(config.socketReleaseTimeoutMs) { ws.released.await() }
        if (released == null) {
            reportError("the dropped socket did not report its end within ${config.socketReleaseTimeoutMs} ms; connecting anyway")
        }
    }

    private suspend fun runSocket(s: Session): SessionEnd {
        when (val first = withTimeoutOrNull(config.connectTimeoutMs) { s.ws.events.receiveCatching().getOrNull() }) {
            null -> return s.dropCause
                ?: SessionEnd.Retry(DisconnectCause.Network("no WebSocket connection within ${config.connectTimeoutMs} ms"), RetryDelay.Backoff)
            is WsEvent.Failure -> return s.dropCause ?: failureEnd(s, first)
            is WsEvent.Closing -> return s.dropCause ?: closeEnd(s, first.code, first.reason)
            is WsEvent.Text -> return SessionEnd.Retry(DisconnectCause.Network("a frame arrived before the upgrade completed"), RetryDelay.Backoff)
            WsEvent.Open -> Unit
        }
        s.lastFrameAtMs = clock.monotonicMs()
        s.startWatchdog()
        s.scope.launch { setup(s) }
        while (true) {
            val event = s.ws.events.receiveCatching().getOrNull()
                ?: return s.dropCause ?: SessionEnd.Retry(DisconnectCause.Network("the connection closed"), RetryDelay.Backoff)
            s.lastFrameAtMs = clock.monotonicMs()
            when (event) {
                is WsEvent.Text -> try {
                    handleText(s, event.text)
                } catch (e: CancellationException) {
                    throw e
                } catch (e: Exception) {
                    // Applying the frame failed locally (the store threw): nothing of it was
                    // committed, so the next session replays it from the stored cursor.
                    val message = "applying a frame failed: ${e.message ?: e.javaClass.simpleName}"
                    reportError(message, e)
                    return SessionEnd.Retry(DisconnectCause.ClientError(message), RetryDelay.Backoff)
                }
                is WsEvent.Closing -> return s.dropCause ?: closeEnd(s, event.code, event.reason)
                is WsEvent.Failure -> return s.dropCause ?: failureEnd(s, event)
                WsEvent.Open -> Unit
            }
        }
    }

    private fun failureEnd(s: Session, failure: WsEvent.Failure): SessionEnd = when (failure.httpStatus) {
        HTTP_UNAUTHORIZED, HTTP_FORBIDDEN -> SessionEnd.Suspend(SuspendReason.Unauthorized(failure.httpStatus))
        else -> if (s.replacedNotice) {
            SessionEnd.Suspend(SuspendReason.Replaced)
        } else {
            val message = failure.httpStatus?.let { "the server refused the WebSocket upgrade (HTTP $it)" }
                ?: failure.error.message ?: failure.error.javaClass.simpleName
            SessionEnd.Retry(DisconnectCause.Network(message), RetryDelay.Backoff)
        }
    }

    private fun closeEnd(s: Session, code: Int, reason: String): SessionEnd = when (code) {
        CLOSE_REPLACED -> SessionEnd.Suspend(SuspendReason.Replaced)
        CLOSE_REVOKED -> SessionEnd.Suspend(SuspendReason.Revoked)
        CLOSE_CLIENT_TIMEOUT -> SessionEnd.Retry(DisconnectCause.ServerTimeout, RetryDelay.Backoff)
        CLOSE_PROTOCOL_VIOLATION -> {
            reportError("the server closed the connection for a protocol violation (4003): $reason")
            SessionEnd.Retry(DisconnectCause.ProtocolViolation(reason), RetryDelay.Maximum)
        }
        CLOSE_MESSAGE_TOO_BIG -> SessionEnd.Retry(DisconnectCause.MessageTooBig, RetryDelay.Backoff)
        CLOSE_GOING_AWAY -> SessionEnd.Retry(DisconnectCause.ServerShutdown, RetryDelay.Backoff)
        else -> if (s.replacedNotice) {
            SessionEnd.Suspend(SuspendReason.Replaced)
        } else {
            SessionEnd.Retry(DisconnectCause.Closed(code, reason), RetryDelay.Backoff)
        }
    }

    /**
     * Measures the current connection's silence against the client timeout now (protocol.md
     * §2.2), at moments after which the phone may have slept (the app returning to the
     * foreground, a network becoming available). The watchdog's timer runs on the coroutine
     * clock, which stops in deep sleep: after waking it would still wait for the rest of its delay
     * while the socket died long ago, and the app would show the dead connection as live.
     * [Clock.monotonicMs] counts the sleep, so a connection that has been silent for the client
     * timeout is closed and replaced at once (without the backoff wait: nothing failed to
     * connect, and the user is looking at the app); otherwise the watchdog is re-armed for the
     * time actually left.
     */
    private fun checkLiveness(reason: String) {
        val s = session ?: return
        if (!s.watching) return
        val timeout = s.clientTimeoutMs
        val idle = clock.monotonicMs() - s.lastFrameAtMs
        if (idle >= timeout) {
            log(SyncLogger.Level.Info, "$reason: no frame for $idle ms (client timeout $timeout ms): closing the connection")
            s.drop(SessionEnd.Retry(DisconnectCause.Watchdog(timeout), RetryDelay.Immediate))
        } else {
            s.startWatchdog()
        }
    }

    /** Closes the socket when no frame arrived within the client timeout (protocol.md §2.2, §7.5). */
    private suspend fun watchdog(s: Session) {
        while (true) {
            val timeout = s.clientTimeoutMs
            val idle = clock.monotonicMs() - s.lastFrameAtMs
            if (idle >= timeout) {
                log(SyncLogger.Level.Info, "no frame within $timeout ms: closing the connection")
                s.drop(SessionEnd.Retry(DisconnectCause.Watchdog(timeout), RetryDelay.Backoff))
                return
            }
            delay(timeout - idle)
        }
    }

    // ----- setup ----------------------------------------------------------------------------------

    private suspend fun setup(s: Session) {
        try {
            val lastEpoch = store.transaction { it.epoch() }
            val init = try {
                call(s, Methods.Initialize, InitializeParams(PROTOCOL_VERSION, clientInfo, lastEpoch))
            } catch (e: RpcException) {
                if (e.kind != ErrorKind.ProtocolVersionUnsupported) throw e
                reportError("the server does not support protocol $PROTOCOL_VERSION: ${e.error.message}")
                s.drop(SessionEnd.Suspend(SuspendReason.Incompatible(e.error.message)))
                return
            }
            if (init.protocolVersion != PROTOCOL_VERSION) {
                val message = "the server speaks protocol ${init.protocolVersion}, this client $PROTOCOL_VERSION"
                reportError(message)
                s.drop(SessionEnd.Suspend(SuspendReason.Incompatible(message)))
                return
            }
            s.init = init
            s.clientTimeoutMs = init.policy.clientTimeoutMs
            s.startWatchdog()
            s.rpc.maxClientFrameBytes = init.policy.maxClientFrameBytes
            _status.update { it.copy(server = init.server, deviceId = init.device.id, policy = init.policy) }
            followLock.withLock {
                val workspaceCursor = store.transaction { it.cursor(WORKSPACE_STREAM) }
                val fresh = lastEpoch == null || workspaceCursor == null || init.epochChanged || lastEpoch != init.server.epoch
                if (fresh) fullResync(s) else resume(s, workspaceCursor)
                s.established = true
            }
            val now = clock.nowMs()
            write { tx, _ -> tx.setLastSyncAtMs(now) }
            val reconnect = everEstablished
            everEstablished = true
            _status.update {
                it.copy(
                    connection = ConnectionState.Online(now),
                    lastError = null,
                    serverShuttingDown = false,
                    serverShutdownReason = null,
                    serverRestartExpected = null,
                    reconnects = if (reconnect) it.reconnects + 1 else it.reconnects,
                )
            }
            log(SyncLogger.Level.Info, "session established (epoch ${init.server.epoch})")
            s.scope.launch { runOutbox(s) }
        } catch (e: CancellationException) {
            throw e
        } catch (e: ConnectionLostException) {
            // The reader reports why the connection ended.
        } catch (e: Exception) {
            val message = "setup failed: ${e.message ?: e.javaClass.simpleName}"
            reportError(message, e)
            s.drop(SessionEnd.Retry(DisconnectCause.SetupFailed(message), RetryDelay.Backoff))
        }
    }

    /** Wipe + snapshot + subscribe from its head, then every open thread (protocol.md §2, step 2). */
    private suspend fun fullResync(s: Session) {
        val epoch = s.init!!.server.epoch
        log(SyncLogger.Level.Info, "full resync (epoch $epoch)")
        s.suspendedStreams += WORKSPACE_STREAM
        val snapshot = call(s, Methods.WorkspaceSnapshot, Empty)
        write { tx, signals -> EventApplier.applySnapshot(tx, epoch, snapshot, signals) }
        s.suspendedStreams -= WORKSPACE_STREAM
        s.liveThreads.clear()
        val result = subscribe(s, listOf(Subscription(WORKSPACE_STREAM, snapshot.head)))
        check(result.subscriptions.any { it.stream == WORKSPACE_STREAM && it.status == SubscriptionState.Ok }) {
            "the server did not accept the workspace subscription: ${result.subscriptions}"
        }
        for (threadId in openCounts.keys.toList()) loadThread(s, threadId)
    }

    /** Resubscribes the workspace and the open threads from their cursors (protocol.md §2, step 3). */
    private suspend fun resume(s: Session, workspaceCursor: Long) {
        val cursors = store.transaction { it.cursors() }
        val subscriptions = mutableListOf(Subscription(WORKSPACE_STREAM, workspaceCursor))
        val needRead = mutableListOf<ThreadId>()
        for (threadId in openCounts.keys.toList()) {
            val cursor = cursors[threadStream(threadId)]
            if (cursor != null) subscriptions += Subscription(threadStream(threadId), cursor) else needRead += threadId
        }
        val requested = subscriptions.associate { it.stream to it.after }
        val result = subscribe(s, subscriptions)
        for (st in result.subscriptions) {
            val after = requested[st.stream] ?: continue
            if (st.stream == WORKSPACE_STREAM) {
                // A head behind the cursor means the server lost events we applied (a restored
                // data folder): the local state cannot be trusted, start over.
                if (st.status != SubscriptionState.Ok || st.head < after) {
                    log(SyncLogger.Level.Warn, "workspace head ${st.head} is behind the cursor $after (${st.status}): resyncing")
                    fullResync(s)
                    return
                }
                continue
            }
            val threadId = threadIdOfStream(st.stream) ?: continue
            when {
                st.status == SubscriptionState.NotFound -> threadGone(s, threadId)
                st.status != SubscriptionState.Ok || st.head < after -> needRead += threadId
                else -> {
                    s.liveThreads += threadId
                    writeLock.withLock { views.setSync(threadId, ThreadSync.Live) }
                }
            }
        }
        for (threadId in needRead) loadThread(s, threadId)
    }

    /**
     * `thread/read` of the latest page, committed with the stream's cursor, then `subscribe`
     * after its head. Batches of an earlier subscription that arrive meanwhile are harmless:
     * those before the head are skipped by the cursor, later ones continue from it.
     *
     * A failure other than the connection's own (an error answer such as `internal` for data the
     * server cannot decode, no answer in time, an unreadable result, the store) concerns this
     * thread only: it becomes [ThreadSync.Failed], its stream's batches are ignored, and the
     * caller (the setup, a reload) goes on. A lost connection is rethrown (the reader handles it).
     */
    private suspend fun loadThread(s: Session, threadId: ThreadId) {
        val stream = threadStream(threadId)
        try {
            writeLock.withLock { views.setSync(threadId, ThreadSync.Loading) }
            val read = try {
                call(s, Methods.ThreadRead, ThreadReadParams(threadId, limitTurns = config.threadPageTurns))
            } catch (e: RpcException) {
                if (e.kind != ErrorKind.NotFound) throw e
                threadGone(s, threadId)
                return
            }
            if (!openCounts.containsKey(threadId)) return
            write { tx, signals -> EventApplier.applyThreadRead(tx, read, signals, ::warnData) }
            s.suspendedStreams -= stream
            val result = subscribe(s, listOf(Subscription(stream, read.head)))
            if (result.subscriptions.any { it.stream == stream && it.status == SubscriptionState.NotFound }) {
                threadGone(s, threadId)
                return
            }
            s.liveThreads += threadId
            writeLock.withLock { views.setSync(threadId, ThreadSync.Live) }
        } catch (e: CancellationException) {
            throw e
        } catch (e: ConnectionLostException) {
            throw e
        } catch (e: Exception) {
            s.liveThreads -= threadId
            // Until a later load commits, whatever this stream delivers has no trusted base.
            s.suspendedStreams += stream
            val message = "loading thread $threadId failed: ${e.message ?: e.javaClass.simpleName}"
            reportError(message, e)
            writeLock.withLock { views.setLoadFailed(threadId, SyncError(clock.nowMs(), message)) }
        }
    }

    private suspend fun threadGone(s: Session, threadId: ThreadId) {
        s.liveThreads -= threadId
        s.suspendedStreams -= threadStream(threadId)
        write { tx, signals ->
            if (tx.thread(threadId) != null) signals += SyncSignal.ThreadRemoved(threadId)
            tx.removeThread(threadId)
        }
    }

    private suspend fun subscribe(s: Session, subscriptions: List<Subscription>): SubscribeResult =
        call(s, Methods.Subscribe, SubscribeParams(subscriptions))

    /**
     * Runs [block] (a reload, a resubscription or an outbox send on a live session); a failure
     * other than a lost connection drops the session, whose next setup recovers from the
     * stored state. (A thread's own load failure does not get here: [loadThread] keeps it to
     * that thread.)
     */
    private suspend fun guarded(s: Session, block: suspend () -> Unit) {
        try {
            block()
        } catch (e: CancellationException) {
            throw e
        } catch (e: ConnectionLostException) {
            // The reader reports why the connection ended.
        } catch (e: Exception) {
            val message = e.message ?: e.javaClass.simpleName
            reportError(message, e)
            val cause = if (e is RpcException || e is CallTimeoutException) DisconnectCause.SetupFailed(message) else DisconnectCause.ClientError(message)
            s.drop(SessionEnd.Retry(cause, RetryDelay.Backoff))
        }
    }

    // ----- incoming frames (reader coroutine: never waits for a response) -------------------------

    private suspend fun handleText(s: Session, text: String) {
        val msg = try {
            AasJson.decodeFromString(RpcMessage.serializer(), text)
        } catch (e: SerializationException) {
            reportError("unreadable frame from the server: ${e.message}")
            return
        } catch (e: IllegalArgumentException) {
            reportError("unreadable frame from the server: ${e.message}")
            return
        }
        when (msg.kind) {
            RpcMessage.Kind.Response -> s.rpc.onResponse(msg)
            RpcMessage.Kind.Notification -> onNotification(s, msg.method!!, msg.params)
            // The server sends no requests (protocol.md §1); anything else is not JSON-RPC.
            RpcMessage.Kind.Request, RpcMessage.Kind.Invalid -> log(SyncLogger.Level.Warn, "ignored frame: ${text.take(LOG_EXCERPT_CHARS)}")
        }
    }

    private suspend fun onNotification(s: Session, method: String, params: JsonElement?) {
        val notification = try {
            ServerNotification.parse(method, params)
        } catch (e: SerializationException) {
            reportError("malformed $method notification: ${e.message}", e)
            return
        } catch (e: IllegalArgumentException) {
            reportError("malformed $method notification: ${e.message}", e)
            return
        }
        when (notification) {
            is ServerNotification.Batch -> applyBatch(s, notification.batch)
            is ServerNotification.Beat -> onHeartbeat(s, notification.heartbeat.heads)
            ServerNotification.Replaced -> {
                s.replacedNotice = true
                log(SyncLogger.Level.Info, "replaced by a newer connection of this device")
            }
            is ServerNotification.ShuttingDown -> {
                val info = notification.info
                log(SyncLogger.Level.Info, "the server is shutting down (${info.reason.wire}, restart expected: ${info.restartExpected})")
                _status.update {
                    it.copy(serverShuttingDown = true, serverShutdownReason = info.reason, serverRestartExpected = info.restartExpected)
                }
            }
            is ServerNotification.Unknown -> log(SyncLogger.Level.Debug, "unknown notification ${notification.method}")
        }
    }

    private fun isFollowed(stream: String): Boolean =
        stream == WORKSPACE_STREAM || threadIdOfStream(stream)?.let { openCounts.containsKey(it) } == true

    private suspend fun applyBatch(s: Session, batch: StreamBatch) {
        if (!isFollowed(batch.stream) || batch.stream in s.suspendedStreams) return
        val now = clock.nowMs()
        val outcome = write { tx, signals ->
            // An empty batch moves the cursor to the head (protocol.md §2.1): see EventApplier.
            val o = EventApplier.applyBatch(tx, batch, signals, ::warnData)
            if (o.cursorMoved) tx.setLastSyncAtMs(now)
            o
        }
        val overlap = outcome.overlapAtSeq ?: return
        log(SyncLogger.Level.Info, "merged delta ending at $overlap overlaps the cursor of ${batch.stream}: reloading")
        s.suspendedStreams += batch.stream
        s.scope.launch {
            followLock.withLock {
                if (session !== s) return@withLock
                guarded(s) {
                    if (batch.stream == WORKSPACE_STREAM) {
                        fullResync(s)
                    } else {
                        val threadId = threadIdOfStream(batch.stream)
                        if (threadId != null && openCounts.containsKey(threadId)) loadThread(s, threadId) else s.suspendedStreams -= batch.stream
                    }
                }
            }
        }
    }

    private fun onHeartbeat(s: Session, heads: Map<String, Long>) {
        val now = clock.nowMs()
        val caughtUp = s.established && heads.all { (stream, head) -> !isFollowed(stream) || (views.cursors[stream] ?: -1) >= head }
        _status.update {
            it.copy(
                lastHeartbeatAtMs = now,
                serverHeads = heads,
                lastSyncAtMs = if (caughtUp) maxOf(now, it.lastSyncAtMs ?: 0) else it.lastSyncAtMs,
            )
        }
        if (!s.established) return
        val stalled = mutableListOf<Subscription>()
        val mono = clock.monotonicMs()
        for ((stream, head) in heads) {
            val cursor = views.cursors[stream]
            if (!isFollowed(stream) || stream in s.suspendedStreams || cursor == null || head <= cursor) {
                s.stallMarks.remove(stream)
                continue
            }
            val mark = s.stallMarks[stream]
            when {
                mark == null || mark.cursor != cursor -> s.stallMarks[stream] = StallMark(cursor, mono)
                // No progress for a whole client timeout while the server's head is ahead: the
                // batches are not coming (protocol.md §2.2), so resubscribe from the cursor.
                mono - mark.sinceMs >= s.clientTimeoutMs -> {
                    s.stallMarks.remove(stream)
                    stalled += Subscription(stream, cursor)
                }
            }
        }
        if (stalled.isEmpty()) return
        s.scope.launch {
            followLock.withLock {
                if (session !== s) return@withLock
                log(SyncLogger.Level.Info, "resubscribing stalled streams ${stalled.map { it.stream }}")
                _status.update { it.copy(stallResubscribes = it.stallResubscribes + stalled.size) }
                guarded(s) {
                    val result = subscribe(s, stalled)
                    for (st in result.subscriptions) {
                        if (st.status == SubscriptionState.NotFound) threadIdOfStream(st.stream)?.let { threadGone(s, it) }
                    }
                }
            }
        }
    }

    // ----- outbox ---------------------------------------------------------------------------------

    /**
     * Sends outbox entries on an established session, in the order they were made:
     *
     * * One entry at a time per lane ([laneOf]): the next entry of a thread waits until the
     *   previous one was answered, so a retried request never lands after a later one.
     * * Except for the stop controls ([bypassesRetryWait]): while the lane's first entry only
     *   waits for its retry (it is not on the wire), `turn/interrupt`, `thread/stop` and
     *   `backgroundTask/stop` of that thread go ahead of it, one at a time in their own order.
     *   Stopping the agent (or its background work) must not
     *   depend on an input the server refuses for now (a steer that keeps failing, a harness
     *   that is gone). An entry on the wire is still waited for: the server handles the
     *   requests of one thread one at a time in arrival order anyway (protocol.md §1).
     * * An entry whose frame may exceed the server's `maxClientFrameBytes` is sent alone. The
     *   server answers it with `payloadTooLarge` or, beyond its transport limit, closes the
     *   connection with 1009 — sent alone, that close is attributable to it (see [runSession]).
     * * An entry that failed non-definitively waits until its `nextAttemptAtMs`.
     * * An entry waiting for a harness ([OutboxEntry.waitingForHarness]) is not sent while the
     *   synced workspace does not list that harness as available, whatever its `nextAttemptAtMs`
     *   says; it holds its lane like an entry waiting for its retry.
     */
    private suspend fun runOutbox(s: Session) {
        val inFlight = HashMap<String, Job>()
        val largeCache = HashMap<String, Boolean>()
        var exclusive: String? = null
        while (true) {
            inFlight.values.removeAll { it.isCompleted }
            if (exclusive != null && exclusive !in inFlight) {
                exclusive = null
                s.largeInFlight = null
            }
            val entries = views.outbox.value
            largeCache.keys.retainAll(entries.map { it.clientRequestId }.toSet())
            val now = clock.nowMs()
            var wakeAt: Long? = null
            val lanes = HashMap<String, LaneState>()
            for (entry in entries) {
                val lane = laneOf(entry)
                val before = lanes[lane]
                when (before) {
                    LaneState.Busy -> continue
                    LaneState.RetryWaiting -> if (!bypassesRetryWait(entry)) continue
                    null -> Unit
                }
                if (entry.clientRequestId in inFlight) {
                    lanes[lane] = LaneState.Busy
                    continue
                }
                val harness = entry.waitingForHarness
                if (harness != null && !harnessAvailable(harness)) {
                    // No timer: the server probes the harness itself and says when it is back
                    // (harness/updated), which releases the entry (releaseHarnessWaits).
                    lanes[lane] = if (before == null && !bypassesRetryWait(entry)) LaneState.RetryWaiting else LaneState.Busy
                    continue
                }
                if (entry.nextAttemptAtMs > now) {
                    wakeAt = minOf(wakeAt ?: Long.MAX_VALUE, entry.nextAttemptAtMs)
                    // A stop control waiting for its own retry holds back everything after it
                    // (the stop controls keep their order among themselves).
                    lanes[lane] = if (before == null && !bypassesRetryWait(entry)) LaneState.RetryWaiting else LaneState.Busy
                    continue
                }
                lanes[lane] = LaneState.Busy
                if (exclusive != null) break
                val large = largeCache.getOrPut(entry.clientRequestId) { isLarge(s, entry) }
                if (large && inFlight.isNotEmpty()) break
                inFlight[entry.clientRequestId] = s.scope.launch {
                    guarded(s) { sendEntry(s, entry) }
                    outboxKick.trySend(Unit)
                }
                if (large) {
                    exclusive = entry.clientRequestId
                    s.largeInFlight = entry
                    break
                }
            }
            val wait = wakeAt?.let { (it - clock.nowMs()).coerceAtLeast(1) }
            if (wait == null) outboxKick.receive() else withTimeoutOrNull(wait) { outboxKick.receive() }
        }
    }

    /** What the entries seen so far allow for the rest of a lane in one pass of [runOutbox]. */
    private enum class LaneState {
        /** An entry of the lane is on the wire or was just started: the rest waits. */
        Busy,

        /** The lane's first entry waits for its retry: only [bypassesRetryWait] entries may go. */
        RetryWaiting,
    }

    /**
     * The requests that stop work — `turn/interrupt`, `thread/stop` and `backgroundTask/stop` —
     * pass an entry of their thread that waits for its retry: input the server does not accept
     * now must not keep the user from stopping a runaway agent or its background work.
     */
    private fun bypassesRetryWait(entry: OutboxEntry): Boolean = entry.method in STOP_METHODS

    /** Requests that must keep their order share a lane: same thread, same project, same interaction. */
    private fun laneOf(entry: OutboxEntry): String {
        entry.threadId?.let { return "thread:$it" }
        entry.projectId?.let { return "project:$it" }
        (entry.params[JsonKeys.INTERACTION_ID] as? JsonPrimitive)?.let { return "interaction:${it.content}" }
        return "global"
    }

    /** Size of the entry's frame with the widest possible id (an upper bound of the real frame). */
    private fun frameSize(entry: OutboxEntry): Long =
        AasJson.encodeToString(RpcMessage.serializer(), RpcMessage.request(Long.MAX_VALUE, entry.method, entry.params)).utf8Size()

    private fun isLarge(s: Session, entry: OutboxEntry): Boolean {
        val limit = s.rpc.maxClientFrameBytes ?: return false
        return frameSize(entry) > limit
    }

    private suspend fun sendEntry(s: Session, entry: OutboxEntry) {
        val crid = entry.clientRequestId
        synchronized(outboxGuard) {
            // Discarded since the pass of runOutbox that chose it.
            if (crid in outboxGuard.discarding || views.outbox.value.none { it.clientRequestId == crid }) return
            outboxGuard.sending += crid
        }
        try {
            val result = s.rpc.call(entry.method, entry.params, enforceFrameLimit = false)
            finishOutbox(OutboxResult.Succeeded(entry, result))
        } catch (e: CancellationException) {
            throw e
        } catch (e: RpcException) {
            when {
                e.kind.definitive -> finishOutbox(OutboxResult.Failed(entry, e.error))
                e.kind == ErrorKind.HarnessUnavailable -> waitForHarness(entry, e.error)
                else -> retryOutbox(entry, "${e.kind.wire}: ${e.error.message}")
            }
        } catch (e: CallTimeoutException) {
            retryOutbox(entry, e.message ?: "no response")
        } catch (e: ConnectionLostException) {
            // Resent after the next connection's setup, with the same clientRequestId.
        } finally {
            synchronized(outboxGuard) { outboxGuard.sending -= crid }
        }
    }

    private suspend fun failOutbox(entry: OutboxEntry, error: dev.aas.android.protocol.RpcError) {
        finishOutbox(OutboxResult.Failed(entry, error))
    }

    /**
     * Removes an answered (or discarded) entry, once, and reports the outcome to its caller and
     * [results]. Returns whether the entry was still there.
     */
    private suspend fun finishOutbox(result: OutboxResult): Boolean {
        val crid = result.entry.clientRequestId
        val removed = write { tx, _ -> tx.removeOutbox(crid) }
        if (!removed) return false
        val waiter = waiters.remove(crid)
        when (result) {
            is OutboxResult.Succeeded -> waiter?.complete(result.result)
            is OutboxResult.Failed -> {
                log(SyncLogger.Level.Info, "${result.entry.method} ($crid) failed definitively: ${result.error.kind.wire}")
                waiter?.completeExceptionally(RpcException(result.error))
            }
            is OutboxResult.Discarded -> waiter?.completeExceptionally(OutboxClearedException())
        }
        emitResult(result)
        return true
    }

    private suspend fun retryOutbox(entry: OutboxEntry, message: String) {
        val failures = entry.failures + 1
        val next = entry.copy(
            failures = failures,
            lastError = message,
            nextAttemptAtMs = clock.nowMs() + config.outboxRetryDelayMs(failures),
            waitingForHarness = null,
        )
        log(SyncLogger.Level.Info, "${entry.method} (${entry.clientRequestId}) failed ($message); retrying later")
        write { tx, _ -> tx.updateOutbox(next) }
    }

    /**
     * The server refused [entry] with `harnessUnavailable` (not definitive, protocol.md §1.3): it
     * had probed the harness again before refusing. The entry waits for that harness with the
     * server's reason; it is resent once the synced workspace lists the harness as available
     * (`harness/updated`, [releaseHarnessWaits]), not on a timer. The retry delay still applies
     * while the workspace already shows the harness as available (its update may be on the way,
     * or the server disagrees with it), so the entry never loops.
     *
     * The harness comes from `data.harnessId`, else from the request (`harnessId`, or the
     * thread's harness). A request whose harness is unknown is retried like any other
     * non-definitive failure.
     */
    private suspend fun waitForHarness(entry: OutboxEntry, error: RpcError) {
        val harness = error.harnessId ?: harnessOf(entry)
        if (harness == null) {
            retryOutbox(entry, "${error.kind.wire}: ${error.message}")
            return
        }
        val failures = entry.failures + 1
        val reason = error.reason ?: error.message
        val next = entry.copy(
            failures = failures,
            lastError = reason,
            nextAttemptAtMs = clock.nowMs() + config.outboxRetryDelayMs(failures),
            waitingForHarness = harness,
        )
        log(SyncLogger.Level.Info, "${entry.method} (${entry.clientRequestId}) waits for harness $harness: $reason")
        write { tx, _ -> tx.updateOutbox(next) }
    }

    /** The harness a request needs: its `harnessId`, else its thread's harness. */
    private fun harnessOf(entry: OutboxEntry): String? =
        (entry.params[JsonKeys.HARNESS_ID] as? JsonPrimitive)?.contentOrNull
            ?: entry.threadId?.let { id -> views.workspace.value.threads.firstOrNull { it.thread.id == id }?.thread?.harnessId }

    private fun harnessAvailable(harnessId: String): Boolean = views.workspace.value.harnesses.any { it.id == harnessId && it.available }

    private fun availableHarnesses(): Set<String> = views.workspace.value.harnesses.filter { it.available }.map { it.id }.toSet()

    /**
     * Sends the entries waiting for any of [harnessIds] at once: those harnesses became
     * available (a `harness/updated` or snapshot committed, or a `harness/refresh` said so).
     */
    private suspend fun releaseHarnessWaits(harnessIds: Set<String>) {
        if (harnessIds.isEmpty() || views.outbox.value.none { it.waitingForHarness in harnessIds }) return
        val released = write { tx, _ ->
            tx.outbox().filter { it.waitingForHarness in harnessIds }.onEach { tx.updateOutbox(it.copy(nextAttemptAtMs = 0, waitingForHarness = null)) }
        }
        if (released.isEmpty()) return
        log(SyncLogger.Level.Info, "harness ${harnessIds.joinToString()} available: sending ${released.map { it.clientRequestId }}")
        outboxKick.trySend(Unit)
    }

    // ----- store access ---------------------------------------------------------------------------

    /**
     * Runs one store transaction and, after it committed, mirrors its writes into the views,
     * refreshes the status and emits the collected signals. The transaction itself is not
     * cancellable once started: a commit whose caller was cancelled must still reach the views.
     */
    private suspend fun <T> write(block: suspend (SyncTx, MutableList<SyncSignal>) -> T): T {
        var nowAvailable: Set<String> = emptySet()
        val result = writeLock.withLock { writeLocked(block) { nowAvailable = it } }
        // Outside the lock: releasing is a write of its own.
        if (nowAvailable.isNotEmpty()) withContext(NonCancellable) { releaseHarnessWaits(nowAvailable) }
        return result
    }

    /** [write] under [writeLock]; reports the harnesses the commit made available to [onAvailable]. */
    private suspend fun <T> writeLocked(block: suspend (SyncTx, MutableList<SyncSignal>) -> T, onAvailable: (Set<String>) -> Unit): T {
        var recorder: RecordingTx? = null
        var signals = ArrayList<SyncSignal>()
        val result = withContext(NonCancellable) {
            store.transaction { tx ->
                val rec = RecordingTx(tx)
                recorder = rec
                signals = ArrayList()
                block(rec, signals)
            }
        }
        val changes = recorder?.changes.orEmpty()
        val harnessesChanged = changes.any { it is StoreChange.HarnessUpserted || it is StoreChange.HarnessesReplaced }
        val availableBefore = if (harnessesChanged) availableHarnesses() else emptySet()
        views.apply(changes)
        if (harnessesChanged) onAvailable(availableHarnesses() - availableBefore)
        if (changes.isNotEmpty()) {
            _status.update {
                it.copy(
                    pendingOutbox = views.outbox.value.size,
                    cursors = views.cursors.toMap(),
                    lastSyncAtMs = views.lastSyncAtMs?.let { stored -> maxOf(stored, it.lastSyncAtMs ?: 0) } ?: it.lastSyncAtMs,
                )
            }
        }
        signals.forEach(::emitSignal)
        return result
    }

    private suspend fun <P, R> call(s: Session, method: RpcMethod<P, R>, params: P): R {
        val result = s.rpc.call(method.name, encode(method, params), enforceFrameLimit = true)
        return AasJson.decodeFromJsonElement(method.result, result)
    }

    private fun <P> encode(method: RpcMethod<P, *>, params: P): JsonElement = AasJson.encodeToJsonElement(method.params, params)

    private fun emitSignal(signal: SyncSignal) {
        if (!_signals.tryEmit(signal)) {
            _status.update { it.copy(droppedSignals = it.droppedSignals + 1) }
            log(SyncLogger.Level.Warn, "signal dropped (collector too slow): $signal")
        }
    }

    private fun emitResult(result: OutboxResult) {
        if (!_results.tryEmit(result)) log(SyncLogger.Level.Warn, "outbox result dropped (collector too slow): ${result.entry.clientRequestId}")
    }

    private fun reportError(message: String, error: Throwable? = null) {
        logger.log(SyncLogger.Level.Warn, message, error)
        _status.update { it.copy(lastError = SyncError(clock.nowMs(), message)) }
    }

    private fun log(level: SyncLogger.Level, message: String) = logger.log(level, message, null)

    /** Something in the server's data had to be corrected to be stored (see [EventApplier]). */
    private fun warnData(message: String) = log(SyncLogger.Level.Warn, message)

    companion object {
        /** Close codes of the server (protocol.md §2.3). */
        const val CLOSE_GOING_AWAY = 1001
        const val CLOSE_MESSAGE_TOO_BIG = 1009
        const val CLOSE_REPLACED = 4000
        const val CLOSE_REVOKED = 4001
        const val CLOSE_CLIENT_TIMEOUT = 4002
        const val CLOSE_PROTOCOL_VIOLATION = 4003

        /** Upgrade refusals that mean the token is not accepted (protocol.md §1: 401 without a valid token). */
        private const val HTTP_UNAUTHORIZED = 401
        private const val HTTP_FORBIDDEN = 403

        /** Characters of an unexpected frame kept in the log. */
        private const val LOG_EXCERPT_CHARS = 200

        /**
         * Buffer of [signals] and [results] for collectors that are momentarily slow (a burst of
         * a replay). A capacity, not a timing policy: it bounds memory, never delays anything.
         */
        const val EVENT_BUFFER = 1024

        /** The requests that stop work: they pass a retry-waiting entry of their lane ([bypassesRetryWait]). */
        private val STOP_METHODS = setOf(Methods.TurnInterrupt.name, Methods.ThreadStop.name, Methods.BackgroundTaskStop.name)
    }
}
