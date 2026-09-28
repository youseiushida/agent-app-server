package dev.aas.android.data

import android.content.ContentResolver
import android.graphics.Bitmap
import android.graphics.ImageDecoder
import android.net.Uri
import androidx.core.net.toUri
import dev.aas.android.ImageUploadPolicy
import dev.aas.android.protocol.Attachment
import dev.aas.android.protocol.BlobId
import dev.aas.android.protocol.InputPart
import dev.aas.android.security.CredentialStore
import dev.aas.android.security.PairingState
import dev.aas.android.sync.AasHttp
import dev.aas.android.sync.HttpApiException
import dev.aas.android.sync.SyncEngine
import kotlinx.coroutines.CoroutineDispatcher
import kotlinx.coroutines.withContext
import java.io.ByteArrayOutputStream
import java.io.IOException
import kotlin.math.max
import kotlin.math.roundToInt

/** An image stored on the daemon (`POST /v1/blobs`), ready to be referenced by a message. */
data class UploadedImage(val blobId: BlobId, val mime: String, val size: Long) {
    /** The `turn/start` / `thread/create` input part. */
    val inputPart: InputPart get() = InputPart.Image(blobId)

    /** The attachment as the userMessage item will show it. */
    val attachment: Attachment get() = Attachment.Image(blobId, mime)
}

/** Why an image could not be uploaded. */
sealed class ImageUploadException(message: String, cause: Throwable? = null) : Exception(message, cause) {
    class NotPaired : ImageUploadException("the device is not paired")

    /** The picked content could not be read or decoded as an image. */
    class Unreadable(cause: Throwable) : ImageUploadException("the image could not be read: ${cause.message}", cause)

    /** The file is larger than the app reads into memory. */
    class SourceTooLarge(val limit: Long) : ImageUploadException("the image file exceeds $limit bytes")

    /** Even the smallest re-encoding is above the server's limit. */
    class TooLarge(val limit: Long) : ImageUploadException("the image does not fit in $limit bytes")

    /** The daemon refused the upload (`{kind, message}`, e.g. 413 or 415). */
    class Rejected(val error: HttpApiException) : ImageUploadException(error.message ?: error.kind, error)

    /** The network failed. */
    class Network(cause: IOException) : ImageUploadException("the upload failed: ${cause.message}", cause)
}

/**
 * Uploads images picked in the composer (part 2) to the daemon.
 *
 * * PNG, WebP and GIF within the server's limit are sent unchanged (screenshots keep their
 *   pixels).
 * * JPEG is always re-encoded: that applies the EXIF orientation and drops the metadata (GPS
 *   position, camera serial), which would otherwise reach the agent's model provider.
 * * Other formats (HEIC/HEIF, AVIF, …), and anything above the limit, are decoded and encoded
 *   as JPEG with the long edge at most [ImageUploadPolicy.maxEdgePx], shrinking by
 *   [ImageUploadPolicy.downscaleStep] until it fits.
 *
 * The limit is the server's `maxBlobBytes` from `initialize`, or the daemon's default before
 * the first connection. The daemon deduplicates by content, so retrying an upload is safe.
 */
