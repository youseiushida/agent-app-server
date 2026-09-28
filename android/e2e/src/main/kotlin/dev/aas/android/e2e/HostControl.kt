package dev.aas.android.e2e

import org.json.JSONException
import org.json.JSONObject
import java.io.BufferedReader
import java.io.InputStreamReader
import java.io.OutputStreamWriter
import java.net.InetAddress
import java.net.InetSocketAddress
import java.net.Socket

/**
 * The test-only control channel of android/scripts/run-device-tests.ps1 (docs/android.md 23.2):
 * drives the test server's stdin (chaos, restart, pairing codes) and takes screenshots on the
 * host. One request per connection: command lines, an empty line, then the answer's JSON lines
 * until the script closes the connection.
 */
class HostControl(private val port: Int, private val screenshots: Boolean) {
    /** Runs [commands] in order (back to back on the host) and returns every answer line. */
    fun run(vararg commands: String): List<JSONObject> {
        require(commands.isNotEmpty() && commands.none { it.isBlank() || '\n' in it }) { "one command per line: ${commands.toList()}" }
        val lines = Socket().use { socket ->
            socket.connect(InetSocketAddress(InetAddress.getByName(LOOPBACK), port), CONNECT_TIMEOUT_MS)
            socket.soTimeout = READ_TIMEOUT_MS
            val writer = OutputStreamWriter(socket.getOutputStream(), Charsets.UTF_8)
            writer.write(commands.joinToString(separator = "\n", postfix = "\n\n"))
            writer.flush()
            BufferedReader(InputStreamReader(socket.getInputStream(), Charsets.UTF_8)).readLines()
        }
        val answers = lines.filter { it.isNotBlank() }.map { line ->
            try {
                JSONObject(line)
            } catch (e: JSONException) {
                throw AssertionError("the control channel answered a line that is not JSON: $line", e)
            }
        }
        for (command in commands) {
            val end = answers.lastOrNull { it.optString("cmd") == command && it.optString("event") in setOf("ok", "error") }
                ?: throw AssertionError("no answer to '$command' (got $answers)")
            if (end.optString("event") == "error") throw AssertionError("'$command' failed: ${end.optString("message")}")
        }
        return answers
    }

    /** A fresh pairing code (codes are single use). */
    fun pairingCode(): String =
        run("pairing-code").firstOrNull { it.optString("event") == "pairingCode" }?.getString("code")
            ?: throw AssertionError("the server printed no pairing code")

    /** `chaos pass|drop|blackhole|delay <ms>`. */
    fun chaos(vararg modes: String) {
        run(*modes.map { "chaos $it" }.toTypedArray())
    }

    /** Restarts the daemon (the proxy keeps its port) and returns its new ready line. */
    fun restart(): JSONObject =
        run("restart").lastOrNull { it.optString("event") == "ready" } ?: throw AssertionError("the server printed no ready line after restart")

    /** Saves a screenshot of the device on the host as `<name>.png`, when the run keeps screenshots. */
    fun screenshot(name: String) {
        if (screenshots) run("screenshot $name")
    }

    private companion object {
        const val LOOPBACK = "127.0.0.1"

        /** Connecting to adb's reverse socket on the device's loopback. */
        const val CONNECT_TIMEOUT_MS = 10_000

        /**
         * Waiting for an answer: longer than the script's own bound for one server command
         * (`-ServerTimeoutSeconds`, 120 s), so the script's error arrives instead of a timeout.
         */
        const val READ_TIMEOUT_MS = 180_000
    }
}
