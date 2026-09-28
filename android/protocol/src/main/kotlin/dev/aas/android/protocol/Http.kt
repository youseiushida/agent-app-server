package dev.aas.android.protocol

import kotlinx.serialization.Serializable
import java.net.URI
import java.net.URLDecoder

/** `POST /v1/pair` request. */
@Serializable
data class PairRequest(val code: String, val deviceName: String, val platform: String)

/** `POST /v1/pair` response. */
@Serializable
data class PairResponse(val deviceId: DeviceId, val token: String, val server: PairServerInfo)

@Serializable
data class PairServerInfo(val name: String, val epoch: String)

/** `POST /v1/blobs` response. */
@Serializable
data class BlobUploadResponse(val blobId: BlobId, val mime: String, val size: Long)

/** Error body of every HTTP endpoint. */
@Serializable
data class HttpError(val kind: String, val message: String)

/** `GET /v1/healthz` response. */
@Serializable
data class Health(val ok: Boolean)

/** Contents of a pairing QR code: `aas://pair?u=<wss url>&c=<code>&n=<server name>`. */
data class PairingLink(val wsUrl: String, val code: String, val serverName: String?) {
    companion object {
        /** Parses a pairing link; `null` when it is not one. */
        fun parse(text: String): PairingLink? {
            val trimmed = text.trim()
            if (!trimmed.startsWith("aas://pair?")) return null
            val query = trimmed.removePrefix("aas://pair?")
            val params = query.split('&').mapNotNull { part ->
                val eq = part.indexOf('=')
                if (eq <= 0) null else part.substring(0, eq) to URLDecoder.decode(part.substring(eq + 1), "UTF-8")
            }.toMap()
            val url = params["u"]?.takeIf { it.startsWith("ws://") || it.startsWith("wss://") } ?: return null
            val code = params["c"]?.takeIf { it.isNotBlank() } ?: return null
            return PairingLink(url, code, params["n"])
        }
    }
}

/** HTTP endpoints derived from the WebSocket URL (`wss://host/v1/ws` → `https://host/v1/...`). */
data class ServerEndpoints(val wsUrl: String) {
    /** `https://host[:port]` (or `http://` for `ws://`). */
    val httpBase: String = run {
        val uri = URI(wsUrl)
        val scheme = when (uri.scheme) {
            "wss" -> "https"
            "ws" -> "http"
            else -> throw IllegalArgumentException("not a WebSocket URL: $wsUrl")
        }
        val port = if (uri.port == -1) "" else ":${uri.port}"
        val prefix = uri.path.orEmpty().removeSuffix("/").removeSuffix("/v1/ws")
        "$scheme://${uri.host}$port$prefix"
    }

    val pair: String get() = "$httpBase/v1/pair"
    val blobs: String get() = "$httpBase/v1/blobs"

    fun blob(id: BlobId): String = "$httpBase/v1/blobs/$id"
}
