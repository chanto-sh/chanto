# Security Policy

## Reporting a vulnerability

**Please do not report security vulnerabilities through public GitHub issues,
pull requests, or discussions.**

Instead, report them privately through GitHub's private vulnerability reporting:

1. Go to the repository's **Security** tab.
2. Click **Report a vulnerability** (under *Advisories*).
3. Fill in the details.

This opens a private advisory visible only to you and the maintainers. If you
are unable to use that channel, email
[security@chanto.sh](mailto:security@chanto.sh). Don't include details in a
public message.

Please include, as far as you can:

- the affected version or commit,
- a description of the vulnerability and its impact,
- steps to reproduce, or a minimal proof of concept,
- any known mitigations.

Please give us a reasonable chance to release a fix before any public
disclosure.

## How we respond

1. **Acknowledge** the report within 3 business days.
2. **Triage:** reproduce it, assess severity (we use CVSS as a guide) and agree
   on a timeline with you. We aim to fix critical and high-severity issues
   within 14 days and others within 60.
3. **Fix** it in a private advisory (or privately, for issues reported by email),
   with a regression test, and **release** a patched version.
4. **Disclose:** describe the fix in the release notes and, when it affects
   users, publish a GitHub security advisory (requesting a CVE where
   appropriate).
5. **Credit** the reporter by name or handle in the release notes and the
   advisory, unless they ask to stay anonymous.

We keep you updated throughout, and you're welcome to review the fix before it
ships.

## Fixed vulnerabilities

| Version | Issue | Found by |
|---|---|---|
| v0.1.1 | Shell commands could be hidden from the permission check by a quote in a comment, a heredoc body or an escape | Internal security review |

## Supported versions

Chanto is pre-1.0 and ships from `main`. Security fixes land on `main` and in
the next release; please test against the latest release or `main` before
reporting.

## Scope

Chanto runs as a local coding agent that executes tools and model-chosen
commands on your machine. Reports that are most useful concern, for example:

- ways a model or untrusted input can cause actions outside the intended
  permission model (the risk classifier, the sandbox, permission prompts),
- secrets or credentials being logged, leaked, or sent to an unintended host,
- the hosted service at api.chanto.sh exposing one user's resources to another.

Running an agent against untrusted code or prompts carries inherent risk; the
permission system and sandbox are defense-in-depth, not a guarantee. Findings
that strengthen those boundaries are especially welcome.

[docs/security-model.md](docs/security-model.md) describes the threat model,
the trust boundaries and how Chanto defends them.
