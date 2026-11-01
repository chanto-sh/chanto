# Security model

Chanto is a coding agent: a language model decides which tools to call, and
Chanto runs them on your machine. This document is the project's assurance
case: what we protect, from whom, where the trust boundaries are, and how each
one is defended. To report a problem, see [SECURITY.md](../SECURITY.md).

## What we protect

- **Your machine and files** outside what you asked Chanto to work on.
- **Your credentials:** API keys, cloud credentials, SSH keys, tokens, `.env` values.
- **Your intent:** Chanto should only take consequential actions you allowed,
  either by approving them or by choosing a permission mode that allows them.
- **The software you install:** releases should be the ones this project built.

## Threats

| Threat | Example |
|---|---|
| Prompt injection | A README, web page, issue or dependency file tells the model to run a command or send data somewhere. |
| A mistaken model | The model runs something destructive while trying to help. |
| A malicious repository | Project files try to run code or change settings when you open the folder (hooks, MCP servers, `.claude/` or `.chanto/` config). |
| Other local users and web pages | Something on the same machine, or a site in your browser, reaches a server Chanto runs on localhost. |
| Network attackers | Someone between you and a model provider, the update server or a remote session. |
| A compromised release channel | A tampered download, package or update. |

Out of scope: an attacker who already runs code as your user, and the model
provider itself (Chanto sends it your prompts and the files it reads, as you'd
expect).

## Trust boundaries and defenses

### 1. Model ↔ tools: the permission layer

Every tool call the model makes goes through one decision point before it runs.

- **Permission modes** decide the default: *default* asks before edits and
  before any shell command not known to be read-only; *acceptEdits* allows edits
  inside the project; *plan* allows only read-only actions; *auto* lets a
  separate classifier model judge each command; *bypass* allows everything and
  must be chosen explicitly. No project file can turn on bypass mode, and
  neither can a session file: resuming or importing a session never restores
  bypass, and an imported session keeps none of its permission settings.
- **Shell commands** are parsed before they're judged: split into simple
  commands, with comments, heredoc bodies and escapes read the way bash reads
  them, so text bash treats as data can't hide a command (fixed in v0.1.1).
  Command substitution, process substitution and write redirections always
  need approval. A read-only command stops counting as one when an option
  makes it run a program or write a file (`fd -x`, `find -fprint`,
  `git fetch --upload-pack` or a fetch from a URL instead of a configured
  remote, `git grep -O`, `sort --compress-program`, `rg --pre`, `xxd in out`,
  `tree -o`, `go env -w`, `go build -o`), in auto mode's fast path too
  (`tar --checkpoint-action`, `tar -I`, `git rebase -x`). So does a word
  the shell rewrites before the command sees it (`$'…'` quoting, brace
  expansion).
- **Files** outside the project, protected paths (`.git/`, Chanto's own and
  other agents' settings and hooks, `.mcp.json`, `.husky/`, `.vscode/`
  settings and tasks) and secret files (`.env`, `~/.ssh`, `~/.aws` and
  similar) always need a human, in every mode except bypass. Protected paths
  are checked for the file tools and for the shell writes Chanto can see in a
  command: redirections, `tee`, the destinations of `cp`, `mv`, `ln`,
  `install` and `rsync`, the source of a link, `sed -i`, `dd of=`,
  `curl -o`, `wget -O`, `tar -C`, `go build -o`, `git config` (including
  `--file`) and `git remote add`. Chanto follows `cd`, `pushd`, wrappers such
  as `env -C`, `sudo` and `timeout`, `sh -c` scripts, command substitutions
  and brace expansion. A target the shell computes when it runs (`$var`,
  `$(…)`, `$'…'`) can't be checked, so it asks too. An allow rule that covers
  the command still applies. In auto mode, extracting an archive or applying
  a patch (`tar x`, `unzip`, `patch`, `git apply`, `git am`) asks, because
  its input names the files it writes. Files that a script or program writes
  (an interpreter, a build, `cp -r dir/. .`) rely on the sandbox.
