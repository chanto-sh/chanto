# Evaluation sets for auto mode's fast path

Each line is `{"cmd", "risky", "why"?}`, labelled by reviewers against [../LABELING.md](../LABELING.md) without seeing the classifier's verdict. Run `chanto --eval-risk classifier/eval/<file>.jsonl`. It reports the share of routine commands cleared fast and lists every risky command that was fast-allowed.

- **`real.jsonl`:** real commands that coding agents ran on the author's machine and that reach the fast path, i.e. the static rules don't decide them.
  - Every command the fast path allowed was sampled, plus a random sample of the ones it deferred.
  - Anonymized with [../anonymize.py](../anonymize.py):
    - Heredoc bodies, comments and messages become `…`.
    - Names, hosts, domains and projects become placeholders.
    - Every uncommon word becomes `w…` or `x…`, and money amounts and long numbers are masked.
    - Anything that looked like a secret was dropped.
  - Placeholders can hide what a command really does. For example, a script named like `upload` becomes `site215` and no longer trips the outward-facing guard. So a few "leaks" here are artefacts of anonymization.
- **`tldr.jsonl`:** command examples from [tldr-pages](https://github.com/tldr-pages/tldr), © the tldr-pages team and contributors, licensed under [CC BY 4.0](https://creativecommons.org/licenses/by/4.0/). The commands are unchanged apart from filling in `{{placeholders}}`; `source` names the page. Most use tools that coding agents rarely run, which the fast path deliberately leaves to the model classifier.

The labelling was partly incomplete: some reviewer runs stopped early. So these are samples, not every candidate command.
