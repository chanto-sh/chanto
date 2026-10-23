# Auto mode's fast path

**Opt-in and experimental:** set `permissions.fast_path = true` in your Chanto config. It stays off by default until the evaluation sets show no risky command fast-allowed. The current results are below.

In auto mode, Chanto checks each action before running it:

1. **Static rules** allow read-only and known-safe commands and block known-dangerous ones (`src/permissions.rs`).
2. **The fast path** (`src/risk.rs`): a small logistic-regression model over shell-command features. It clears routine commands in microseconds.
3. **The model classifier** reads your recent requests and decides everything else. It is slow, especially on local models.

The classifier uses the current agent model unless `classifier_model` is set (a named `provider/model` is supported). If a server explicitly rejects constrained JSON output, Chanto retries once without that constraint; the response must still contain a valid JSON `allow` or `block` decision. Connection, authentication, model-access, timeout, and malformed-response failures never approve an action. In interactive sessions, three consecutive blocks fall back to a permission prompt; non-interactive sessions keep denying. Use `chanto --classify <Tool> '<json>' "<request>"` to check the configured classifier.

The fast path can only **allow**. Blocking needs your intent, which only the model classifier sees. It defers to the model classifier when:

- **Unknown program.** A program it never saw in routine training, or an unfamiliar subcommand of a subcommand-style tool (`wrangler d1 …` isn't `wrangler dev`).
- **Hard guard.** These features always defer:
  - pipes into a shell;
  - sudo;
  - remote targets (`user@host:`), and `--remote`, `--prod` or `--production` flags;
  - writing outside the project, through system paths, shallow home paths, personal areas (`~/Library`, `~/Documents`, `~/Downloads`, `~/Desktop`, …), `~/.dotfiles` or redirects;
  - secret-looking variables;
  - SQL that writes, and HTTP calls that write, anywhere in the command (including inside `$(…)`, `ssh "…"` or `docker exec`);
  - interpreter one-liners that write files or run subprocesses (`python3 -c "json.dump(d, open('f','w'))"`, `node -e "fs.writeFileSync(…)"`), whose writes the project-boundary check can't see;
  - background jobs;
  - local scripts named like deploy, publish, release or push.
- **Denied before.** You denied a command of the same shape (program plus subcommand). Learning only ever makes it stricter.
- **Custom rules.** You wrote `permissions.auto_mode_rules`. Only the model can read those.

A command is as risky as its riskiest part. Every part of a compound command scores separately: `&&`/`;`/`|` chains, loops, and commands inside `$(…)` or backticks. Comments and heredoc bodies are ignored, because they don't run as commands.

## Files

- `gen.py`: generates most of `train.jsonl`. These are synthetic, public examples, labelled by construction and composed the way agents write commands.
- `mine.py`: adds routine examples from the vendored public snapshots in `sources/` (see `sources/README.md` for provenance and licenses). It keeps only read-only local tools and drops any example that carries a risky token or a home/system path.
- `LABELING.md`: the rubric for the hand-labelled evaluation sets.
- `anonymize.py`: anonymizes real agent commands for evaluation. It strips:
  - heredoc bodies, comments and commit messages;
  - names, hosts, domains and project names;
  - any word not found in public vocabularies.

  It also drops anything that looks like a secret.

## Training and evaluating

```bash
python3 classifier/gen.py  > classifier/train.jsonl
python3 classifier/mine.py >> classifier/train.jsonl
chanto --train-risk classifier/train.jsonl src/risk_model.json   # prints held-out results
cargo build                                                       # the model ships in the binary
chanto --eval-risk classifier/eval/real.jsonl                     # labelled evaluation sets
chanto --risk 'cargo test && git push'                            # features and verdict for one command
```

Training holds out 20% of the examples. It picks the threshold so that no held-out risky command is fast-allowed, then trains on everything.

The mix is deliberately permissive. Shapes that would teach the model to flag a common routine feature (`python3 -c`, `curl -k`, `sed -i` on a manifest) just to catch one judgement-call leak cost far more routine coverage than they buy, so those commands are left to the model classifier instead. What matters is precision on allow: of everything it clears fast, ~99% is genuinely routine.

## Current results

| Set | Routine commands cleared fast | Risky commands fast-allowed |
|---|---|---|
| Synthetic, held out | 92% (995/1077) | 0 / 763 |
| `eval/real.jsonl` | 75% (1314/1753) | 7 / 330 |
| `eval/tldr.jsonl` | 38% (331/859) | 8 / 187 |

On the author's full real history (19k agent commands), the share needing a model classifier call drops from 75% to 59%.

The 7 remaining real "leaks" are mostly judgement calls that aren't worth a hard guard:

- **Anonymization artefacts (4):** a script named `upload…` became a placeholder, so the outward-facing guard no longer fires; two commands the anonymizer left malformed. The real commands defer.
- **Judgement calls (3):** inline `python3 -c` that prints an app's own headers, a GET to an `/auth/me` endpoint with `-k`/`--resolve`, and a version bump made with `sed -i` on `package.json`. Guarding `-k`/`--resolve` or `sed -i` would defer 18 routine commands to close 3, so they're left to the model classifier.

Reads of the user's personal areas (`~/Library`, `~/Documents`, `~/Downloads`, …) are guarded, which closed the two `ls ~/Library/…` leaks without costing routine coverage. So did interpreter one-liners that write files, whose writes the project-boundary check can't see.

The tldr ones are mostly game-server binaries and other unfamiliar local tools.
