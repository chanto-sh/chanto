#!/usr/bin/env python3
"""Anonymize real shell commands from coding-agent transcripts for the
fast-path evaluation set.

    python3 classifier/anonymize.py IN.json OUT.jsonl [VOCAB.json]

IN is a JSON list of commands (strings, or {"cmd": ...}). Each command is
rewritten so the shapes the classifier looks at survive (programs,
subcommands, flags, operators, path *kinds*) while personal content does not:

- heredoc bodies, comments, commit messages, PR titles/bodies -> placeholders
- the user's name, home and project directory names, temp/session dirs,
  hostnames, emails, IPs, private domains, GitHub owners -> generic names
- commands containing anything that looks like a secret are dropped
  entirely (not redacted), as are very long ones.
- finally, every word that isn't common is replaced by a placeholder: a word
  survives only if it appears in the public training data
  (classifier/train.jsonl), is a program name from VOCAB (tldr-pages
  examples with a "page" field), is a SQL keyword, or appears in at least 8
  different projects (by "cwd"). Money amounts and long numbers are masked.
  Program names, flags and everyday words survive; project, file, table and
  host names and topic words don't.

The output is checked again for leftovers before it's written.
"""

import json
import os
import re
import sys

USER = os.path.basename(os.path.expanduser("~"))


def identity_tokens():
    """Words that identify the user: login, git name/email, GitHub login,
    plus any in $ANON_EXTRA (comma-separated). Commands still containing one
    after anonymizing are dropped."""
    import subprocess

    def run(*a):
        try:
            return subprocess.run(a, capture_output=True, text=True, timeout=10).stdout.strip()
        except Exception:
            return ""

    toks = {USER}
    toks.update(run("git", "config", "user.name").split())
    toks.add(run("git", "config", "user.email").split("@")[0])
    toks.add(run("gh", "api", "user", "--jq", ".login"))
    toks.update(t for t in os.environ.get("ANON_EXTRA", "").split(",") if t)
    return {t.lower() for t in toks if len(t) >= 4}


IDENTITY = identity_tokens()
PUBLIC_DOMAINS = {
    "github.com", "api.github.com", "raw.githubusercontent.com", "githubusercontent.com", "gitlab.com", "pypi.org", "files.pythonhosted.org",
    "registry.npmjs.org", "npmjs.com", "www.npmjs.com", "crates.io", "docs.rs", "static.crates.io", "example.com", "localhost", "127.0.0.1",
    "google.com", "www.google.com", "anthropic.com", "docs.anthropic.com", "openai.com", "api.openai.com", "cloudflare.com", "api.cloudflare.com",
    "developers.cloudflare.com", "workers.dev", "pages.dev", "huggingface.co", "docker.io", "ghcr.io", "hub.docker.com", "stackoverflow.com",
    "wikipedia.org", "en.wikipedia.org", "rust-lang.org", "doc.rust-lang.org", "go.dev", "pkg.go.dev", "nodejs.org", "python.org", "docs.python.org",
    "ollama.com", "brew.sh", "sh.rustup.rs", "astral.sh", "vercel.com", "fly.io", "amazonaws.com", "s3.amazonaws.com", "apple.com",
}
SECRET = re.compile(
    r"(sk-[A-Za-z0-9_-]{16,}|sk-ant-|ghp_[A-Za-z0-9]{20,}|github_pat_|gho_[A-Za-z0-9]{20,}|AKIA[0-9A-Z]{16}|xox[abposr]-|AIza[0-9A-Za-z_-]{30,}|"
    r"-----BEGIN [A-Z ]*PRIVATE KEY|eyJ[A-Za-z0-9_-]{10,}\.eyJ|Bearer\s+[A-Za-z0-9._-]{20,}|[?&](token|key|secret|sig|signature)=[^&\s'\"]{8,}|"
    r"(api[_-]?key|token|secret|password|passwd)\s*[=:]\s*['\"]?[A-Za-z0-9/+_.-]{12,})",
    re.I,
)
WORD = re.compile(r"[A-Za-z][A-Za-z0-9]{2,}")
SQL_WORDS = {
    "select", "from", "where", "insert", "into", "update", "set", "delete", "drop", "table", "schema", "database", "alter", "create",
    "truncate", "grant", "revoke", "values", "order", "limit", "group", "join", "and", "not", "null", "count", "distinct", "index", "view",
    "cascade", "returning", "begin", "commit", "rollback", "flushall", "flushdb",
}


def mask_numbers(c):
    """Money amounts and long numbers say more about the data than the command."""
    c = re.sub(r"\$\d[\d,]*(\.\d+)?", "$0", c)
    return re.sub(r"(?<![\w.-])\d{5,}(\.\d+)?(?![\w])", "0", c)
HIGH_ENTROPY = re.compile(r"\b[A-Za-z0-9+/_-]{32,}\b")


def entropy_like(tok):
    # Hex hashes (commit SHAs, digests) are fine; mixed-case+digit blobs aren't.
    if re.fullmatch(r"[0-9a-f]{32,64}", tok) or re.fullmatch(r"[0-9a-f-]{36}", tok):
        return False
    return bool(re.search(r"[a-z]", tok) and re.search(r"[A-Z]", tok) and re.search(r"[0-9]", tok))


