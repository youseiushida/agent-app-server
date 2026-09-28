package dev.aas.android.sync

import dev.aas.android.protocol.AasJson
import dev.aas.android.protocol.BlobId
import dev.aas.android.protocol.BlobUploadResponse
import dev.aas.android.protocol.HttpError
import dev.aas.android.protocol.PairRequest
import dev.aas.android.protocol.PairResponse
import dev.aas.android.protocol.ServerEndpoints
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.withContext
import kotlinx.serialization.SerializationException
import okhttp3.MediaType.Companion.toMediaType
import okhttp3.OkHttpClient
import okhttp3.Request
import okhttp3.RequestBody.Companion.toRequestBody
import okhttp3.Response

/**
 * An HTTP endpoint answered with an error (body `{kind, message}`, protocol.md §6). [detail] is
 * the server's `message` (or an excerpt of a non-protocol body) without the kind and status.
 */
class HttpApiException(val status: Int, val kind: String, val detail: String) : Exception("$kind ($status): $detail")

/** The plain HTTP endpoints: pairing and blobs (protocol.md §6). */
class AasHttp(private val http: OkHttpClient) {
    /** `POST /v1/pair`: exchanges a pairing code for a device token. */
    suspend fun pair(wsUrl: String, code: String, deviceName: String, platform: String = "android"): PairResponse {
        val body = AasJson.encodeToString(PairRequest.serializer(), PairRequest(code, deviceName, platform))
        val request = Request.Builder()
            .url(ServerEndpoints(wsUrl).pair)
            .post(body.toRequestBody(JSON))
            .build()
        return execute(request) { AasJson.decodeFromString(PairResponse.serializer(), it.body.string()) }
    }

    /** `POST /v1/blobs`: uploads an image; returns its content-addressed id. */
    suspend fun uploadBlob(credentials: Credentials, bytes: ByteArray, mime: String): BlobUploadResponse {
        val request = Request.Builder()
            .url(ServerEndpoints(credentials.wsUrl).blobs)
            .header("Authorization", "Bearer ${credentials.token}")
            .post(bytes.toRequestBody(mime.toMediaType()))
            .build()
        return execute(request) { AasJson.decodeFromString(BlobUploadResponse.serializer(), it.body.string()) }
    }

    /**
     * `GET /v1/blobs/{id}` (truncated command output, large patches, images), read into memory.
     * A blob larger than [maxBytes] fails with a `payloadTooLarge` [HttpApiException] instead of
     * exhausting the heap.
     */
    suspend fun downloadBlob(credentials: Credentials, id: BlobId, maxBytes: Long = DEFAULT_MAX_BLOB_DOWNLOAD_BYTES): ByteArray {
        val request = Request.Builder()
            .url(ServerEndpoints(credentials.wsUrl).blob(id))
            .header("Authorization", "Bearer ${credentials.token}")
            .get()
            .build()
        return execute(request) { response ->
            val length = response.body.contentLength()
            if (length > maxBytes) throw HttpApiException(HTTP_PAYLOAD_TOO_LARGE, "payloadTooLarge", "blob $id is $length bytes (limit $maxBytes)")
            val source = response.body.source()
            // request(n) is true when at least n bytes are available, i.e. the blob is too large.
            if (source.request(maxBytes + 1)) throw HttpApiException(HTTP_PAYLOAD_TOO_LARGE, "payloadTooLarge", "blob $id exceeds $maxBytes bytes")
            source.buffer.readByteArray()
        }
    }

    private suspend fun <T> execute(request: Request, read: (Response) -> T): T = withContext(Dispatchers.IO) {
        http.newCall(request).execute().use { response ->
            if (!response.isSuccessful) {
                val text = response.body.string()
                val error = try {
                    AasJson.decodeFromString(HttpError.serializer(), text)
                } catch (e: SerializationException) {
                    null
                } catch (e: IllegalArgumentException) {
                    null
                }
                // A body that is not the protocol's error shape (a proxy's page) is reported verbatim.
                throw HttpApiException(response.code, error?.kind ?: "http${response.code}", error?.message ?: text.take(ERROR_BODY_EXCERPT))
            }
            read(response)
        }
    }

    companion object {
        /**
         * Default download limit: above the server's image limit (`max_blob_bytes`, 25 MiB) and
         * large enough for long command outputs, small enough to hold in a phone's heap.
         */
        const val DEFAULT_MAX_BLOB_DOWNLOAD_BYTES: Long = 64L * 1024 * 1024

        /** The status reported for a download over the local limit (the server's own code for "too large"). */
        private const val HTTP_PAYLOAD_TOO_LARGE = 413

        /** Characters of a non-protocol error body kept in the exception message. */
        private const val ERROR_BODY_EXCERPT = 200

        private val JSON = "application/json; charset=utf-8".toMediaType()
    }
}
