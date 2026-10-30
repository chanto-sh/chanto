# Changelog fragments

A user-visible change adds one file here, named after its branch
(`changelog.d/fix-venv-path.md`), instead of editing `CHANGELOG.md`. Every pull
request writing its own file means none of them conflict with each other.

A fragment holds one or more list items in the changelog's style, with no
headings:

```markdown
- **Smaller install.** The release binary is about 21% smaller ...
```

The release pull request runs `python3 packaging/changelog.py vX.Y.Z`, which
moves any **Unreleased** entries and every fragment (in the order they were
added) into the dated `## vX.Y.Z` section and deletes the fragments. CI checks
each fragment's shape with `python3 packaging/changelog.py --check`.

CI requires a fragment from every pull request whose title has a user-visible
type (`feat`, `fix`, `perf`). If users can't notice the change (a warning
only CI sees, a test-only fix), say so in the description instead, on a line
of its own with the reason, which the squash commit keeps:

```markdown
Changelog: none (Windows-only compiler warnings)
```