def strip_heredocs(cmd):
    out, lines, i = [], cmd.split("\n"), 0
    while i < len(lines):
        line = lines[i]
        out.append(line)
        for d in re.findall(r"<<-?\s*['\"\\]?(\w+)", line):
            body = []
            i += 1
            while i < len(lines) and lines[i].strip() != d:
                body.append(lines[i])
                i += 1
            if i < len(lines):
                out.append("…")
                out.append(lines[i])
        i += 1
    return "\n".join(out)


def public(host):
    h = host.lower().rstrip(".")
    return h in PUBLIC_DOMAINS or h.split(".")[0] in {"github", "gitlab", "bitbucket", "localhost"} or any(h.endswith("." + p) for p in PUBLIC_DOMAINS)


# Generic words that are never personal, so never replaced.
COMMON = {
    "config", "npm", "postgres", "postgresql", "internal", "temporal", "redis", "db", "api", "app", "web", "server", "prod", "production",
    "staging", "dev", "local", "localhost", "worker", "workers", "backend", "frontend", "openai", "anthropic", "repositories", "noop", "main",
    "master", "origin", "docker", "node", "python", "test", "tests", "src", "lib", "pi", "mcp", "github", "gitlab", "..", ".", "actions",
}


class Names:
    """Stable placeholders: the same original always maps to the same name."""

    def __init__(self):
        self.maps = {}

    def get(self, kind, orig):
        if orig.lower() in COMMON:
            return orig
        m = self.maps.setdefault(kind, {})
        if orig not in m:
            m[orig] = f"{kind}{len(m) + 1}"
        return m[orig]


def anonymize(cmd, names):
    if len(cmd) > 4000 or SECRET.search(cmd):
        return None
    if any(entropy_like(t) for t in HIGH_ENTROPY.findall(cmd)):
        return None
    c = strip_heredocs(cmd)
    # Comments (whole-line or trailing) -> placeholder.
    c = re.sub(r"(^|\s)#[^\n]*", lambda m: m.group(1) + "# …", c)
    # Commit messages, PR/issue text.
    c = re.sub(r"""(-m|--message|--title|--body|-t|-b)\s+("([^"\\]|\\.)*"|'[^']*')""", lambda m: f'{m.group(1)} "…"', c)
    c = re.sub(r'(-m|--body|--title)\s+"\$\(cat <<', r'\1 "$(cat <<', c)
    # Home and user.
    c = c.replace(f"/Users/{USER}", "/Users/user").replace(f"/home/{USER}", "/home/user")
    c = re.sub(rf"\b{re.escape(USER)}\b", "user", c)
    # Agent temp/session dirs.
    c = re.sub(r"/(private/)?tmp/claude-\d+/[^\s'\"/]+(/[0-9a-f-]{36})?", "/tmp/session", c)
    c = re.sub(r"\b[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}\b", "00000000-0000-0000-0000-000000000000", c)
    # Project directory names under common code roots.
    c = re.sub(r"(/Users/user/(IdeaProjects|code|src|projects|dev|Documents|Downloads)/)([^\s/'\"]+)", lambda m: m.group(1) + names.get("project", m.group(3)), c)
    c = re.sub(r"(~/(IdeaProjects|code|src|projects|dev)/)([^\s/'\"]+)", lambda m: m.group(1) + names.get("project", m.group(3)), c)
    c = re.sub(r"(\.claude/projects/)[^\s/'\"]+", r"\1project", c)
    # Emails, IPs, user@host targets, private domains.
    c = re.sub(r"\b[\w.+-]+@[\w-]+\.[\w.-]+\b", "user@example.com", c)
    c = re.sub(r"\b(\d{1,3}\.){3}\d{1,3}\b", lambda m: m.group(0) if m.group(0).startswith(("127.", "0.0.0.0")) else "192.0.2.1", c)
    def login(m):
        # Only remote-login targets (ssh/scp/... or user@host:path), not npm@1.2 or decorators.
        before = c[max(0, m.start() - 120):m.start()]
        remote = re.search(r"\b(ssh|scp|rsync|sftp|mosh)\b[^|;&\n]*$", before) or (m.end() < len(c) and c[m.end()] == ":")
        if not remote or public(m.group(2)):
            return m.group(0)
        return f"{m.group(1) if m.group(1) in ('root', 'ubuntu', 'git') else 'user'}@{names.get('host', m.group(2))}"

    c = re.sub(r"\b([\w-]+)@([A-Za-z][\w.-]*)", login, c)

    def domain(m):
        d = m.group(2).lower()
        if d in PUBLIC_DOMAINS or any(d.endswith("." + p) for p in PUBLIC_DOMAINS if p.count(".") >= 1 and p not in ("workers.dev", "pages.dev")):
            return m.group(0)
        return m.group(1) + names.get("site", d) + ".example"

    c = re.sub(r"\b[\w-]+\.([\w-]+\.)?(workers|pages)\.dev\b", lambda m: names.get("site", m.group(0)) + ".workers.dev", c)
    c = re.sub(r"(https?://)([\w.-]+)", domain, c)
    # Bare domains (whois loops, curl without scheme, config values).
    c = re.sub(
        r"(?<![\w./@-])((?:[a-z0-9-]+\.)+(?:com|app|io|net|co|dev|ai|org|sh|so|xyz|me|tech|site|online|store|cloud|link))\b",
        lambda m: m.group(0) if public(m.group(1)) or m.group(1).endswith(".example") else names.get("site", m.group(1)) + ".example",
        c,
        flags=re.I,
    )
    # Bare host names after ssh/scp/rsync (no user@).
    c = re.sub(r"\b(ssh|mosh)((\s+-\S+(\s+\d+)?)*)\s+([A-Za-z][\w.-]*)(?=[\s:'\"]|$)", lambda m: f"{m.group(1)}{m.group(2)} " + (m.group(5) if m.group(5).startswith("host") or public(m.group(5)) else names.get("host", m.group(5))), c)
    # GitHub owners in repo paths and gh -R.
    c = re.sub(r"(github\.com[/:]|repos/|-R\s+|--repo\s+)([\w-]+)/([\w.-]+)", lambda m: f"{m.group(1)}owner/{names.get('repo', m.group(3))}", c)
    low = c.lower()
    if any(t in low for t in IDENTITY):
        return None
    return c


