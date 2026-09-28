package dev.aas.android.pairing

import dev.aas.android.protocol.PairingLink
import org.junit.Test
import kotlin.test.assertEquals
import kotlin.test.assertIs

class PairingParserTest {
    /** Release policy: no cleartext anywhere. */
    private val release = PairingParser { false }

    /** Debug policy: the emulator host and loopback only. */
    private val debug = PairingParser { host -> host in setOf("10.0.2.2", "127.0.0.1", "localhost") }

    private fun ok(result: PairingParseResult) = assertIs<PairingParseResult.Ok>(result).target

    private fun invalid(result: PairingParseResult) = assertIs<PairingParseResult.Invalid>(result).error

    @Test
    fun parsesTheDaemonsQrPayload() {
        // The exact string of aas_protocol::http::pair_url's test (percent-encoded components).
        val text = "aas://pair?u=wss%3A%2F%2Fpc.tail-x.ts.net%2Fv1%2Fws&c=ABCD-1234&n=home%20pc"
        val target = ok(release.parseLink(text))
        assertEquals(PairingTarget("wss://pc.tail-x.ts.net/v1/ws", "ABCD-1234", "home pc"), target)
        assertEquals("pc.tail-x.ts.net", target.host)
        // Same reading as the :protocol helper.
        val link = PairingLink.parse(text)!!
        assertEquals(link.wsUrl, target.wsUrl)
        assertEquals(link.code, target.code)
        assertEquals(link.serverName, target.serverName)
    }

    @Test
    fun toleratesWhitespaceCaseAndParameterOrder() {
        val target = ok(release.parseLink("  aas://pair?n=pc&c=abcd-efgh&u=wss%3A%2F%2Fh.ts.net%3A8443%2Fv1%2Fws\n"))
        assertEquals("wss://h.ts.net:8443/v1/ws", target.wsUrl)
        assertEquals("ABCD-EFGH", target.code)
        assertEquals("pc", target.serverName)
    }

    @Test
    fun rejectsOtherCodesAndBrokenPayloads() {
        assertEquals(PairingInputError.NotAPairingLink, invalid(release.parseLink("https://example.com")))
        assertEquals(PairingInputError.NotAPairingLink, invalid(release.parseLink("WIFI:S:home;T:WPA;P:x;;")))
        assertIs<PairingInputError.MalformedEncoding>(invalid(release.parseLink("aas://pair?u=wss%3&c=A")))
        assertEquals(PairingInputError.MissingUrl, invalid(release.parseLink("aas://pair?c=ABCD-1234")))
        assertEquals(PairingInputError.MissingCode, invalid(release.parseLink("aas://pair?u=wss%3A%2F%2Fh%2Fv1%2Fws")))
        assertEquals(PairingInputError.MissingCode, invalid(release.parseLink("aas://pair?u=wss%3A%2F%2Fh%2Fv1%2Fws&c=%20")))
        assertIs<PairingInputError.InvalidUrl>(invalid(release.parseLink("aas://pair?u=https%3A%2F%2Fh%2Fv1%2Fws&c=A")))
        assertIs<PairingInputError.InvalidUrl>(invalid(release.parseLink("aas://pair?u=wss%3A%2F%2F%2Fv1%2Fws&c=A")))
    }

    @Test
    fun cleartextFollowsTheBuildsNetworkPolicy() {
        val emulator = "aas://pair?u=ws%3A%2F%2F10.0.2.2%3A7878%2Fv1%2Fws&c=ABCD-1234&n=dev"
        assertEquals(PairingInputError.CleartextNotAllowed("10.0.2.2"), invalid(release.parseLink(emulator)))
        assertEquals("ws://10.0.2.2:7878/v1/ws", ok(debug.parseLink(emulator)).wsUrl)
        assertEquals(PairingInputError.CleartextNotAllowed("pc.lan"), invalid(debug.parseManual("ws://pc.lan:7878/v1/ws", "ABCD-1234")))
    }

    @Test
    fun manualEntryIsValidatedTheSameWay() {
        val target = ok(release.parseManual(" wss://pc.tail-x.ts.net/v1/ws ", " abcd-1234 "))
        assertEquals(PairingTarget("wss://pc.tail-x.ts.net/v1/ws", "ABCD-1234", null), target)
        assertEquals(PairingInputError.MissingUrl, invalid(release.parseManual(" ", "ABCD")))
        assertEquals(PairingInputError.MissingCode, invalid(release.parseManual("wss://h/v1/ws", "")))
        assertIs<PairingInputError.InvalidUrl>(invalid(release.parseManual("pc.tail-x.ts.net", "ABCD")))
        assertIs<PairingInputError.InvalidUrl>(invalid(release.parseManual("wss://bad host/v1/ws", "ABCD")))
    }
}
