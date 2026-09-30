package dev.aas.android.sync

import kotlinx.serialization.json.JsonArray
import kotlinx.serialization.json.JsonObject
import kotlinx.serialization.json.JsonPrimitive
import kotlinx.serialization.json.contentOrNull
import kotlinx.serialization.json.jsonObject
import java.io.File
import java.io.IOException
import java.nio.file.Files
import java.time.Instant
import java.util.concurrent.LinkedBlockingQueue
import java.util.concurrent.TimeUnit
import kotlin.math.abs

/**
 * Drives `aas-test-server` (crates/aas-testkit/src/bin/aas-test-server.rs): the real daemon
 * (engine + transport, fake harness with `aas-dummy-agent` processes) behind a chaos proxy,
 * controlled over stdin and reporting on stdout (design.md §16).
 *
 * Every instance has its own state folder; [close] quits the server (killing it when it does
 * not exit in time) and deletes the folder.
 */
class AasTestServer private constructor(
    val exe: File,
    val stateDir: File,
    private val process: Process,
    private val ownsStateDir: Boolean,
) : AutoCloseable {
    /** The `ready` line: where to connect and with which pre-paired device. */
    data class Ready(
        val wsUrl: String,
        val httpUrl: String,
        val token: String,
        val deviceId: String,
        val pairingCode: String,
        val root: String,
        val epoch: String,
        /** Where the fake harness keeps its native sessions (the CLI's own data, kept across `reset`). */
        val nativeSessionsDir: String,
        /** The project folder of the seeded native sessions ("used on the PC"), under [root]. */
        val nativeProject: String,
        /** The native sessions recorded for [nativeProject], newest first. */
        val nativeSessions: List<NativeSessionInfo>,
    ) {
        val credentials: Credentials get() = Credentials(wsUrl, token)
    }

    /**
     * Daemon policy values for a test (the server's flags; `null`: the daemon's default):
     * `policy.idle_process_ttl`, `background_progress_interval`, `background_stop_confirm_timeout`,
     * `max_inline_output_bytes`.
     */
    data class Policy(
        val idleProcessTtlMs: Long? = null,
        val backgroundProgressMs: Long? = null,
        val backgroundStopConfirmMs: Long? = null,
        val maxInlineOutputBytes: Long? = null,
    ) {
        fun args(): List<String> = buildList {
            idleProcessTtlMs?.let { addAll(listOf("--idle-process-ttl-ms", it.toString())) }
            backgroundProgressMs?.let { addAll(listOf("--background-progress-ms", it.toString())) }
            backgroundStopConfirmMs?.let { addAll(listOf("--background-stop-confirm-ms", it.toString())) }
            maxInlineOutputBytes?.let { addAll(listOf("--max-inline-output-bytes", it.toString())) }
        }
    }

    /** A native session of the fake harness, as the server's ready line lists it. */
    data class NativeSessionInfo(val nativeSessionId: String, val title: String)

    /** A native session recorded with [nativeSession]. */
    data class RecordedSession(val nativeSessionId: String, val cwd: String, val title: String)

    /** Proxy behaviour (`chaos <mode>`). */
    sealed interface Chaos {
        val command: String

        data object Pass : Chaos {
            override val command = "chaos pass"
        }

        /** Drops every current connection; new ones pass. */
        data object Drop : Chaos {
            override val command = "chaos drop"
        }

        /** Accepts but forwards nothing (a dead network path). */
        data object Blackhole : Chaos {
            override val command = "chaos blackhole"
        }

        data class Delay(val ms: Long) : Chaos {
            override val command = "chaos delay $ms"
        }
    }

    private val lines = LinkedBlockingQueue<Line>()
    private val stderr = File(stateDir, STDERR_FILE)

    /** Kills the server when the test JVM exits without [quit] (a test abandoned on timeout). */
    private val killOnExit = Thread({ if (process.isAlive) kill() }, "aas-test-server-kill")

    private sealed interface Line {
        data class Json(val obj: JsonObject) : Line

        data class Garbage(val text: String) : Line

        data object Eof : Line
    }

    /** The latest `ready` line. */
    @Volatile
    lateinit var ready: Ready
        private set

    init {
        Runtime.getRuntime().addShutdownHook(killOnExit)
        val reader = Thread({
            process.inputStream.bufferedReader(Charsets.UTF_8).useLines { seq ->
                for (text in seq) {
                    val line = try {
                        Line.Json(dev.aas.android.protocol.AasJson.parseToJsonElement(text).jsonObject)
                    } catch (e: IllegalArgumentException) {
                        Line.Garbage(text)
                    }
                    lines.put(line)
                }
            }
            lines.put(Line.Eof)
        }, "aas-test-server-stdout")
        reader.isDaemon = true
        reader.start()
    }

    private fun nextLine(timeoutMs: Long): JsonObject {
        val line = lines.poll(timeoutMs, TimeUnit.MILLISECONDS)
            ?: throw AssertionError("aas-test-server printed nothing within $timeoutMs ms\n${stderrTail()}")
        return when (line) {
            is Line.Json -> line.obj
            is Line.Garbage -> throw AssertionError("aas-test-server printed a non-JSON line: ${line.text}\n${stderrTail()}")
            Line.Eof -> throw AssertionError("aas-test-server closed its stdout (exit ${exitCodeOrNull()})\n${stderrTail()}")
        }
    }

    private fun awaitReady(timeoutMs: Long): Ready {
        val obj = nextLine(timeoutMs)
        check(obj.str("event") == "ready") { "expected a ready line, got $obj" }
        return parseReady(obj)
    }

    /**
     * Sends [cmd] and returns every line printed for it; the last is its `ok` line.
     * @throws AssertionError the command failed or printed nothing in time.
     */
    fun command(cmd: String, timeoutMs: Long = STEP_TIMEOUT_MS): List<JsonObject> {
        val stdin = process.outputStream
        stdin.write("$cmd\n".toByteArray(Charsets.UTF_8))
        stdin.flush()
        val out = mutableListOf<JsonObject>()
        while (true) {
            val obj = nextLine(timeoutMs)
            out += obj
            obj.str("event")?.let { event ->
                if (event == "ready") ready = parseReady(obj)
                if ((event == "ok" || event == "error") && obj.str("cmd") == cmd) {
                    if (event == "error") throw AssertionError("aas-test-server: '$cmd' failed: ${obj.str("message")}")
                    return out
                }
            }
        }
    }

    fun chaos(mode: Chaos) {
        command(mode.command)
    }

    /** Graceful daemon restart on the same state (same epoch, URL and token). */
    fun restart(): Ready {
        command("restart")
        return ready
    }

    /** Deletes the database: a new epoch and a newly paired device (new token). */
    fun reset(): Ready {
        command("reset")
        return ready
    }

    /**
     * Records one more native session as if someone had used the CLI on the PC: in [folder]
     * (relative to the root; created when missing), with [prompt] in the fake agent's scenario
     * language (a real line break stands for the command's `\n`).
     */
    fun nativeSession(folder: String, prompt: String): RecordedSession {
        require(!folder.contains(' ')) { "the folder is one word of the command line: $folder" }
        val line = command("native-session $folder ${prompt.replace("\n", "\\n")}").first { it.str("event") == "nativeSession" }
        fun req(key: String) = line.str(key) ?: throw AssertionError("nativeSession line without $key: $line")
        return RecordedSession(req("nativeSessionId"), req("cwd"), req("title"))
    }

    /** A fresh pairing code. */
    /**
     * Marks the native session [nativeSessionId] as held by another process (like a Codex thread
     * open in Codex desktop): resuming it fails as `resumeFailed`, forking it works.
     */
    fun holdSession(nativeSessionId: String) {
        command("hold-session $nativeSessionId")
    }

    /** Ends the hold of [holdSession]. */
    fun releaseSession(nativeSessionId: String) {
        command("release-session $nativeSessionId")
    }

    fun pairingCode(): String =
        command("pairing-code").first { it.str("event") == "pairingCode" }.str("code") ?: throw AssertionError("pairingCode line without code")

    /** Agent processes that recorded themselves (`<state>/agent-pids`, pid → OS creation time). */
    fun recordedAgents(): List<RecordedProcess> {
        val dir = File(stateDir, "agent-pids")
        return dir.listFiles().orEmpty().mapNotNull { f ->
            val pid = f.name.toLongOrNull() ?: return@mapNotNull null
            val created = f.readText().trim().toLongOrNull() ?: return@mapNotNull null
            RecordedProcess(pid, created)
        }
    }

    /** A process an agent recorded: its pid and Windows creation time (FILETIME, 100 ns since 1601). */
    data class RecordedProcess(val pid: Long, val createdFiletime: Long) {
        /** Whether this very process (not a later one reusing the pid) still runs. */
        fun alive(): Boolean {
            val handle = ProcessHandle.of(pid).orElse(null) ?: return false
            if (!handle.isAlive) return false
            val start = handle.info().startInstant().orElse(null) ?: return true
            return abs(start.toEpochMilli() - filetimeToInstant(createdFiletime).toEpochMilli()) <= START_TIME_TOLERANCE_MS
        }
    }

    fun stderrTail(): String {
        if (!stderr.isFile) return "(no stderr)"
        val text = stderr.readText()
        return "--- aas-test-server stderr (tail) ---\n" + text.takeLast(STDERR_TAIL_CHARS)
    }

    private fun exitCodeOrNull(): Int? = if (process.isAlive) null else process.exitValue()

    /**
     * Quits the server (`quit`; killed when it does not exit within [QUIT_TIMEOUT_MS]) and
     * returns its exit code. Idempotent.
     */
    fun quit(): Int {
        if (process.isAlive) {
            try {
                process.outputStream.write("quit\n".toByteArray(Charsets.UTF_8))
                process.outputStream.flush()
                process.outputStream.close()
            } catch (e: IOException) {
                // The process is already going away; waiting below decides.
            }
            if (!process.waitFor(QUIT_TIMEOUT_MS, TimeUnit.MILLISECONDS)) {
                kill()
                return KILLED
            }
        }
        try {
            Runtime.getRuntime().removeShutdownHook(killOnExit)
        } catch (e: IllegalStateException) {
            // The JVM is already shutting down; the hook runs and finds the process gone.
        }
        return process.exitValue()
    }

    private fun kill() {
        // Its agents live in Job Objects that close with it (KILL_ON_JOB_CLOSE).
        process.descendants().forEach { it.destroyForcibly() }
        process.destroyForcibly()
        process.waitFor(QUIT_TIMEOUT_MS, TimeUnit.MILLISECONDS)
    }

    override fun close() {
        quit()
        if (ownsStateDir) deleteWithRetries(stateDir)
    }

    companion object {
        /** Upper bound for one step (start, restart, reset, a chaos command). */
        const val STEP_TIMEOUT_MS = 60_000L

        /** Upper bound for a graceful quit before the process is killed. */
        const val QUIT_TIMEOUT_MS = 30_000L

        /** Exit code reported by [quit] when the process had to be killed. */
        const val KILLED = -1

        private const val STDERR_FILE = "aas-test-server.stderr.log"
        private const val STDERR_TAIL_CHARS = 8_000

        /** Java reports process start times in milliseconds; FILETIME has 100 ns resolution. */
        private const val START_TIME_TOLERANCE_MS = 2L

        /** The executable named by `AAS_TEST_SERVER`, or why it cannot be used. */
        fun locate(): Result<File> {
            val path = System.getenv("AAS_TEST_SERVER")?.takeIf { it.isNotBlank() }
                ?: return Result.failure(IllegalStateException("AAS_TEST_SERVER is not set"))
            val exe = File(path)
            if (!exe.isFile) return Result.failure(IllegalStateException("AAS_TEST_SERVER=$path is not a file"))
            val agent = File(exe.parentFile, if (isWindows()) "aas-dummy-agent.exe" else "aas-dummy-agent")
            if (!agent.isFile) return Result.failure(IllegalStateException("${agent.path} is missing (it must sit next to aas-test-server)"))
            return Result.success(exe)
        }

        /**
         * Starts a server on a new temporary state folder (deleted by [close]) and waits for its
         * ready line. [policy] sets daemon policy values the server exposes as flags.
         */
        fun start(
            exe: File,
            heartbeatMs: Long = 300,
            clientTimeoutMs: Long = 1_500,
            stateDir: File? = null,
            policy: Policy = Policy(),
        ): AasTestServer {
            val dir = stateDir ?: Files.createTempDirectory("aas-test-server-").toFile()
            val stderr = File(dir, STDERR_FILE)
            val process = ProcessBuilder(
                listOf(
                    exe.absolutePath,
                    "--state-dir", dir.absolutePath,
                    "--heartbeat-ms", heartbeatMs.toString(),
                    "--client-timeout-ms", clientTimeoutMs.toString(),
                ) + policy.args(),
            )
                .redirectError(ProcessBuilder.Redirect.appendTo(stderr))
                .start()
            val server = AasTestServer(exe, dir, process, ownsStateDir = stateDir == null)
            try {
                server.ready = server.awaitReady(STEP_TIMEOUT_MS)
            } catch (e: Throwable) {
                server.close()
                throw e
            }
            return server
        }

        private fun isWindows() = System.getProperty("os.name").orEmpty().startsWith("Windows")

        private fun filetimeToInstant(filetime: Long): Instant {
            // FILETIME counts 100 ns intervals since 1601-01-01; Unix time starts 11644473600 s later.
            val unix100ns = filetime - 116_444_736_000_000_000L
            return Instant.ofEpochSecond(unix100ns / 10_000_000, (unix100ns % 10_000_000) * 100)
        }

        private fun parseReady(obj: JsonObject): Ready {
            fun req(key: String) = obj.str(key)?.takeIf { it.isNotEmpty() } ?: throw AssertionError("ready line without $key: $obj")
            val sessions = (obj["nativeSessions"] as? JsonArray ?: throw AssertionError("ready line without nativeSessions: $obj")).map { entry ->
                val session = entry.jsonObject
                NativeSessionInfo(
                    session.str("nativeSessionId") ?: throw AssertionError("native session without an id: $session"),
                    session.str("title") ?: throw AssertionError("native session without a title: $session"),
                )
            }
            return Ready(
                req("wsUrl"), req("httpUrl"), req("token"), req("deviceId"), req("pairingCode"), req("root"), req("epoch"),
                req("nativeSessionsDir"), req("nativeProject"), sessions,
            )
        }

        private fun JsonObject.str(key: String): String? = (this[key] as? JsonPrimitive)?.contentOrNull

        /** Deletes a folder; Windows may hold files of a just-exited process for a moment. */
        private fun deleteWithRetries(dir: File) {
            repeat(DELETE_ATTEMPTS) {
                if (dir.deleteRecursively()) return
                Thread.sleep(DELETE_RETRY_MS)
            }
            System.err.println("could not delete the test state folder $dir")
        }

        private const val DELETE_ATTEMPTS = 20
        private const val DELETE_RETRY_MS = 250L
    }
}