def main():
    src, dst = sys.argv[1], sys.argv[2]
    raw = json.load(open(src))
    names = Names()
    # Your repositories' names can appear on their own (`npx wrangler d1 execute <name>`).
    for root in ("IdeaProjects", "code", "src", "projects", "dev", "repos", "git"):
        d = os.path.expanduser(f"~/{root}")
        if os.path.isdir(d):
            for name in sorted(os.listdir(d)):
                if os.path.isdir(os.path.join(d, name, ".git")) and len(name) >= 4:
                    names.get("project", name)
    seen, out, dropped = set(), [], 0
    for r in raw:
        cmd = r if isinstance(r, str) else r["cmd"]
        a = anonymize(cmd, names)
        if a is None:
            dropped += 1
            continue
        if a in seen:
            continue
        seen.add(a)
        out.append(a)
    # Second pass: names discovered anywhere (hosts, sites, projects) also
    # appear inside other strings (container names, paths): replace them all.
    subs = sorted(((o, n) for kind in ("host", "site", "project", "repo") for o, n in names.maps.get(kind, {}).items() if len(o) >= 4 and o.lower() not in COMMON), key=lambda x: -len(x[0]))
    final = []
    for c in out:
        for o, n in subs:
            # Whole words, and as a prefix/part of identifiers (name-api, NAME_KEY).
            c = re.sub(rf"(?<![A-Za-z0-9]){re.escape(o)}(?![A-Za-z0-9])", n, c, flags=re.I)
        if not any(t in c.lower() for t in IDENTITY):
            final.append(c)
    out = list(dict.fromkeys(final))
    # Last pass: keep only common words.
    project_of = {}
    for r in raw:
        if isinstance(r, dict) and r.get("cwd"):
            m = re.match(r"(/Users/[^/]+|/home/[^/]+)/(?:[^/]+/)?([^/]+)", r["cwd"])
            project_of[r["cmd"]] = m.group(2) if m else r["cwd"]
    spread = {}
    for r in raw:
        cmd = r if isinstance(r, str) else r["cmd"]
        p = project_of.get(cmd, "?")
        for w in set(WORD.findall(cmd.lower())):
            spread.setdefault(w, set()).add(p)
    keep = {w for w, ps in spread.items() if len(ps) >= 8}
    if len(sys.argv) > 3:
        # Program names only (tldr page names), not every word in the examples.
        for e in json.load(open(sys.argv[3])):
            page = e.get("page", "") if isinstance(e, dict) else ""
            keep.update(WORD.findall(page.split("/")[-1].lower()))
    keep.update(SQL_WORDS)
    train = os.path.join(os.path.dirname(os.path.abspath(__file__)), "train.jsonl")
    if os.path.exists(train):
        for line in open(train):
            keep.update(WORD.findall(json.loads(line)["cmd"].lower()))
    keep.update({"user", "example", "session", "tmp", "host", "site", "project", "owner", "repo"})
    placeholders = {}

    def word(m):
        w = m.group(0)
        if w.lower() in keep or re.fullmatch(r"(host|site|project|repo)\d+", w) or w.isdigit():
            return w
        return placeholders.setdefault(w.lower(), f"w{len(placeholders) + 1}")

    out = list(dict.fromkeys(mask_numbers(WORD.sub(word, c)) for c in out))
    with open(dst, "w") as f:
        for c in out:
            f.write(json.dumps({"cmd": c}) + "\n")
    print(f"{len(out)} unique commands written, {dropped} dropped (possible secrets or too long)", file=sys.stderr)


if __name__ == "__main__":
    main()
