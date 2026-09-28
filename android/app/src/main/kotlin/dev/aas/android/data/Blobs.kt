package dev.aas.android.data

import android.graphics.Bitmap
import android.graphics.BitmapFactory
import android.util.LruCache
import androidx.core.graphics.scale
import dev.aas.android.R
import dev.aas.android.protocol.BlobId
import dev.aas.android.sync.AasHttp
import dev.aas.android.sync.Credentials
import dev.aas.android.sync.HttpApiException
import dev.aas.android.ui.common.UiText
import kotlinx.coroutines.CoroutineDispatcher
import kotlinx.coroutines.sync.Mutex
import kotlinx.coroutines.sync.withLock
import kotlinx.coroutines.withContext
import java.io.File
import java.io.IOException
import kotlin.math.max

/** Why a blob (`GET /v1/blobs/{id}`) could not be read. */
sealed class BlobException(message: String, cause: Throwable? = null) : Exception(message, cause) {
    class NotPaired : BlobException("the device is not paired")

    /**
     * The daemon no longer has it (404): its thread or project was deleted, or no message
     * referenced it for longer than the daemon's grace period (`unreferenced_blob_grace`).
     */
    class Missing(val blobId: BlobId) : BlobException("blob $blobId does not exist")

    /** Larger than the app reads into memory for this use. */
    class TooLarge(val limit: Long) : BlobException("the blob exceeds $limit bytes")

    class Rejected(val error: HttpApiException) : BlobException(error.message ?: error.kind, error)

    class Network(cause: IOException) : BlobException("the download failed: ${cause.message}", cause)

    /** The bytes are not an image Android can decode. */
    class Undecodable(val blobId: BlobId) : BlobException("blob $blobId is not a decodable image")

    /** What the user is told. */
    fun describe(): UiText = when (this) {
        is NotPaired -> UiText.of(R.string.blob_not_paired)
        is Missing -> UiText.of(R.string.blob_missing)
        is TooLarge -> UiText.of(R.string.blob_too_large, limit / BYTES_PER_MIB)
        is Rejected -> UiText.of(R.string.blob_rejected, error.detail.ifEmpty { error.kind })
        is Network -> UiText.of(R.string.blob_network, cause?.message ?: "")
        is Undecodable -> UiText.of(R.string.blob_undecodable)
    }

    private companion object {
        const val BYTES_PER_MIB = 1024L * 1024
    }
}

/**
 * Downloaded blobs on disk (the app's cache directory). Blob ids are content hashes, so an entry
 * never goes stale; the least recently used entries go when the total exceeds [maxBytes].
 */
class BlobCache(private val dir: File, private val maxBytes: Long, private val io: CoroutineDispatcher) {
    private val lock = Mutex()

    suspend fun get(id: BlobId): ByteArray? = withContext(io) {
        lock.withLock {
            val file = fileOf(id) ?: return@withLock null
            if (!file.isFile) return@withLock null
            // Reading marks the entry as recently used.
            file.setLastModified(System.currentTimeMillis())
            file.readBytes()
        }
    }

    suspend fun put(id: BlobId, bytes: ByteArray) = withContext(io) {
        lock.withLock {
            val file = fileOf(id) ?: return@withLock
            if (bytes.size > maxBytes) return@withLock
            if (!dir.isDirectory && !dir.mkdirs()) throw IOException("cannot create the blob cache folder $dir")
            val temp = File(dir, "${file.name}.tmp")
            temp.writeBytes(bytes)
            if (!temp.renameTo(file)) {
                // Another writer stored the same content first (Windows cannot rename over it).
                temp.delete()
            }
            trim()
        }
    }

    /** Bytes held on disk (diagnostics and tests). */
    suspend fun size(): Long = withContext(io) { lock.withLock { entries().sumOf { it.length() } } }

    private fun trim() {
        var total = entries().sumOf { it.length() }
        if (total <= maxBytes) return
        for (file in entries().sortedBy { it.lastModified() }) {
            if (total <= maxBytes) break
            val length = file.length()
            if (file.delete()) total -= length
        }
    }

    private fun entries(): List<File> = dir.listFiles()?.filter { it.isFile && !it.name.endsWith(".tmp") }.orEmpty()

