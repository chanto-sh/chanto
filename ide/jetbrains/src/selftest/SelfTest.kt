package dev.chanto.ide

import java.net.InetAddress
import java.net.Socket
import java.util.concurrent.CountDownLatch
import java.util.concurrent.TimeUnit
import java.util.concurrent.atomic.AtomicBoolean

/**
 * Exercises the editor-independent bridge (JSON, framing, JSON-RPC dispatch and
 * the lockfile) without the IntelliJ SDK, so the protocol logic can be verified
 * on any machine with `kotlinc`. Run `./selftest.sh`.
 */

private var failures = 0

private fun verify(name: String, condition: Boolean, detail: () -> String = { "" }) {
    if (condition) {
        println("ok   $name")
    } else {
        failures++
        println("FAIL $name ${detail()}")
    }
}

private fun <T> verifyThrows(name: String, block: () -> T) {
    try {
        block()
        failures++
        println("FAIL $name (no exception)")
    } catch (e: Exception) {
        println("ok   $name")
    }
}

/** Canned answers standing in for a real editor. */
private class FakeIde(private val folders: List<String>) : IdeApi {
    var diffResult: Json = json("accepted" to true, "contents" to "proposed")
    var lastDiffPath: String? = null
    var closedDiffs: String? = "unset"

    override fun getSelection(): Json? = json("file" to "/p/a.kt", "text" to "val x = 1", "startLine" to 3, "endLine" to 3, "isEmpty" to false)
    override fun getOpenEditors(): Json = Json.Arr(listOf(json("file" to "/p/a.kt", "isActive" to true, "isDirty" to false)))
    override fun getWorkspaceFolders(): Json = Json.of(folders)
    override fun getDiagnostics(path: String?): Json =
        if (path == null) Json.Arr(listOf(json("file" to "/p/a.kt", "diagnostics" to emptyList<Any>()))) else Json.Arr(emptyList())
    override fun openFile(path: String, line: Int?, endLine: Int?): Json = json("ok" to true)
    override fun openDiff(path: String, newContents: String, title: String?): Json {
        lastDiffPath = path
        return diffResult
    }
    override fun closeDiffs(path: String?) { closedDiffs = path }
}

private fun jsonRoundTrips() {
    val source = """{"a":[1,2.5,true,null,"x\ny"],"b":{"c":"\u00e9"},"neg":-3,"big":1e3}"""
    val parsed = Json.parse(source)
    verify("json: scalars parse", parsed["neg"]?.asInt == -3 && parsed["big"]?.asDouble == 1000.0) { source }
    verify("json: nesting parses", parsed["a"]?.asArray?.size == 5) { source }
    verify("json: escapes decode", parsed["a"]?.asArray?.get(4)?.asString == "x\ny") { source }
    verify("json: unicode escape decodes", parsed["b"]?.get("c")?.asString == "é") { source }
    verify("json: round trips", Json.parse(parsed.stringify()).stringify() == parsed.stringify()) { parsed.stringify() }
    verify("json: null is distinct", parsed["missing"] == null && parsed["a"]?.asArray?.get(3) === Json.Null)
    verify("json: empty object/array", Json.parse("{}").asObject.isEmpty() && Json.parse("[]").asArray.isEmpty())
    verifyThrows("json: malformed is rejected") { Json.parse("""{"a":}""") }
    verifyThrows("json: trailing junk is rejected") { Json.parse("1 2") }
    // Integers must not gain a decimal point: ids and line numbers go over the wire.
    verify("json: integers stay integral", json("id" to 1, "line" to 42).stringify() == """{"id":1,"line":42}""") { json("id" to 1, "line" to 42).stringify() }
}

private fun handshakeKey() {
    // The worked example from RFC 6455 §1.3.
    verify("websocket: accept key matches RFC 6455", WebSocket.acceptKey("dGhlIHNhbXBsZSBub25jZQ==") == "s3pPLMBiTxaQ9kYGzzhZRbK+xOo=")
}

private fun framing() {
    val small = WebSocket.encodeText("hi", mask = byteArrayOf(1, 2, 3, 4))
    val decoded = WebSocket.readFrame(small.inputStream())
    verify("websocket: short frame round trips", decoded?.opcode == WebSocket.OP_TEXT && String(decoded!!.payload) == "hi")

    val long = "x".repeat(70_000)
    val encoded = WebSocket.encodeText(long, mask = byteArrayOf(9, 8, 7, 6))
    val back = WebSocket.readFrame(encoded.inputStream())
    verify("websocket: extended-length frame round trips", back != null && String(back.payload) == long)

    val unmasked = WebSocket.encodeText("plain")
    val back2 = WebSocket.readFrame(unmasked.inputStream())
    verify("websocket: unmasked (server) frame round trips", String(back2!!.payload) == "plain")
    verify("websocket: clean EOF returns null", WebSocket.readFrame(ByteArray(0).inputStream()) == null)
    verifyThrows("websocket: truncated frame is reported") { WebSocket.readFrame(byteArrayOf(0x81.toByte()).inputStream()) }
}

