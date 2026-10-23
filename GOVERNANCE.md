# Governance

Chanto is a young open-source project with a single maintainer. This document
says how decisions are made today, who does what, and how the project keeps
going if the maintainer is unavailable. It will change as more people join.

## Roles

**Contributors** are everyone who opens an issue, starts a discussion, reviews
or sends a pull request. Anyone can contribute; [CONTRIBUTING.md](CONTRIBUTING.md)
explains how, and the [Code of Conduct](CODE_OF_CONDUCT.md) applies everywhere.

**Maintainers** review and merge pull requests, triage issues, cut releases and
handle security reports. Today there is one:

| Maintainer | Areas |
|---|---|
| Daniel Phang ([@dphang](https://github.com/dphang)) | Everything: code, releases, security response, chanto.sh, the GitHub organization and the package registries |

A maintainer is responsible for:

- reviewing pull requests and keeping `main` releasable (CI must pass; changes
  land through pull requests),
- answering issues and security reports (see [SECURITY.md](SECURITY.md)),
- cutting releases with notes in [CHANGELOG.md](CHANGELOG.md),
- keeping the documentation current with each release.

## How decisions are made

- Day-to-day changes are decided in pull requests: anyone can propose one, and a
  maintainer merges it once it fits the project and CI passes.
- Larger changes (new features, changes to the permission model, sandbox or
  wire protocol, anything that breaks users) start as an issue or a
  [discussion](https://github.com/chanto-sh/chanto/discussions) so the approach
  can be agreed before code is written.
- When people disagree, the maintainers decide, explaining why in the thread.
  While there is one maintainer, that's their call; once there are several,
  they decide by consensus and fall back to a simple majority.
- The direction of the project is described in [ROADMAP.md](ROADMAP.md).

## Becoming a maintainer

Contributors who have made sustained, high-quality contributions and reviews
can be invited to become maintainers by the existing maintainers. New
maintainers get merge rights first; release and organization-owner access
follow once they've been active for a while.

## Continuity

The project's accounts and the people who hold them:

| Asset | Held by |
|---|---|
| GitHub organization [chanto-sh](https://github.com/chanto-sh) (code, releases, Homebrew tap) | @dphang (owner) |
| chanto.sh domain, DNS and website hosting (Cloudflare) | @dphang |
| crates.io crates `chanto`, `chanto-proto` | @dphang |
| npm packages `@chanto/cli*` (organization `chanto`) | @dphang |

Releases are built and published by GitHub Actions from version tags
(crates.io with an API token, npm through trusted publishing, Homebrew with a
deploy key), so no release depends on one person's machine. The source,
history, issues and releases are public, so the project can always be forked
and continued.

With one maintainer there is no second person who can take over within a week
yet. Adding a second organization owner and registry owner is the first step
in the roadmap's governance item; until then, if the maintainer is unreachable
for more than a week, contributors should open an issue and contact
hello@chanto.sh.
