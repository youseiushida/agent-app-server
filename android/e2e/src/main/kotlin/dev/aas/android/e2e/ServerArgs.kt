package dev.aas.android.e2e

import android.os.Bundle
import android.util.Base64
import androidx.test.platform.app.InstrumentationRegistry
import org.junit.AssumptionViolatedException

/**
 * The real daemon of this run, from the instrumentation arguments that
 * android/scripts/run-device-tests.ps1 passes (docs/android.md 23.2).
 */
data class ServerArgs(
    /** `ws://127.0.0.1:<port>/v1/ws`: the test server's chaos proxy, reversed to the host by adb. */
    val wsUrl: String,
    val httpUrl: String,
    /** The pairing code of the ready line (single use; tests ask the control channel for more). */
    val pairingCode: String,
    /** The project root on the PC (a Windows path). */
    val root: String,
    /** The script's control channel on the device's loopback (reversed to the host). */
    val controlPort: Int,
    /** Whether the script saves screenshots (`-ScreenshotDir`). */
    val screenshots: Boolean,
) {
    companion object {
        /** The server of this run, or `null` when the suite runs without the script. */
        fun fromInstrumentation(arguments: Bundle = InstrumentationRegistry.getArguments()): ServerArgs? {
            val wsUrl = arguments.getString("wsUrl") ?: return null
            fun required(name: String) = arguments.getString(name) ?: throw IllegalArgumentException("instrumentation argument $name is missing (run android/scripts/run-device-tests.ps1)")
            return ServerArgs(
                wsUrl = wsUrl,
                httpUrl = required("httpUrl"),
                pairingCode = required("pairingCode"),
                // base64url: the device shell that runs `am instrument` would eat a Windows path's backslashes.
                root = String(Base64.decode(required("root"), Base64.URL_SAFE or Base64.NO_PADDING or Base64.NO_WRAP), Charsets.UTF_8),
                controlPort = required("controlPort").toInt(),
                screenshots = arguments.getString("screenshots") == "true",
            )
        }

        /** The server of this run; skips the test when the suite runs without the script. */
        fun require(): ServerArgs =
            fromInstrumentation() ?: throw AssumptionViolatedException("no test server: run android/scripts/run-device-tests.ps1 (docs/android.md 23.2)")
    }
}
