package dev.aas.android.pairing

import java.net.URI
import java.net.URISyntaxException
import java.net.URLDecoder

/**
 * What the app needs to pair: the daemon's WebSocket URL, the one-time code and (from a QR code)
 * the server's name. The QR payload is `aas://pair?u=<wss URL>&c=<code>&n=<server name>` with
 * percent-encoded components (docs/protocol.md §6, `aas_protocol::http::pair_url`).
 */
data class PairingTarget(val wsUrl: String, val code: String, val serverName: String?) {
    /** Host of [wsUrl] (for display and the cleartext check). */
    val host: String get() = URI(wsUrl).host
}

/** Why a pairing payload or manual entry was rejected. */
sealed interface PairingInputError {
    /** The text is not an `aas://pair` link (e.g. another QR code). */
    data object NotAPairingLink : PairingInputError

    /** A component is not valid percent-encoding. */
    data class MalformedEncoding(val detail: String) : PairingInputError

    data object MissingUrl : PairingInputError

    /** The URL is not `ws://` / `wss://` with a host. */
    data class InvalidUrl(val url: String) : PairingInputError

    /** `ws://` to a host this build may not reach without TLS (release builds: every host). */
    data class CleartextNotAllowed(val host: String) : PairingInputError

    data object MissingCode : PairingInputError
}

/** Either a [PairingTarget] or why the input was rejected. */
sealed interface PairingParseResult {
    data class Ok(val target: PairingTarget) : PairingParseResult

    data class Invalid(val error: PairingInputError) : PairingParseResult
}

/**
 * Parses pairing input. [cleartextPermitted] says whether a `ws://` URL to a host is allowed;
 * the app passes the platform's network security policy (debug builds allow the emulator host
 * and loopback, release builds nothing), so the answer here matches what OkHttp would enforce.
 */
class PairingParser(private val cleartextPermitted: (host: String) -> Boolean) {
    /** Parses the text of a QR code (or an `aas://pair` link opened from another app). */
    fun parseLink(text: String): PairingParseResult {
        val trimmed = text.trim()
        if (!trimmed.startsWith(PREFIX, ignoreCase = true)) return PairingParseResult.Invalid(PairingInputError.NotAPairingLink)
        val params = HashMap<String, String>()
        for (part in trimmed.substring(PREFIX.length).split('&')) {
            if (part.isEmpty()) continue
            val eq = part.indexOf('=')
            if (eq <= 0) continue
            val key = part.substring(0, eq)
            val value = try {
                URLDecoder.decode(part.substring(eq + 1), "UTF-8")
            } catch (e: IllegalArgumentException) {
                return PairingParseResult.Invalid(PairingInputError.MalformedEncoding(e.message ?: part))
            }
            // The first occurrence wins, like the daemon's own reader of query strings.
            params.putIfAbsent(key, value)
        }
        return validate(params["u"], params["c"], params["n"]?.takeIf { it.isNotBlank() })
    }

    /** Validates a manually entered server URL and code. */
    fun parseManual(url: String, code: String): PairingParseResult = validate(url.trim(), code, null)

    private fun validate(url: String?, code: String?, serverName: String?): PairingParseResult {
        if (url.isNullOrBlank()) return PairingParseResult.Invalid(PairingInputError.MissingUrl)
        val uri = try {
            URI(url.trim())
        } catch (e: URISyntaxException) {
            return PairingParseResult.Invalid(PairingInputError.InvalidUrl(url))
        }
        val scheme = uri.scheme?.lowercase()
        val host = uri.host
        if ((scheme != "ws" && scheme != "wss") || host.isNullOrEmpty()) {
            return PairingParseResult.Invalid(PairingInputError.InvalidUrl(url))
        }
        if (scheme == "ws" && !cleartextPermitted(host)) {
            return PairingParseResult.Invalid(PairingInputError.CleartextNotAllowed(host))
        }
        val normalizedCode = normalizeCode(code.orEmpty())
        if (normalizedCode.isEmpty()) return PairingParseResult.Invalid(PairingInputError.MissingCode)
        return PairingParseResult.Ok(PairingTarget(uri.toString(), normalizedCode, serverName))
    }

    companion object {
        private const val PREFIX = "aas://pair?"

        /**
         * The daemon ignores case, `-` and whitespace in codes (protocol.md §6); the app sends
         * the code as typed apart from surrounding whitespace, upper-cased for display.
         */
        fun normalizeCode(code: String): String = code.trim().uppercase()
    }
}
