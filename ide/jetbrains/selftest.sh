#!/bin/sh
# Compile and run the editor-independent bridge core (JSON, WebSocket framing,
# JSON-RPC dispatch, lockfile) without the IntelliJ SDK. The plugin build itself
# needs Gradle; this is the part that must work anywhere.
set -e
cd "$(dirname "$0")"

if ! command -v kotlinc >/dev/null 2>&1; then
    echo "kotlinc not found; install the Kotlin compiler to run the bridge self-test" >&2
    exit 127
fi

CHANTO_HOME="$(mktemp -d)"
export CHANTO_HOME
WORK="$(mktemp -d)"
trap 'rm -rf "$CHANTO_HOME" "$WORK"' EXIT

SRC="src/main/kotlin/dev/chanto/ide"
kotlinc -nowarn -include-runtime -d "$WORK/selftest.jar" \
    "$SRC/Json.kt" \
    "$SRC/WebSocket.kt" \
    "$SRC/IdeApi.kt" \
    "$SRC/IdeServer.kt" \
    src/selftest/SelfTest.kt

# The bridge uses java.lang.ProcessHandle (Java 9+), matching what the IntelliJ
# platform requires. Prefer the `kotlin` launcher, which carries a modern JDK.
if command -v kotlin >/dev/null 2>&1; then
    exec kotlin -classpath "$WORK/selftest.jar" dev.chanto.ide.SelfTestKt
fi
exec java -jar "$WORK/selftest.jar"