- **The auto-mode classifier** judges an action against what you typed: your
  prompts and the messages you send while the agent works, without the files
  you mention or the context Chanto adds to them. Only input a frontend
  receives from you counts: the TUI, the REPL, the web UI and phone, an ACP
  client's prompt and `-p`. Everything else is shown to the classifier as
  the agent's task, written by the agent, so a model can't approve its own
  action by claiming you asked for it. That covers a subagent's task, goal,
  team and outcome steps, a background run's task and reports, a scheduled
  task, an expanded slash command and a repair prompt. With `optimize.token_classifier`,
  a one-word ALLOW settles an action only when it is the model's most likely
  answer with at least 90% probability, and only a whole `ALLOW` token
  counts; anything less gets the full verdict.
- **MCP tools** always need approval (or an allow rule) outside auto and
  bypass mode, even when a local server marks them read-only
  (`readOnlyHint`): a read-only fetch or search can still send data out.
- **Web fetches** to cloud instance metadata, local files outside the project
  or loopback services need approval, to counter server-side request forgery.
- **Your own allow and deny rules** apply to each simple command, and an allow
  rule never vouches for an attached substitution or redirection.
- **Approval covers the file a path named when it was approved.** Just before
  each call in a batch starts, the paths in its arguments are resolved again;
  if a symlink (or the working directory) changed since approval, the call is
  refused rather than writing or reading wherever the path now points. A shell
  command is judged again too, and refused if it now reaches a secret file or
  is no longer allowed outright. The check runs at the start of the call, so a
  process running concurrently that swaps a link during the call itself
  isn't caught.
- **Text the user didn't type is never run as typed input.** A prompt the
  model scheduled with `CronCreate` (which itself needs approval outside bypass
  mode), the default `/loop` prompt, background reports and messages left
  over from a turn that ended before delivering them go to the model as plain
  text: never run as `!shell`, `/command` or `#note`, and with no `@file`
  attached. A command's arguments reach its `` !`cmd` `` blocks only as
  data (environment variables and positional parameters), so they can't add a
  command.
- **User memory** is loaded in every project, so the model's saves to it need
  approval in every mode except bypass; the prompt shows the description and
  body.

### 2. Commands ↔ the operating system: the sandbox

Shell commands run inside an OS sandbox by default (Seatbelt on macOS,
Landlock and seccomp on Linux, a write-restricted token on Windows):

- writes are limited to the project, temporary directories and package
  managers' download caches (named entries only, not all of `~/.cache` or
  `~/Library/Caches`). Dependency downloads (extracted crates, Go modules,
  Maven and Gradle jars) are writable, like the project's own `node_modules`;
  toolchains, installed tools and their settings are not;
- on macOS, paths inside those trees whose contents run later outside the
  sandbox stay read-only: the project's `.git` (pinned, so it can't be renamed
  away or created), its hooks, config and submodule/worktree git dirs, nested
  repositories' hooks and config, agent and editor settings that carry hooks
  (`.chanto/`, `.claude/`, `.mcp.json`, `.husky/`, `.githooks/`,
  `.vscode/{settings,tasks}.json`), Chanto's own home, shell startup files and
  toolchain bins and settings;
- on Linux Landlock can only grant, and carving `.git` or `.chanto` out of the
  project would also take away the right to create files at its root or in
  `.git`, which breaks `git commit` and every build. So those files are compared
  around each sandboxed command in the project, and a change is put
  back and reported. The check runs when the command ends, so a process it
  leaves running can change them afterwards, and nested repositories' hooks
  aren't covered. The permission layer still keeps the file tools from changing
  them without asking. The home guards apply only where a widened tree
  (`allow_write`, `--add-dir`) covers them;