class BlobUploader(
    private val resolver: ContentResolver,
    private val http: AasHttp,
    private val credentials: CredentialStore,
    private val engine: SyncEngine,
    private val policy: ImageUploadPolicy,
    private val io: CoroutineDispatcher,
) : ImageUploader {
    /** [uri] is a content URI from the photo picker (as a string). */
    override suspend fun upload(uri: String): UploadedImage = upload(uri.toUri())

    suspend fun upload(uri: Uri): UploadedImage {
        val pairing = (credentials.current() as? PairingState.Paired)?.pairing ?: throw ImageUploadException.NotPaired()
        val limit = engine.status.value.policy?.maxBlobBytes ?: policy.fallbackMaxBlobBytes
        val (bytes, mime) = withContext(io) { prepare(uri, limit) }
        val response = try {
            http.uploadBlob(pairing.credentials, bytes, mime)
        } catch (e: HttpApiException) {
            throw ImageUploadException.Rejected(e)
        } catch (e: IOException) {
            throw ImageUploadException.Network(e)
        }
        return UploadedImage(response.blobId, response.mime, response.size)
    }

    /** Reads the image and decides how to send it (see the class documentation). */
    private fun prepare(uri: Uri, limit: Long): Pair<ByteArray, String> {
        val mime = resolver.getType(uri)?.lowercase()
        val original = readBounded(uri)
        return when (val plan = UploadPlan.decide(mime, original.size.toLong(), limit)) {
            is UploadPlan.SendAsIs -> original to plan.mime
            UploadPlan.Reencode -> reencode(uri, limit)
        }
    }

    private fun readBounded(uri: Uri): ByteArray {
        try {
            val stream = resolver.openInputStream(uri) ?: throw ImageUploadException.Unreadable(IOException("no content for $uri"))
            stream.use { input ->
                val out = ByteArrayOutputStream()
                val buffer = ByteArray(COPY_BUFFER_BYTES)
                var total = 0L
                while (true) {
                    val n = input.read(buffer)
                    if (n < 0) break
                    total += n
                    if (total > policy.maxSourceBytes) throw ImageUploadException.SourceTooLarge(policy.maxSourceBytes)
                    out.write(buffer, 0, n)
                }
                return out.toByteArray()
            }
        } catch (e: IOException) {
            throw ImageUploadException.Unreadable(e)
        } catch (e: SecurityException) {
            // The picker's grant was revoked.
            throw ImageUploadException.Unreadable(e)
        }
    }

    private fun reencode(uri: Uri, limit: Long): Pair<ByteArray, String> {
        var edge = policy.maxEdgePx
        while (true) {
            val bitmap = decode(uri, edge)
            val out = ByteArrayOutputStream()
            try {
                bitmap.compress(Bitmap.CompressFormat.JPEG, policy.jpegQuality, out)
            } finally {
                bitmap.recycle()
            }
            if (out.size() <= limit) return out.toByteArray() to MIME_JPEG
            val next = (edge * policy.downscaleStep).roundToInt()
            if (next < policy.minEdgePx) throw ImageUploadException.TooLarge(limit)
            edge = next
        }
    }

    private fun decode(uri: Uri, maxEdge: Int): Bitmap = try {
        ImageDecoder.decodeBitmap(ImageDecoder.createSource(resolver, uri)) { decoder, info, _ ->
            val longest = max(info.size.width, info.size.height)
            if (longest > maxEdge) {
                val scale = maxEdge.toFloat() / longest
                decoder.setTargetSize((info.size.width * scale).roundToInt().coerceAtLeast(1), (info.size.height * scale).roundToInt().coerceAtLeast(1))
            }
            // A software bitmap can be compressed; hardware bitmaps cannot be read back.
            decoder.allocator = ImageDecoder.ALLOCATOR_SOFTWARE
        }
    } catch (e: IOException) {
        throw ImageUploadException.Unreadable(e)
    }

    private companion object {
        const val MIME_JPEG = "image/jpeg"
        const val COPY_BUFFER_BYTES = 64 * 1024
    }
}

/** Uploads a picked image: what the composer needs of [BlobUploader] (tests substitute it). */
fun interface ImageUploader {
    /** [uri] is the picked content URI as a string. */
    suspend fun upload(uri: String): UploadedImage
}

/** How an image is sent: the decision of [BlobUploader], separated for tests. */
sealed interface UploadPlan {
    /** The original bytes, with their [mime] type. */
    data class SendAsIs(val mime: String) : UploadPlan

    /** Decoded and encoded as JPEG (see [BlobUploader]). */
    data object Reencode : UploadPlan

    companion object {
        /** Formats the daemon accepts that are sent without re-encoding when small enough. */
        val PASS_THROUGH = setOf("image/png", "image/webp", "image/gif")

        fun decide(mime: String?, size: Long, limit: Long): UploadPlan =
            if (mime != null && mime in PASS_THROUGH && size <= limit) SendAsIs(mime) else Reencode
    }
}
