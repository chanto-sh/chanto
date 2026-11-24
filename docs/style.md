# Documentation style

The site is read by people deciding whether to install a terminal tool. Write like
you would in a code review: a fact, then the number that proves it.

1. **One claim per sentence.** Target 20 words; anything over 40 is a defect
   (`scripts/docs_style.py` fails the build on it).
2. **Delete the benefit tail.** If the fact is good, “so it stays responsive and
   leaves your machine free” is filler.
3. **No unmeasured superlatives.** “Fast” needs “11–14 ms”. Otherwise cut the
   adjective.
4. **Name the concrete thing.** “Reads `.claude/settings.json` and `AGENTS.md`”,
   not “works with your setup”.
5. **Periods over dashes and parentheses.** One aside per paragraph at most.
6. **No “and more”, “simply”, “not just X but Y”.** If the list is finite, finish
   it; if it is not, say “such as”.
7. **Say “Chanto” once per page.** Then use “it” or name the subcommand.
8. **Numbers appear once.** Link to the benchmark instead of repeating the table.

Examples:

| Instead of | Write |
|---|---|
| Chanto is a fast, lean coding agent that starts instantly and runs everywhere. | Chanto is a coding agent: one Rust binary, local or hosted models. |
| It checks the tests before it finishes, so you can trust the result. | `--verify` runs the tests related to the change. |
| Powerful permissions, seamless setup and more. | Risky actions ask first. Existing Claude Code settings are read as they are. |