- on macOS and Linux, reads of known credential locations are denied (private
  keys, cloud and registry logins, Chanto's own token files), and network
  access can be turned off or limited to an allowlist of domains (on Windows
  the sandbox confines writes only). The macOS keychain stays reachable, so
  credential helpers (`git credential-osxkeychain`, `gh auth token`) can still
  hand out tokens;
- on macOS, local IPC that would reach a process outside the sandbox is
  blocked: no controlling terminal, no access to Chanto's or other terminal
  windows' ttys, no Apple Events, LaunchServices, clipboard or Shortcuts, and
  Unix sockets only inside the project and the command's own `$TMPDIR`. On
  Linux only the controlling terminal is taken away (and seccomp refuses
  TIOCSTI); the SSH, D-Bus, Docker and `XDG_RUNTIME_DIR` variables are
  removed, but that is advisory, since a command that knows a socket's path
  can still connect to it;
- leaving the sandbox (`dangerouslyDisableSandbox`) always needs a human.

The permission layer and the sandbox are independent, so a mistake in one is
still contained by the other.

Commands Chanto runs from project content rather than from the model also
run under the sandbox and without secret-named variables: verified-mode,
goal, outcome and team checks, formatters, and the scripts a runner takes
from a server (only with `chanto runner connect --allow-scripts`). Language
servers start without secret-named variables.

Git that Chanto runs on the host, outside the sandbox (the startup status,
the branch shown in the TUI, `/diff`, worktrees and background runs), turns
off what a repository's own config could make it run: the fsmonitor hook,
hooks, signing, external diff and textconv drivers, the repository's filter
and merge drivers (Git LFS's standard commands are kept), and submodules.
Bare repositories are never found by walking up from the working directory.
For a worktree, the git directory comes from the repository's own records,
which must name it exactly once and lead back to that repository; the
worktree's `.git` file, which commands in the worktree can rewrite, is never
followed, and file tools ask before writing any `.git` (including its Windows
spellings such as `.git.` and `GIT~1`).

### 3. Project files ↔ your settings: folder trust

Settings in a project that could run code or widen permissions (hooks, MCP
servers, status-line and key-helper commands, environment variables, sandbox
settings, allow rules, guardrail commands, plugins, language servers, the
auto-mode classifier's `classifier_model` and `optimize.token_classifier`) are
ignored until you trust the folder at a prompt. New `.mcp.json` servers also
need approval. Installing a plugin asks first when it has hooks, MCP or
language servers, or skills and commands that run `` !`cmd` `` blocks, register
`hooks` or pre-approve `allowed-tools`; plugins with symlinks are refused.

### 4. Your secrets ↔ the model and logs

Known secret values (from the environment, config, `.env` files, provider
settings and sign-in tokens) and secret-shaped strings are replaced with
placeholders before anything goes to the model, into logs, session files,
run event logs, event streams or shared transcripts. Masking happens in the
provider layer every model request goes through, so side calls (compaction,
hooks, the permission classifier) are covered like turns. Credentials Chanto
stores itself, and background runs' records and logs, are written with
owner-only permissions (on Unix), or to the macOS keychain. On Linux,
Chanto's process is non-dumpable, so commands it starts can't read its
environment from `/proc` or attach to it (nor can debuggers without root).

### 5. Local servers ↔ other programs

`chanto serve`, `chanto web` and the background-run daemon listen on
`127.0.0.1` by default and require a bearer token (compared in constant time,
stored owner-only) on every route except health. Tokens travel in headers,
and the web UI's launch link carries its token in the URL fragment, so browser
pages can't forge requests. The editor bridges authenticate with a header browsers
can't set on a WebSocket. The web UI never inserts HTML from model or tool
output, and is served with a strict Content Security Policy.

### 6. Remote sessions ↔ the network

`/remote` encrypts the session end to end: P-256 ECDH key agreement, keys
derived with HKDF-SHA256 for each purpose and direction, AES-256-GCM with
sequence numbers bound into the authenticated data, and key material only in
the link's URL fragment. A link admits only the first reader to join.

### 7. Releases ↔ you

Releases are built by GitHub Actions from version tags that only organization
admins can create. Archives are published with SHA-256 checksums and GitHub
build-provenance attestations; npm packages are published through trusted
publishing with Sigstore provenance; the Homebrew formula pins each archive's
checksum. All downloads use HTTPS. From v0.1.2 on, each release's
`SHA256SUMS` is signed with an Ed25519 release key whose public half is pinned
in Chanto and in both install scripts; from v0.1.5 on the same key's signature
also comes in SSH form (`SHA256SUMS.sshsig`). `chanto upgrade`, `install.sh`
and `install.ps1` check the signature before they read anything in
`SHA256SUMS`, including which version "latest" is, and refuse a release whose
signature is missing or wrong, or that they can't check: the scripts need
OpenSSL 3 or `ssh-keygen` (OpenSSH 8.1+), and stop rather than install
unverified. Releases before v0.1.2 aren't signed, so the scripts no longer
install them. A signature doesn't prove a release is the newest one, so a
hostile host could serve an older signed release as "latest": `chanto upgrade`
refuses to downgrade, and the scripts refuse a "latest" older than a floor
they carry (v0.1.6, raised with each security release). Within the floor, or
for a version asked for by name, rollback remains possible. See [Verifying a
release](reference.md#verifying-a-release).

## Repository checks and review policy

Changes to `main` require a pull request and the configured CI checks, including
tests, static analysis, secret scanning and fuzzing. Force pushes and deletion
are blocked, and these rules apply to administrators too. Release tags are
protected separately.

Chanto currently has one maintainer. Another person's approval, code-owner
approval and last-push approval are therefore not required. The merge queue and
up-to-date-branch requirement are intentionally disabled; a maintainer can merge
a passing pull request without repeatedly rebasing unrelated changes. Changes
that overlap still need their conflicts resolved and their combined behavior
checked.

OpenSSF Scorecard's **Branch-Protection** baseline is **3** for this policy.
Its warnings about approvals and up-to-date branches remain visible; this
baseline records the deliberate tradeoff rather than claiming maximal branch
protection. The separate static-analysis baseline is **10**, matching the
published result. Other baseline drops still fail the regression workflow.
Review this policy when more maintainers join.

## Secure design principles

- **Least privilege:** asking is the default; modes and rules widen it only on
  your say-so.
- **Fail-safe defaults:** anything the checks can't classify needs approval; a
  heredoc that never closes is judged line by line; an unknown project setting
  is ignored until trusted.
- **Complete mediation:** every tool call passes the same decision point,
  including those from subagents and background runs.
- **Defense in depth:** permissions, sandbox and secret redaction each limit
  what the others miss.
- **Open design:** the code, this model and the security tests are public.

## Common weaknesses countered

| Weakness | Countermeasure |
|---|---|
| CWE-78 OS command injection | Shell parsing before permission checks; sandbox; approval for substitutions and redirections |
| CWE-22 Path traversal | Paths canonicalized (symlinks, `..`, case) before workspace checks |
| CWE-918 Server-side request forgery | Approval for metadata, loopback and local-file URLs |
| CWE-79 Cross-site scripting | No HTML insertion of untrusted text; strict CSP; escaped shared transcripts |
| CWE-352 Cross-site request forgery | Header-based tokens that browsers won't send cross-site |
| CWE-200 / CWE-532 Exposure of sensitive information | Secret redaction before the model, logs and shares |
| CWE-798 Hard-coded credentials | gitleaks on every push; secret scanning with push protection |
| CWE-330 Insufficient randomness | Tokens and keys from the OS CSPRNG |
| CWE-319 Cleartext transmission | HTTPS/TLS for providers, updates and cloud; end-to-end encryption for remote sessions |
| CWE-494 Download without integrity check | Checksums, attestations and provenance (signatures planned) |

## Reviews

- October 2026: an internal review of the whole codebase covering supply chain,
  permissions and sandbox, network-facing surfaces, and credentials and
  updates. Its critical finding was fixed in v0.1.1; the remaining findings are
  being fixed, and are tracked privately until released.

Continuous checks: CodeQL, clippy with warnings as errors, cargo-deny,
gitleaks, OpenSSF Scorecard and Dependabot.
