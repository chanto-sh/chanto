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
disclosure. We aim to acknowledge a report within a few days and to keep you
updated as we investigate.

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
