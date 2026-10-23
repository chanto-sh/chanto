package dev.chanto.ide

/**
 * A tiny JSON tree — enough for the IDE bridge and no dependencies, so the
 * editor-independent core compiles and runs on its own (see `selftest.sh`).
 */
sealed class Json {
    object Null : Json()
    data class Bool(val value: Boolean) : Json()
    data class Num(val value: Double) : Json()
    data class Str(val value: String) : Json()
    data class Arr(val items: List<Json>) : Json()
    data class Obj(val fields: Map<String, Json>) : Json()

    val asString: String? get() = (this as? Str)?.value
    val asBool: Boolean? get() = (this as? Bool)?.value
    val asInt: Int? get() = (this as? Num)?.value?.toInt()
    val asDouble: Double? get() = (this as? Num)?.value
    val asArray: List<Json> get() = (this as? Arr)?.items ?: emptyList()
    val asObject: Map<String, Json> get() = (this as? Obj)?.fields ?: emptyMap()

    operator fun get(key: String): Json? = (this as? Obj)?.fields?.get(key)

    /** Serialize back to a compact JSON string. */
    fun stringify(): String = StringBuilder().also { write(this, it) }.toString()

    companion object {
        fun parse(text: String): Json {
            val parser = Parser(text)
            val value = parser.value()
            parser.skipWhitespace()
            if (parser.index != text.length) parser.fail("trailing characters")
            return value
        }

        /** Convert ordinary Kotlin values (maps, lists, strings, numbers…) to a tree. */
        fun of(value: Any?): Json = when (value) {
            null -> Null
            is Json -> value
            is Boolean -> Bool(value)
            is Int -> Num(value.toDouble())
            is Long -> Num(value.toDouble())
            is Double -> Num(value)
            is Float -> Num(value.toDouble())
            is String -> Str(value)
            is Map<*, *> -> Obj(value.entries.associate { it.key.toString() to of(it.value) })
            is Iterable<*> -> Arr(value.map { of(it) })
            is Array<*> -> Arr(value.map { of(it) })
            else -> Str(value.toString())
        }

        private fun write(value: Json, out: StringBuilder) {
            when (value) {
                is Null -> out.append("null")
                is Bool -> out.append(value.value)
                is Num -> {
                    val d = value.value
                    // Integers render without a decimal point (ports, lines, ids).
                    if (d == Math.floor(d) && !d.isInfinite() && Math.abs(d) < 1e15) out.append(d.toLong()) else out.append(d)
                }
                is Str -> escape(value.value, out)
                is Arr -> {
                    out.append('[')
                    value.items.forEachIndexed { i, item -> if (i > 0) out.append(','); write(item, out) }
                    out.append(']')
                }
                is Obj -> {
                    out.append('{')
                    var first = true
                    for ((k, v) in value.fields) {
                        if (!first) out.append(',')
                        first = false
                        escape(k, out); out.append(':'); write(v, out)
                    }
                    out.append('}')
                }
            }
        }

        private fun escape(text: String, out: StringBuilder) {
            out.append('"')
            for (c in text) {
                when (c) {
                    '"' -> out.append("\\\"")
                    '\\' -> out.append("\\\\")
                    '\n' -> out.append("\\n")
                    '\r' -> out.append("\\r")
                    '\t' -> out.append("\\t")
                    '\b' -> out.append("\\b")
                    '\u000C' -> out.append("\\f")
                    else -> if (c < ' ') out.append("\\u%04x".format(c.code)) else out.append(c)
                }
            }
            out.append('"')
        }
    }

    private class Parser(val text: String) {
        var index = 0

        fun skipWhitespace() {
            while (index < text.length && text[index].isWhitespace()) index++
        }

        fun fail(message: String): Nothing = throw IllegalArgumentException("$message at offset $index")

        private fun expect(word: String) {
            if (!text.startsWith(word, index)) fail("expected $word")
            index += word.length
        }

        fun value(): Json {
            skipWhitespace()
            if (index >= text.length) fail("unexpected end of input")
            return when (text[index]) {
                '{' -> obj()
                '[' -> arr()
                '"' -> Str(string())
                't' -> { expect("true"); Bool(true) }
                'f' -> { expect("false"); Bool(false) }
                'n' -> { expect("null"); Null }
                else -> number()
            }
        }

        private fun obj(): Json {
            index++ // {
            val map = LinkedHashMap<String, Json>()
            skipWhitespace()
            if (index < text.length && text[index] == '}') { index++; return Obj(map) }
            while (true) {
                skipWhitespace()
                if (index >= text.length || text[index] != '"') fail("expected a key")
                val key = string()
                skipWhitespace()
                if (index >= text.length || text[index] != ':') fail("expected ':'")
                index++
                map[key] = value()
                skipWhitespace()
                when {
                    index >= text.length -> fail("unterminated object")
                    text[index] == ',' -> index++
                    text[index] == '}' -> { index++; return Obj(map) }
                    else -> fail("expected ',' or '}'")
                }
            }
        }

        private fun arr(): Json {
            index++ // [
            val items = ArrayList<Json>()
            skipWhitespace()
            if (index < text.length && text[index] == ']') { index++; return Arr(items) }
            while (true) {
                items.add(value())
                skipWhitespace()
                when {
                    index >= text.length -> fail("unterminated array")
                    text[index] == ',' -> index++
                    text[index] == ']' -> { index++; return Arr(items) }
                    else -> fail("expected ',' or ']'")
                }
            }
        }

        private fun string(): String {
            index++ // opening quote
            val sb = StringBuilder()
            while (true) {
                if (index >= text.length) fail("unterminated string")
                val c = text[index++]
                when {
                    c == '"' -> return sb.toString()
                    c == '\\' -> {
                        if (index >= text.length) fail("unterminated escape")
                        when (val e = text[index++]) {
                            '"' -> sb.append('"')
                            '\\' -> sb.append('\\')
                            '/' -> sb.append('/')
                            'b' -> sb.append('\b')
                            'f' -> sb.append('\u000C')
                            'n' -> sb.append('\n')
                            'r' -> sb.append('\r')
                            't' -> sb.append('\t')
                            'u' -> {
                                if (index + 4 > text.length) fail("bad \\u escape")
                                sb.append(text.substring(index, index + 4).toInt(16).toChar())
                                index += 4
                            }
                            else -> fail("bad escape \\$e")
                        }
                    }
                    else -> sb.append(c)
                }
            }
        }

        private fun number(): Json {
            val start = index
            if (index < text.length && text[index] == '-') index++
            while (index < text.length && text[index].isDigit()) index++
            if (index < text.length && text[index] == '.') { index++; while (index < text.length && text[index].isDigit()) index++ }
            if (index < text.length && (text[index] == 'e' || text[index] == 'E')) {
                index++
                if (index < text.length && (text[index] == '+' || text[index] == '-')) index++
                while (index < text.length && text[index].isDigit()) index++
            }
            val token = text.substring(start, index)
            return Num(token.toDoubleOrNull() ?: fail("bad number '$token'"))
        }
    }
}

/** Build a JSON object: `json("id" to 1, "method" to "ping")`. */
fun json(vararg pairs: Pair<String, Any?>): Json = Json.of(pairs.toMap())
