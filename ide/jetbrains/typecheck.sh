#!/bin/sh
# Type-check the IntelliJ-facing sources (IntelliJIde, ChantoIdeService,
# ChantoActions) against a locally installed IntelliJ platform. This is the
# fast check while working; `./gradlew buildPlugin` is the real build and runs
# the plugin verifier too.
#
# Skips cleanly when no IDE is installed. Point CHANTO_IDEA_HOME at the
# ".app/Contents" directory to use a specific installation.
set -e
cd "$(dirname "$0")"

if ! command -v kotlinc >/dev/null 2>&1; then
    echo "kotlinc not found; install the Kotlin compiler to type-check the plugin" >&2
    exit 127
fi

find_idea() {
    for candidate in \
        "$CHANTO_IDEA_HOME" \
        "/Applications/IntelliJ IDEA.app/Contents" \
        "/Applications/IntelliJ IDEA Ultimate.app/Contents" \
        "/Applications/IntelliJ IDEA Community Edition.app/Contents" \
        "$HOME/Applications/IntelliJ IDEA.app/Contents" \
        "$HOME/Library/Application Support/JetBrains/Toolbox/apps/IDEA-U/ch-0"/*/IntelliJ\ IDEA.app/Contents \
        "$HOME/Library/Application Support/JetBrains/Toolbox/apps/IDEA-C/ch-0"/*/IntelliJ\ IDEA.app/Contents
    do
        if [ -n "$candidate" ] && [ -d "$candidate/lib" ]; then
            printf '%s\n' "$candidate"
            return 0
        fi
    done
    return 1
}

IDEA="$(find_idea || true)"
if [ -z "$IDEA" ]; then
    echo "no local IntelliJ installation found; skipping the platform type-check"
    echo "(set CHANTO_IDEA_HOME=$PWD-style path to '.app/Contents', or use ./gradlew buildPlugin)"
    exit 0
fi

TERMINAL="$IDEA/plugins/terminal/lib"
if [ ! -d "$TERMINAL" ]; then
    echo "the IntelliJ installation at $IDEA has no bundled terminal plugin; skipping" >&2
    exit 0
fi

WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT

CP="$(ls "$IDEA"/lib/*.jar "$TERMINAL"/*.jar "$TERMINAL"/modules/*.jar 2>/dev/null | tr '\n' ':')"
echo "Type-checking src/main/kotlin against $IDEA"
kotlinc -nowarn -classpath "$CP" -d "$WORK/classes" src/main/kotlin/dev/chanto/ide/*.kt
echo "ok   platform sources type-check against $(basename "$IDEA")"
