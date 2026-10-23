package dev.chanto.ide

import java.net.InetAddress
import java.net.InetSocketAddress
import java.net.ServerSocket
import java.net.Socket
import java.nio.file.Files
import java.nio.file.Path
import java.nio.file.Paths
import java.nio.file.StandardCopyOption
import java.nio.file.attribute.PosixFilePermissions
import java.security.SecureRandom
import java.util.Base64
import java.util.concurrent.ConcurrentHashMap
import java.util.concurrent.Executors
import java.util.concurrent.atomic.AtomicInteger
import kotlin.concurrent.thread

/**
 * The server side of the chanto IDE bridge: a loopback WebSocket speaking
 * JSON-RPC 2.0, plus the lockfile that lets chanto find it.
 *
 * Editor-independent — it only talks to an [IdeApi]. The IntelliJ wiring lives
 * in [IntelliJIde] / [ChantoIdeService].
 */
class IdeServer(
    private val api: IdeApi,
    private val ideName: String,
    private val log: (String) -> Unit = {},
) {
    private var server: ServerSocket? = null
    private val clients = ConcurrentHashMap<Int, Client>()
    private val nextClientId = AtomicInteger()
    private val random = SecureRandom()

    /**
     * Requests are answered off the reader thread: `openDiff` blocks until the
     * user decides, and chanto sends `closeDiffs` on the same connection to
     * settle it, so a synchronous loop would never read that second request.
     */
    private val workers = Executors.newCachedThreadPool { runnable ->
        Thread(runnable, "chanto-ide-request").apply { isDaemon = true }
    }

    val authToken: String = Base64.getUrlEncoder().withoutPadding().encodeToString(ByteArray(32).also { random.nextBytes(it) })

    /** The port the server is listening on, or -1 before [start]. */
    @Volatile
    var port: Int = -1
        private set

    @Volatile
    private var workspaceFolders: List<String> = emptyList()

    /**
     * Listen on loopback and start accepting connections. Returns the bound
     * port. [preferredPort] of 0 asks the OS to pick one.
     */
    fun start(preferredPort: Int = 0): Int {
        val socket = ServerSocket()
        socket.reuseAddress = true
        socket.bind(InetSocketAddress(InetAddress.getLoopbackAddress(), preferredPort))
        server = socket
        port = socket.localPort
        thread(isDaemon = true, name = "chanto-ide-accept") {
            while (!socket.isClosed) {
                val client = try {
                    socket.accept()
                } catch (e: Exception) {
                    break // the server was closed
                }
                thread(isDaemon = true, name = "chanto-ide-client") { handle(client) }
            }
        }
        return port
    }

    /** Stop listening, drop the clients and remove the lockfile. */
    fun stop() {
        runCatching { server?.close() }
        server = null
        clients.values.forEach { it.close() }
        clients.clear()
        workers.shutdownNow()
        runCatching { Files.deleteIfExists(lockFile()) }
        port = -1
    }

    /** Rewrite the lockfile; call again whenever the workspace folders change. */
    fun writeLockfile(folders: List<String>) {
        workspaceFolders = folders
        val dir = lockDir()
        Files.createDirectories(dir)
        restrict(dir, "rwx------")
        val lock = LockInfo(pid = ProcessHandle.current().pid(), port = port, workspaceFolders = folders, ideName = ideName, authToken = authToken)
        val target = lockFile()
        val temp = dir.resolve(".$port.tmp")
        Files.write(temp, lock.toJson().stringify().toByteArray(Charsets.UTF_8))
        restrict(temp, "rw-------")
        Files.move(temp, target, StandardCopyOption.REPLACE_EXISTING)
        restrict(target, "rw-------")
    }

    /** Push a notification (no id) to every connected chanto client. */
    fun notify(method: String, params: Json) {
        val message = json("jsonrpc" to "2.0", "method" to method, "params" to params).stringify()
        clients.values.forEach { it.sendText(message) }
    }

    // ------------------------------------------------------------ internals

    private fun lockDir(): Path {
        val home = System.getenv("CHANTO_HOME")?.takeIf { it.isNotBlank() } ?: Paths.get(System.getProperty("user.home"), ".chanto").toString()
        return Paths.get(home, "ide")
    }

    private fun lockFile(): Path = lockDir().resolve("$port.lock")

    private fun restrict(path: Path, mode: String) {
        runCatching { Files.setPosixFilePermissions(path, PosixFilePermissions.fromString(mode)) }
    }

    /** Answer one JSON-RPC request object; `null` for a notification (no id). */
    fun dispatch(request: Json): Json? {
        val id = request["id"] ?: return null
        val method = request["method"]?.asString
        val params = request["params"] ?: Json.Obj(emptyMap())
        fun ok(result: Json): Json = json("jsonrpc" to "2.0", "id" to id, "result" to result)
        fun error(code: Int, message: String): Json = json("jsonrpc" to "2.0", "id" to id, "error" to json("code" to code, "message" to message))
        if (method == null) return error(-32600, "invalid request: no method")

        return try {
            when (method) {
                "getSelection" -> ok(api.getSelection() ?: Json.Null)
                "getOpenEditors" -> ok(api.getOpenEditors())
                "getWorkspaceFolders" -> ok(api.getWorkspaceFolders())
                "getDiagnostics" -> ok(api.getDiagnostics(params["path"]?.asString))
                "openFile" -> {
                    val path = params["path"]?.asString ?: return error(-32602, "openFile needs a path")
                    ok(api.openFile(path, params["line"]?.asInt, params["endLine"]?.asInt))
                }
                "openDiff" -> {
                    val path = params["path"]?.asString ?: return error(-32602, "openDiff needs a path")
                    ok(api.openDiff(path, params["newContents"]?.asString ?: "", params["title"]?.asString))
                }
                "closeDiffs" -> {
                    api.closeDiffs(params["path"]?.asString)
                    ok(json("ok" to true))
                }
                else -> error(-32601, "unknown method: $method")
            }
        } catch (e: Exception) {
            log("$method failed: ${e.message}")
            error(-32000, e.message ?: e.toString())
        }
    }

    private fun handle(socket: Socket) {
        socket.use { s ->
            val (requestLine, headers) = try {
                WebSocket.readHttpRequest(s.getInputStream())
            } catch (e: Exception) {
                log("handshake failed: ${e.message}")
                return
            }
            val key = headers["sec-websocket-key"]
            val token = headers[AUTH_HEADER]
            if (key == null || token != authToken) {
                val body = "unauthorized"
                val response = "HTTP/1.1 401 Unauthorized\r\nContent-Length: ${body.length}\r\nConnection: close\r\n\r\n$body"
                runCatching { s.getOutputStream().write(response.toByteArray(Charsets.US_ASCII)); s.getOutputStream().flush() }
                log("rejected ${requestLine.trim()} (bad or missing $AUTH_HEADER)")
                return
            }
            WebSocket.writeUpgradeResponse(s.getOutputStream(), key)
            val id = nextClientId.incrementAndGet()
            val client = Client(s)
            clients[id] = client
            try {
                while (true) {
                    val frame = WebSocket.readFrame(s.getInputStream()) ?: break
                    when (frame.opcode) {
                        WebSocket.OP_TEXT -> {
                            val text = String(frame.payload, Charsets.UTF_8)
                            workers.execute {
                                val reply = try {
                                    dispatch(Json.parse(text))
                                } catch (e: Exception) {
                                    log("bad request: ${e.message}")
                                    null
                                }
                                if (reply != null) client.sendText(reply.stringify())
                            }
                        }
                        WebSocket.OP_PING -> client.send(WebSocket.encode(WebSocket.OP_PONG, frame.payload))
                        WebSocket.OP_CLOSE -> {
                            client.send(WebSocket.encode(WebSocket.OP_CLOSE, ByteArray(0)))
                            break
                        }
                    }
                }
            } catch (e: Exception) {
                // The client vanished mid-frame; nothing to do but clean up.
            } finally {
                clients.remove(id)
            }
        }
    }

    private inner class Client(private val socket: Socket) {
        private val out = socket.getOutputStream()
        private val writeLock = Any()

        fun send(bytes: ByteArray) {
            synchronized(writeLock) {
                out.write(bytes)
                out.flush()
            }
        }

        fun sendText(text: String) = send(WebSocket.encodeText(text))

        fun close() {
            runCatching { socket.close() }
        }
    }

    companion object {
        /** The header chanto authenticates with. */
        const val AUTH_HEADER = "x-chanto-ide-authorization"

        /** A fresh random secret for a lockfile. */
        fun newToken(): String {
            val bytes = ByteArray(32)
            SecureRandom().nextBytes(bytes)
            return Base64.getUrlEncoder().withoutPadding().encodeToString(bytes)
        }
    }
}