private fun dispatch() {
    val server = IdeServer(FakeIde(listOf("/p")), "Test IDE")
    fun call(request: String) = server.dispatch(Json.parse(request))

    val selection = call("""{"jsonrpc":"2.0","id":1,"method":"getSelection","params":{}}""")
    verify("dispatch: getSelection returns the result", selection?.get("result")?.get("file")?.asString == "/p/a.kt") { selection.toString() }
    verify("dispatch: the id is echoed", selection?.get("id")?.asInt == 1)

    val folders = call("""{"jsonrpc":"2.0","id":2,"method":"getWorkspaceFolders","params":{}}""")
    verify("dispatch: workspace folders", folders?.get("result")?.asArray?.first()?.asString == "/p")

    val unknown = call("""{"jsonrpc":"2.0","id":3,"method":"nope","params":{}}""")
    verify("dispatch: unknown method is -32601", unknown?.get("error")?.get("code")?.asInt == -32601) { unknown.toString() }

    val missingPath = call("""{"jsonrpc":"2.0","id":4,"method":"openFile","params":{}}""")
    verify("dispatch: a missing argument is -32602", missingPath?.get("error")?.get("code")?.asInt == -32602) { missingPath.toString() }

    val notify = call("""{"jsonrpc":"2.0","method":"selectionChanged","params":{}}""")
    verify("dispatch: notifications produce no reply", notify == null)

    val noMethod = call("""{"jsonrpc":"2.0","id":5,"params":{}}""")
    verify("dispatch: a request with no method is -32600", noMethod?.get("error")?.get("code")?.asInt == -32600) { noMethod.toString() }

    // A handler that throws becomes -32000.
    val boom = object : IdeApi by FakeIde(listOf("/p")) {
        override fun getOpenEditors(): Json = throw IllegalStateException("editor exploded")
    }
    val failing = IdeServer(boom, "Test IDE").dispatch(Json.parse("""{"jsonrpc":"2.0","id":9,"method":"getOpenEditors","params":{}}"""))
    verify("dispatch: a handler failure is -32000", failing?.get("error")?.get("code")?.asInt == -32000) { failing.toString() }
    verify("dispatch: the failure message is passed through", failing?.get("error")?.get("message")?.asString == "editor exploded")

    // openDiff carries its arguments through to the editor.
    val fake = FakeIde(listOf("/p"))
    val diffServer = IdeServer(fake, "Test IDE")
    val diff = diffServer.dispatch(Json.parse("""{"jsonrpc":"2.0","id":10,"method":"openDiff","params":{"path":"/p/a.kt","newContents":"new","title":"Change"}}"""))
    verify("dispatch: openDiff round trips the decision", diff?.get("result")?.get("accepted")?.asBool == true && fake.lastDiffPath == "/p/a.kt") { diff.toString() }
    diffServer.dispatch(Json.parse("""{"jsonrpc":"2.0","id":11,"method":"closeDiffs","params":{"path":"/p/a.kt"}}"""))
    verify("dispatch: closeDiffs reaches the editor", fake.closedDiffs == "/p/a.kt")

    // No file editor open: getSelection answers JSON null, not an error.
    val none = object : IdeApi by FakeIde(listOf("/p")) {
        override fun getSelection(): Json? = null
    }
    val nullSelection = IdeServer(none, "Test IDE").dispatch(Json.parse("""{"jsonrpc":"2.0","id":12,"method":"getSelection","params":{}}"""))
    verify("dispatch: no selection is JSON null", nullSelection?.get("result") === Json.Null) { nullSelection.toString() }
}

