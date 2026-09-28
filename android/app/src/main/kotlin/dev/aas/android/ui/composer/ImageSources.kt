package dev.aas.android.ui.composer

import android.Manifest
import android.content.Context
import android.content.pm.PackageManager
import android.net.Uri
import androidx.activity.compose.rememberLauncherForActivityResult
import androidx.activity.result.PickVisualMediaRequest
import androidx.activity.result.contract.ActivityResultContracts
import androidx.compose.runtime.Composable
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.saveable.rememberSaveable
import androidx.compose.runtime.setValue
import androidx.compose.ui.platform.LocalContext
import androidx.core.content.ContextCompat
import androidx.core.content.FileProvider
import dev.aas.android.R
import dev.aas.android.ui.common.LocalAppContainer
import dev.aas.android.ui.common.UiText
import java.io.File
import java.io.IOException
import java.util.UUID

/** Where the composer's images come from: the photo picker, or the camera (UX §8.2 入力). */
class ImageSources(val pickFromGallery: () -> Unit, val takePhoto: () -> Unit)

/**
 * The photo picker (no permission needed) and the camera. The camera writes into the app's
 * cache ([CaptureFiles]) through its FileProvider; the app declares the CAMERA permission (for
 * the pairing QR code), so Android requires it granted before another app may take the picture.
 * [onPicked] receives content URIs as strings.
 */
@Composable
fun rememberImageSources(maxItems: Int, onPicked: (List<String>) -> Unit): ImageSources {
    val context = LocalContext.current
    val messages = LocalAppContainer.current.userMessages
    var pendingCapture by rememberSaveable { mutableStateOf<String?>(null) }
    val gallery = rememberLauncherForActivityResult(ActivityResultContracts.PickMultipleVisualMedia(maxItems)) { uris ->
        if (uris.isNotEmpty()) onPicked(uris.map { it.toString() })
    }
    val camera = rememberLauncherForActivityResult(ActivityResultContracts.TakePicture()) { saved ->
        val uri = pendingCapture
        pendingCapture = null
        if (saved && uri != null) onPicked(listOf(uri))
    }
    fun capture() {
        val uri = try {
            CaptureFiles.newCapture(context)
        } catch (e: IOException) {
            messages.show(UiText.of(R.string.camera_file_failed, e.message ?: e.javaClass.simpleName))
            return
        }
        pendingCapture = uri.toString()
        camera.launch(uri)
    }
    val permission = rememberLauncherForActivityResult(ActivityResultContracts.RequestPermission()) { granted ->
        if (granted) capture() else messages.show(UiText.of(R.string.camera_denied))
    }
    return remember(gallery, camera, permission) {
        ImageSources(
            pickFromGallery = { gallery.launch(PickVisualMediaRequest(ActivityResultContracts.PickVisualMedia.ImageOnly)) },
            takePhoto = {
                if (ContextCompat.checkSelfPermission(context, Manifest.permission.CAMERA) == PackageManager.PERMISSION_GRANTED) {
                    capture()
                } else {
                    permission.launch(Manifest.permission.CAMERA)
                }
            },
        )
    }
}

/**
 * Photos taken for a message, in the app's cache. They are only needed until the upload (the
 * daemon keeps the blob) and drafts do not outlive the process, so the folder is emptied when
 * the process starts ([clear]).
 */
object CaptureFiles {
    private const val DIR = "captures"

    /** The FileProvider authority (AndroidManifest.xml, res/xml/file_paths.xml). */
    fun authority(context: Context): String = "${context.packageName}.files"

    fun newCapture(context: Context): Uri {
        val dir = File(context.cacheDir, DIR)
        if (!dir.isDirectory && !dir.mkdirs()) throw IOException("cannot create $dir")
        val file = File(dir, "${UUID.randomUUID()}.jpg")
        if (!file.createNewFile()) throw IOException("cannot create $file")
        return FileProvider.getUriForFile(context, authority(context), file)
    }

    /** Deletes the captures of an earlier process; returns the files that could not be deleted. */
    fun clear(context: Context): List<File> =
        File(context.cacheDir, DIR).listFiles()?.filter { !it.delete() }.orEmpty()
}
