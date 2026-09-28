package dev.aas.android.net

import dev.aas.android.AppPolicy
import okhttp3.OkHttpClient
import java.util.concurrent.TimeUnit

/** The app's OkHttp client (one per process: shared connection pool and dispatcher). */
object HttpClients {
    /**
     * The base client for the plain HTTP calls (pairing, blobs). The sync engine derives its
     * WebSocket client from it (its own connect timeout, no read timeout: silence is detected by
     * the heartbeat watchdog). Cleartext is governed by the platform's network security config,
     * which OkHttp enforces: only debug builds may use `ws://` to loopback / the emulator host.
     */
    fun base(policy: AppPolicy): OkHttpClient = OkHttpClient.Builder()
        .connectTimeout(policy.httpConnectTimeoutMs, TimeUnit.MILLISECONDS)
        .readTimeout(policy.httpReadTimeoutMs, TimeUnit.MILLISECONDS)
        .writeTimeout(policy.httpWriteTimeoutMs, TimeUnit.MILLISECONDS)
        // The daemon never redirects; following one could send the device token elsewhere.
        .followRedirects(false)
        .followSslRedirects(false)
        .build()
}