private fun endToEnd() {
    val home = System.getenv("CHANTO_HOME")
    verify("e2e: CHANTO_HOME is set for the test", !home.isNullOrBlank())

    val api = FakeIde(listOf("/p/project"))
    val server = IdeServer(api, "IntelliJ IDEA")
    val port = server.start()
    server.writeLockfile(listOf("/p/project"))
    verify("e2e: the server binds a port", port > 0)

    // The lockfile matches the protocol: mode 0600 in a 0700 directory.
    val lock = java.nio.file.Paths.get(home, "ide", "$port.lock")
    verify("e2e: the lockfile exists", java.nio.file.Files.exists(lock))
    val parsed = Json.parse(java.nio.file.Files.readString(lock))
    verify("e2e: the lockfile names the IDE", parsed["ideName"]?.asString == "IntelliJ IDEA")
    verify("e2e: the lockfile has the port and token", parsed["port"]?.asInt == port && parsed["authToken"]?.asString == server.authToken)
    verify("e2e: the lockfile lists workspace folders", parsed["workspaceFolders"]?.asArray?.first()?.asString == "/p/project")
    verify("e2e: the lockfile declares protocol v1", parsed["protocolVersion"]?.asInt == 1)

    // Wrong token: the upgrade is refused.
    val denied = Socket(InetAddress.getLoopbackAddress(), port)
    denied.use {
        it.getOutputStream().write(handshake("k", "wrong-token").toByteArray())
        it.getOutputStream().flush()
        val (line, _) = WebSocket.readHttpRequest(it.getInputStream())
        verify("e2e: a bad token is refused with 401", line.contains("401")) { line }
    }

    // Right token: 101, then a JSON-RPC call over real frames.
    val socket = Socket(InetAddress.getLoopbackAddress(), port)
    socket.use {
        val out = it.getOutputStream()
        out.write(handshake("dGhlIHNhbXBsZSBub25jZQ==", server.authToken).toByteArray())
        out.flush()
        val (statusLine, headers) = WebSocket.readHttpRequest(it.getInputStream())
        verify("e2e: a good token upgrades to 101", statusLine.contains("101")) { statusLine }
        verify("e2e: the accept header is correct", headers["sec-websocket-accept"] == WebSocket.acceptKey("dGhlIHNhbXBsZSBub25jZQ==")) { headers.toString() }

        out.write(WebSocket.encodeText("""{"jsonrpc":"2.0","id":7,"method":"getSelection","params":{}}""", mask = byteArrayOf(4, 3, 2, 1)))
        out.flush()
        val reply = WebSocket.readFrame(it.getInputStream())
        val body = Json.parse(String(reply!!.payload))
        verify("e2e: the request is answered over the socket", body["id"]?.asInt == 7 && body["result"]?.get("file")?.asString == "/p/a.kt") { body.stringify() }

        // A server push (selectionChanged) arrives as a notification.
        server.notify("selectionChanged", json("file" to "/p/a.kt", "text" to "hi", "startLine" to 1, "endLine" to 1, "isEmpty" to false))
        val pushed = Json.parse(String(WebSocket.readFrame(it.getInputStream())!!.payload))
        verify("e2e: notifications are pushed", pushed["method"]?.asString == "selectionChanged" && pushed["id"] == null) { pushed.stringify() }
    }

    server.stop()
    verify("e2e: stopping removes the lockfile", !java.nio.file.Files.exists(lock))
}

/**
 * `openDiff` blocks until the user decides, and Chanto settles it with
 * `closeDiffs` on the same connection. The server must therefore answer
 * requests concurrently, not in lockstep, or the second request is never read.
 */
private fun concurrentDispatch() {
    val entered = CountDownLatch(1)
    val release = CountDownLatch(1)
    val closed = AtomicBoolean(false)
    val api = object : IdeApi by FakeIde(listOf("/p")) {
        override fun openDiff(path: String, newContents: String, title: String?): Json {
            entered.countDown()
            release.await(5, TimeUnit.SECONDS)
            return json("accepted" to false)
        }

        override fun closeDiffs(path: String?) {
            closed.set(true)
        }
    }
    val server = IdeServer(api, "Test IDE")
    val port = server.start()
    server.writeLockfile(listOf("/p"))

    Socket(InetAddress.getLoopbackAddress(), port).use {
        it.soTimeout = 5000
        val out = it.getOutputStream()
        val input = it.getInputStream()
        out.write(handshake("dGhlIHNhbXBsZSBub25jZQ==", server.authToken).toByteArray())
        out.flush()
        WebSocket.readHttpRequest(input)

        out.write(WebSocket.encodeText("""{"jsonrpc":"2.0","id":20,"method":"openDiff","params":{"path":"/p/a.kt","newContents":"n"}}""", mask = byteArrayOf(1, 1, 1, 1)))
        out.flush()
        verify("concurrency: openDiff is in flight", entered.await(5, TimeUnit.SECONDS))

        // Sent while the first request is still blocked: it must still be answered.
        out.write(WebSocket.encodeText("""{"jsonrpc":"2.0","id":21,"method":"closeDiffs","params":{}}""", mask = byteArrayOf(2, 2, 2, 2)))
        out.flush()
        val first = Json.parse(String(WebSocket.readFrame(input)!!.payload))
        verify("concurrency: closeDiffs is answered while openDiff waits", first["id"]?.asInt == 21) { first.stringify() }
        verify("concurrency: closeDiffs reached the editor", closed.get())

        release.countDown()
        val second = Json.parse(String(WebSocket.readFrame(input)!!.payload))
        verify("concurrency: the pending diff still answers", second["id"]?.asInt == 20 && second["result"]?.get("accepted")?.asBool == false) { second.stringify() }
    }
    server.stop()
}

private fun handshake(key: String, token: String): String =
    "GET / HTTP/1.1\r\n" +
        "Host: 127.0.0.1\r\n" +
        "Upgrade: websocket\r\n" +
        "Connection: Upgrade\r\n" +
        "Sec-WebSocket-Key: $key\r\n" +
        "Sec-WebSocket-Version: 13\r\n" +
        "${IdeServer.AUTH_HEADER}: $token\r\n" +
        "\r\n"

fun main() {
    println("chanto JetBrains bridge — self-test")
    jsonRoundTrips()
    handshakeKey()
    framing()
    dispatch()
    endToEnd()
    concurrentDispatch()
    if (failures > 0) {
        println("\n$failures verify(s) failed")
        kotlin.system.exitProcess(1)
    }
    println("\nall checks passed")
}
