package dev.chanto.ide

import java.io.ByteArrayOutputStream
import java.io.EOFException
import java.io.InputStream
import java.io.OutputStream
import java.security.MessageDigest
import java.util.Base64

/**
 * Minimal RFC 6455 framing — text frames plus ping/pong/close, which is all the
 * bridge needs. Hand-rolled so the core has no third-party dependency and can
 * be compiled and exercised on its own.
 */
object WebSocket {
    /** The GUID from RFC 6455 §1.3, used to derive `Sec-WebSocket-Accept`. */
    const val GUID = "258EAFA5-E914-47DA-95CA-C5AB0DC85B11"

    const val OP_TEXT = 0x1
    const val OP_CLOSE = 0x8
    const val OP_PING = 0x9
    const val OP_PONG = 0xA

    /** The value for `Sec-WebSocket-Accept` given the client's key. */
    fun acceptKey(clientKey: String): String {
        val sha1 = MessageDigest.getInstance("SHA-1").digest((clientKey + GUID).toByteArray(Charsets.UTF_8))
        return Base64.getEncoder().encodeToString(sha1)
    }

    data class Frame(val opcode: Int, val payload: ByteArray) {
        override fun equals(other: Any?): Boolean = other is Frame && opcode == other.opcode && payload.contentEquals(other.payload)
        override fun hashCode(): Int = 31 * opcode + payload.contentHashCode()
    }

    /** Encode one frame. `mask` (4 bytes) is set by clients, per the spec. */
    fun encode(opcode: Int, payload: ByteArray, mask: ByteArray? = null): ByteArray {
        val out = ByteArrayOutputStream()
        out.write(0x80 or (opcode and 0x0F)) // FIN + opcode
        val maskBit = if (mask != null) 0x80 else 0
        when {
            payload.size < 126 -> out.write(maskBit or payload.size)
            payload.size <= 0xFFFF -> {
                out.write(maskBit or 126)
                out.write((payload.size ushr 8) and 0xFF)
                out.write(payload.size and 0xFF)
            }
            else -> {
                out.write(maskBit or 127)
                for (shift in 7 downTo 0) out.write(((payload.size.toLong() ushr (8 * shift)) and 0xFF).toInt())
            }
        }
        if (mask != null) {
            out.write(mask, 0, 4)
            val masked = ByteArray(payload.size) { (payload[it].toInt() xor mask[it % 4].toInt()).toByte() }
            out.write(masked, 0, masked.size)
        } else {
            out.write(payload, 0, payload.size)
        }
        return out.toByteArray()
    }

    fun encodeText(text: String, mask: ByteArray? = null): ByteArray = encode(OP_TEXT, text.toByteArray(Charsets.UTF_8), mask)

    /** Read one frame; `null` at a clean end of stream. */
    fun readFrame(input: InputStream): Frame? {
        val first = input.read()
        if (first < 0) return null
        val second = input.read()
        if (second < 0) throw EOFException("truncated frame header")
        val opcode = first and 0x0F
        val masked = (second and 0x80) != 0
        var length = (second and 0x7F).toLong()
        if (length == 126L) {
            length = ((readByte(input).toLong() shl 8) or readByte(input).toLong())
        } else if (length == 127L) {
            length = 0
            repeat(8) { length = (length shl 8) or readByte(input).toLong() }
        }
        if (length > Int.MAX_VALUE) throw EOFException("frame too large")
        val mask = if (masked) readExactly(input, 4) else null
        val payload = readExactly(input, length.toInt())
        if (mask != null) for (i in payload.indices) payload[i] = (payload[i].toInt() xor mask[i % 4].toInt()).toByte()
        return Frame(opcode, payload)
    }

    private fun readByte(input: InputStream): Int {
        val b = input.read()
        if (b < 0) throw EOFException("truncated frame")
        return b
    }

    private fun readExactly(input: InputStream, count: Int): ByteArray {
        val buffer = ByteArray(count)
        var read = 0
        while (read < count) {
            val n = input.read(buffer, read, count - read)
            if (n < 0) throw EOFException("truncated frame payload")
            read += n
        }
        return buffer
    }

    /**
     * Read an HTTP request's start line and headers, up to the blank line. Read
     * byte by byte so no part of a following frame is consumed.
     */
    fun readHttpRequest(input: InputStream): Pair<String, Map<String, String>> {
        val text = StringBuilder()
        while (!text.endsWith("\r\n\r\n")) {
            val b = input.read()
            if (b < 0) throw EOFException("connection closed during the handshake")
            text.append(b.toChar())
            if (text.length > 64 * 1024) throw IllegalStateException("handshake too large")
        }
        val lines = text.toString().split("\r\n").filter { it.isNotEmpty() }
        val headers = LinkedHashMap<String, String>()
        for (line in lines.drop(1)) {
            val colon = line.indexOf(':')
            if (colon > 0) headers[line.substring(0, colon).trim().lowercase()] = line.substring(colon + 1).trim()
        }
        return (lines.firstOrNull() ?: "") to headers
    }

    /** Write the 101 Switching Protocols response for a completed handshake. */
    fun writeUpgradeResponse(out: OutputStream, clientKey: String) {
        val response = "HTTP/1.1 101 Switching Protocols\r\n" +
            "Upgrade: websocket\r\n" +
            "Connection: Upgrade\r\n" +
            "Sec-WebSocket-Accept: ${acceptKey(clientKey)}\r\n\r\n"
        out.write(response.toByteArray(Charsets.US_ASCII))
        out.flush()
    }
}
