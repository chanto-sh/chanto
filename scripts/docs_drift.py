#!/usr/bin/env python3
"""Fail when a command-line flag, subcommand, slash command, config key or tool
isn't in the docs, or the docs name a flag or slash command that no longer exists.

    python3 scripts/docs_drift.py

It compares names only, so it finds what's missing, not what's wrong. Something
left out on purpose goes in scripts/docs_drift_allow.txt, with the reason.
"""

from pathlib import Path
import re
import sys

ROOT = Path(__file__).resolve().parent.parent
DOCS = ["README.md", "docs/*.md"]
ALLOW = ROOT / "scripts/docs_drift_allow.txt"
# The config structs whose fields are keys people set.
CONFIG_STRUCTS = (
    "Config", "EmbeddingsConfig", "OptimizeConfig", "SandboxConfig", "ProviderConfig",
    "PermissionsConfig", "WebConfig", "BrowserConfig", "McpServerConfig", "CompatConfig",
    "LearningConfig", "HookConfig",
)


def struct_body(source: str, name: str) -> str:
    m = re.search(rf"pub struct {name} \{{\n(.*?)^\}}", source, re.S | re.M)
    return m[1] if m else ""


def fields(body: str):
    """(field name, its attributes) of each public field in a struct body."""
    attrs: list[str] = []
    for line in body.splitlines():
        line = line.strip()
        if line.startswith("#["):
            attrs.append(line)
        elif m := re.match(r"pub (\w+):", line):
            yield m[1], " ".join(attrs)
            attrs = []
        elif attrs and not line.startswith("///"):
            attrs[-1] += " " + line


def flags(source: str) -> set[str]:
    """The `--long` flags of `Cli` that `--help` shows."""
    out = set()
    for name, attrs in fields(struct_body(source, "Cli")):
        if "#[arg" not in attrs or "hide = true" in attrs or "skip" in attrs:
            continue
        if m := re.search(r'\blong = "([^"]+)"', attrs):
            out.add("--" + m[1])
        elif re.search(r"\blong\b", attrs):
            out.add("--" + name.replace("_", "-"))
    return out


def subcommands(source: str) -> set[str]:
    """The commands `--help` lists."""
    m = re.search(r'COMMANDS_HELP: &str = concat!\(\s*"\\\nCommands:\n(.*?)"', source, re.S)
    out = set()
    for line in (m[1] if m else "").splitlines():
        if line.startswith("  ") and not line.startswith("   "):
            out.add(line.split()[0])
    return out


def slash_commands(source: str) -> set[str]:
    return set(re.findall(r'CommandInfo \{\s*name: "([^"]+)"', source))


def config_keys(source: str) -> set[str]:
    out = set()
    for struct in CONFIG_STRUCTS:
        for name, attrs in fields(struct_body(source, struct)):
            if "serde(skip)" not in attrs:
                out.add(f"{struct}.{name}")
    return out


def tool_names(sources: list[str]) -> set[str]:
    out = set()
    for source in sources:
        out.update(re.findall(r'fn name\(&self\) -> String \{\s*"([A-Z]\w+)"', source))
    return out


def mentioned(text: str, word: str) -> bool:
    return re.search(rf"(?<![\w-]){re.escape(word)}(?![\w-])", text) is not None


def drift(docs: str, cli: str, commands: str, config: str, tools: list[str], code: str) -> list[str]:
    """What's out of step. `code` is all the Rust, since subcommands parse their
    own flags; a flag the docs give a `chanto` command must appear in it."""
    found = []
    for flag in flags(cli):
        if not mentioned(docs, flag):
            found.append(f"flag {flag}")
    for command in subcommands(cli):
        if not mentioned(docs, command):
            found.append(f"subcommand {command}")
    known = slash_commands(commands)
    for command in known:
        if not mentioned(docs, "/" + command):
            found.append(f"slash {command}")
    for key in config_keys(config):
        if not mentioned(docs, key.split(".")[1]):
            found.append(f"key {key}")
    for tool in tool_names(tools):
        if not mentioned(docs, tool):
            found.append(f"tool {tool}")
    # Only where a command starts, not `reproduce.py --agents chanto --no-install`.
    for line in re.findall(r"(?:^|`)chanto [^`\n#]*", docs, re.M):
        for flag in re.findall(r"(?<![\w-])--[a-z][a-z-]+", line):
            field = flag[2:].replace("-", "_")
            if flag not in code and f"pub {field}:" not in code:
                found.append(f"gone-flag {flag}")
    return sorted(set(found))


def allowed(text: str) -> set[str]:
    out = set()
    for line in text.splitlines():
        entry, _, why = line.partition("#")
        if entry.strip():
            if not why.strip():
                raise SystemExit(f"docs_drift_allow.txt: {line!r} needs a reason after #")
            out.add(" ".join(entry.split()))
    return out


def main() -> int:
    docs = "\n".join(p.read_text() for g in DOCS for p in sorted(ROOT.glob(g)))
    read = lambda p: (ROOT / p).read_text()
    tools = [p.read_text() for p in (ROOT / "src/tools").rglob("*.rs")]
    code = "\n".join(p.read_text() for p in (ROOT / "src").rglob("*.rs"))
    found = drift(
        docs, read("src/lib.rs"), read("src/commands.rs"), read("src/config.rs"), tools, code
    )
    allow = allowed(ALLOW.read_text())
    missing = [f for f in found if f not in allow]
    outdated = sorted(allow - set(found))
    for item in missing:
        print(f"::error::docs drift: {item} (document it, or add it to scripts/docs_drift_allow.txt with why)")
    for item in outdated:
        print(f"::error::docs drift: {item} is allowed but no longer missing; remove it from the allow list")
    if not missing and not outdated:
        print(f"docs drift: none ({len(allow)} left out on purpose)")
    return 1 if missing or outdated else 0


if __name__ == "__main__":
    sys.exit(main())
