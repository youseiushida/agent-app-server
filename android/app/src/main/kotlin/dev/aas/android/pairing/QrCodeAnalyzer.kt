package dev.aas.android.pairing

import androidx.camera.core.ImageAnalysis
import androidx.camera.core.ImageProxy
import com.google.zxing.BarcodeFormat
import com.google.zxing.BinaryBitmap
import com.google.zxing.ChecksumException
import com.google.zxing.DecodeHintType
import com.google.zxing.FormatException
import com.google.zxing.NotFoundException
import com.google.zxing.PlanarYUVLuminanceSource
import com.google.zxing.common.HybridBinarizer
import com.google.zxing.qrcode.QRCodeReader

/**
 * Decodes QR codes from CameraX frames with ZXing (core only, no Play services). Only the
 * luminance plane (Y of YUV_420_888) is read; QR finder patterns are found at any rotation, so
 * the frame is not rotated. [onText] is called on the analysis thread for every decoded frame;
 * the caller decides what to keep.
 */
class QrCodeAnalyzer(private val onText: (String) -> Unit) : ImageAnalysis.Analyzer {
    private val reader = QRCodeReader()
    private val hints = mapOf(DecodeHintType.POSSIBLE_FORMATS to listOf(BarcodeFormat.QR_CODE))

    override fun analyze(image: ImageProxy) {
        try {
            val plane = image.planes[0]
            val buffer = plane.buffer
            val data = ByteArray(buffer.remaining())
            buffer.get(data)
            // rowStride may exceed the width (padding): the source reads each row at its stride.
            val source = PlanarYUVLuminanceSource(data, plane.rowStride, image.height, 0, 0, image.width, image.height, false)
            val result = reader.decode(BinaryBitmap(HybridBinarizer(source)), hints)
            onText(result.text)
        } catch (e: NotFoundException) {
            // No QR code in this frame: the normal case until the user points the camera at one.
        } catch (e: ChecksumException) {
            // A blurred or partial code; one of the next frames decodes it.
        } catch (e: FormatException) {
            // Same as above: a misread frame, not an error of the code.
        } finally {
            reader.reset()
            image.close()
        }
    }
}
