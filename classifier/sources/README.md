# Vendored training sources

Compact snapshots of public command data, read by [`../mine.py`](../mine.py) to
add routine examples the synthetic generator doesn't cover. Each line is
`{"cmd", "source"}`; `source` names the page the command came from.

- **`tldr.jsonl`** — command examples from [tldr-pages](https://github.com/tldr-pages/tldr),
  © the tldr-pages team and contributors, licensed under
  [CC BY 4.0](https://creativecommons.org/licenses/by/4.0/). Vendored here, so the
  training set rebuilds offline. Snapshot of `pages/common` at commit
  `273668766e52afe5307483ab06b65198794a5932`, with `{{placeholders}}` left as-is;
  `mine.py` fills or rejects them.

  tldr has pages for `rm`, `dd`, `sudo`, `git push`, … Teaching those as routine
  would be a safety hole, so `mine.py` keeps only an allowlist of read-only local
  tools (`SAFE_PROGRAMS`) and drops any example carrying a risky token, a shell
  redirect into the system, or a home path.

## Not vendored

- **GTFOBins** ([github.com/GTFOBins/GTFOBins.github.io](https://github.com/GTFOBins/GTFOBins.github.io))
  is licensed **GPL-3.0**, which the permissive-only training set avoids. It is
  therefore *not* committed here, and `classifier/sources/gtfobins.jsonl` is
  gitignored so a local snapshot can't be committed by accident. `mine.py` reads
  or writes GTFOBins data only with an explicit `--allow-gpl`
  (`--gtfobins-dir <clone>/_gtfobins` for a local clone); without it the data is
  skipped. Its output must not be redistributed with the shipped weights unless
  the licensing question is settled.

## Rebuilding a snapshot

```bash
python3 classifier/mine.py --extract tldr /path/to/tldr/pages/common classifier/sources/tldr.jsonl
```
