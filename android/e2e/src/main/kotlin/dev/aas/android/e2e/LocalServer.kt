package dev.aas.android.e2e

import java.io.BufferedInputStream
import java.io.ByteArrayOutputStream
import java.io.IOException
import java.io.InputStream
import java.net.InetAddress
import java.net.ServerSocket
import java.net.Socket
import java.net.SocketException
import java.util.concurrent.CopyOnWriteArrayList
import java.util.concurrent.LinkedBlockingQueue
import java.util.concurrent.TimeUnit

/**
 * A minimal HTTP/1.1 server on the device's loopback address for the instrumented tests (the app
 * connects to it from its own process). It keeps the test APK free of the app's libraries: the
 * tests are built once for the debug and the R8-processed staging build and must not depend on
 * either's OkHttp. It answers each request with [respond] and records what it received.
 */
class LocalServer(private val respond: (Request) -> Response) : AutoCloseable {
    data class Request(val method: String, val path: String, val headers: Map<String, String>, val body: String) {
        fun header(name: String): String? = headers[name.lowercase()]
    }

    data class Response(val status: Int, val body: String = "", val contentType: String = "application/json")

    private val socket = ServerSocket(0, BACKLOG, InetAddress.getByName(LOOPBACK))
    private val received = LinkedBlockingQueue<Request>()
    val requests = CopyOnWriteArrayList<Request>()
    private val acceptor = Thread({ acceptLoop() }, "local-server").apply {
        isDaemon = true
        start()
    }

    val port: Int get() = socket.localPort

    /** The next request (in arrival order) within [timeoutMs], or a failure. */
    fun nextRequest(timeoutMs: Long): Request =
        received.poll(timeoutMs, TimeUnit.MILLISECONDS) ?: throw AssertionError("no request within $timeoutMs ms (got $requests)")

    private fun acceptLoop() {
        while (!socket.isClosed) {
            val client = try {
                socket.accept()
            } catch (e: SocketException) {
                return // closed
            }
            Thread({ serve(client) }, "local-server-conn").apply {
                isDaemon = true
                start()
            }
        }
    }

    private fun serve(client: Socket) {
        client.use { s ->
            try {
                val input = BufferedInputStream(s.getInputStream())
                val requestLine = readLine(input) ?: return
                val parts = requestLine.split(' ')
                val headers = LinkedHashMap<String, String>()
                while (true) {
                    val line = readLine(input) ?: return
                    if (line.isEmpty()) break
                    val colon = line.indexOf(':')
                    if (colon > 0) headers[line.substring(0, colon).trim().lowercase()] = line.substring(colon + 1).trim()
                }
                val length = headers["content-length"]?.toIntOrNull() ?: 0
                val body = ByteArray(length)
                var read = 0
                while (read < length) {
                    val n = input.read(body, read, length - read)
                    if (n < 0) break
                    read += n
                }
                val request = Request(parts.getOrElse(0) { "" }, parts.getOrElse(1) { "" }, headers, String(body, 0, read, Charsets.UTF_8))
                requests += request
                received.put(request)
                val response = respond(request)
                val bytes = response.body.toByteArray(Charsets.UTF_8)
                val head = "HTTP/1.1 ${response.status} ${reason(response.status)}\r\n" +
                    "Content-Type: ${response.contentType}\r\n" +
                    "Content-Length: ${bytes.size}\r\n" +
                    "Connection: close\r\n\r\n"
                s.getOutputStream().apply {
                    write(head.toByteArray(Charsets.US_ASCII))
                    write(bytes)
                    flush()
                }
            } catch (e: IOException) {
                // The client went away mid-request; the test sees the missing request instead.
            }
        }
    }

    private fun readLine(input: InputStream): String? {
        val line = ByteArrayOutputStream()
        while (true) {
            val b = input.read()
            if (b < 0) return if (line.size() == 0) null else line.toString(Charsets.US_ASCII.name())
            if (b == '\n'.code) return line.toString(Charsets.US_ASCII.name()).trimEnd('\r')
            line.write(b)
        }
    }

    private fun reason(status: Int) = when (status) {
        HTTP_OK -> "OK"
        HTTP_SERVICE_UNAVAILABLE -> "Service Unavailable"
        else -> "Status"
    }

    override fun close() {
        socket.close()
        acceptor.join(CLOSE_JOIN_MS)
    }

    companion object {
        const val LOOPBACK = "127.0.0.1"
        const val HTTP_OK = 200
        const val HTTP_SERVICE_UNAVAILABLE = 503
        private const val BACKLOG = 16
        private const val CLOSE_JOIN_MS = 1_000L
    }
}