    /** Blob ids are `blb_<hex>`; anything else is not used as a file name. */
    private fun fileOf(id: BlobId): File? = if (SAFE_ID.matches(id)) File(dir, id) else null

    private companion object {
        val SAFE_ID = Regex("^[A-Za-z0-9_-]{1,200}$")
    }
}

/**
 * Reads blobs: from the disk cache, else from the daemon (then cached). A failure to store the
 * offline copy does not fail the read (the bytes are valid); it goes to [onCacheFailure] (the
 * diagnostics log).
 */
class BlobRepository(
    private val http: AasHttp,
    /** This device's credentials, `null` when not paired. */
    private val credentials: suspend () -> Credentials?,
    private val cache: BlobCache,
    private val onCacheFailure: (BlobId, IOException) -> Unit,
) {
    /** The blob's bytes, at most [maxBytes]. */
    suspend fun bytes(id: BlobId, maxBytes: Long): ByteArray {
        val cached = try {
            cache.get(id)
        } catch (e: IOException) {
            onCacheFailure(id, e)
            null
        }
        if (cached != null) {
            if (cached.size > maxBytes) throw BlobException.TooLarge(maxBytes)
            return cached
        }
        val auth = credentials() ?: throw BlobException.NotPaired()
        val bytes = try {
            http.downloadBlob(auth, id, maxBytes)
        } catch (e: HttpApiException) {
            throw when {
                e.status == HTTP_NOT_FOUND || e.kind == "notFound" -> BlobException.Missing(id)
                e.kind == "payloadTooLarge" -> BlobException.TooLarge(maxBytes)
                else -> BlobException.Rejected(e)
            }
        } catch (e: IOException) {
            throw BlobException.Network(e)
        }
        try {
            cache.put(id, bytes)
        } catch (e: IOException) {
            onCacheFailure(id, e)
        }
        return bytes
    }

    /** Text blobs (command output, patches) are UTF-8. */
    suspend fun text(id: BlobId, maxBytes: Long): String = bytes(id, maxBytes).toString(Charsets.UTF_8)

    private companion object {
        const val HTTP_NOT_FOUND = 404
    }
}

/** Decoded images of blobs, kept in memory by size. */
class BlobImages(
    private val blobs: BlobRepository,
    memoryBytes: Int,
    /** The largest image read (the daemon's `maxBlobBytes`: nothing it stored is larger). */
    private val maxImageBytes: () -> Long,
    private val decodeDispatcher: CoroutineDispatcher,
) {
    private val memory = object : LruCache<String, Bitmap>(memoryBytes) {
        override fun sizeOf(key: String, value: Bitmap): Int = value.byteCount
    }

    /** The image scaled so its longest edge is at most [maxEdgePx] (never enlarged). */
    suspend fun load(id: BlobId, maxEdgePx: Int): Bitmap {
        val key = "$id@$maxEdgePx"
        memory.get(key)?.let { return it }
        val bytes = blobs.bytes(id, maxImageBytes())
        val bitmap = withContext(decodeDispatcher) { decode(bytes, maxEdgePx) } ?: throw BlobException.Undecodable(id)
        memory.put(key, bitmap)
        return bitmap
    }

    private fun decode(bytes: ByteArray, maxEdgePx: Int): Bitmap? {
        val bounds = BitmapFactory.Options().apply { inJustDecodeBounds = true }
        BitmapFactory.decodeByteArray(bytes, 0, bytes.size, bounds)
        if (bounds.outWidth <= 0 || bounds.outHeight <= 0) return null
        var sample = 1
        // Power-of-two subsampling down to at least twice the target, then an exact scale.
        while (max(bounds.outWidth, bounds.outHeight) / (sample * 2) >= maxEdgePx) sample *= 2
        val decoded = BitmapFactory.decodeByteArray(bytes, 0, bytes.size, BitmapFactory.Options().apply { inSampleSize = sample }) ?: return null
        val longest = max(decoded.width, decoded.height)
        if (longest <= maxEdgePx) return decoded
        val scale = maxEdgePx.toFloat() / longest
        val scaled = decoded.scale((decoded.width * scale).toInt().coerceAtLeast(1), (decoded.height * scale).toInt().coerceAtLeast(1))
        if (scaled !== decoded) decoded.recycle()
        return scaled
    }
}
