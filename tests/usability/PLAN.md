# End-user usability tests

What a user does with Chanto, checked from the outside. Each scenario has a
**verify** step that checks the actual outcome (files, git state, tests, the
terminal's contents), never just the agent's own claim. A scenario passes only
when every check passes. Run on a real model (DeepSeek through
the benchmark suite's `proxy.sh`, plus a local Ollama model), on a quiet machine. Not while a
benchmark is running.

Record for each scenario: pass or fail, wall time, tokens, and anything
confusing a new user would hit (wording, missing hints, surprises).

## Getting started

1. **First run, nothing configured.** Fresh `HOME`, no keys: `chanto`.
   Verify:
   - the picker shows Found / Sign in / Free sections;
   - choosing a local model writes `~/.chanto/config.toml`;
   - the first prompt gets an answer;
   - `chanto doctor` shows no warnings.
2. **Free model path.** Choose OpenRouter's free models, then the Gemini free tier.
   Verify:
   - only tool-capable models with at least 64k context are listed;
   - a two-step task that needs a tool call completes.
3. **Misconfigured.** Try a wrong model name, a server that's down, a bad key, and `deepseek/…` with no key.
   Verify that each message says what to do, and that the suggested command fixes it.

## Everyday coding (in a real repo, with its tests)

4. **Question only.** "What does this project do, and where is X handled?"
   Verify:
   - the answer matches the code (spot-check two claims);
   - `git status` is clean, meaning no edits.
5. **Fix a seeded bug.** Break one function, then ask for the failing test to be fixed.
   Verify:
   - the project's test suite passes when run independently afterwards;
   - the diff touches only the bug;
   - no test was edited or skipped.
6. **Feature with `--verify`.** Ask for a small feature.
   Verify:
   - the final answer ends with the evidence line;
   - rerunning that exact command passes;
   - `--verify` refuses to finish while a test is failing.
7. **Rewind.** After two edit turns, `/rewind` to before the first ("code").
   Verify:
   - every touched file is byte-identical to before (hashes);
   - files created by shell commands are gone too;
   - `/rewind` undo restores the edits.
8. **Resume from elsewhere.** Quit, `cd /`, then `chanto -r <id>`.
   Verify:
   - it reopens in the original project;
   - the next answer uses the earlier context.

## Safety

9. **Permission modes.**
   - Default: an edit asks first, and "no" leaves the file unchanged.
   - Auto mode: `rm -rf ~/somewhere-outside` is blocked.
   - Verify no file outside the project changed (checksums of a decoy directory).
10. **Sandbox.** Ask for a command that writes outside the project and one that fetches a URL.
    Verify:
    - both are blocked;
    - the agent explains, rather than looping.
11. **Two sessions, one repo.** Two terminals each get a task touching the same file.
    Verify:
    - each is told about the other;
    - the second edit to the shared file stops once and names the other task;
    - `/peers` lists both;
    - neither session's changes are lost (diff against both intended changes).

## The terminal

12. **Inline mode.** A long answer with code blocks; scroll back while it streams; resize twice mid-stream; quit mid-answer.
    Verify:
    - in the scrollback (tmux `capture-pane -S -`), every line appears, none of the live region does, and nothing is lost on quit.
13. **Full screen.** Scroll up while it streams.
    Verify:
    - the view doesn't move;
    - copy (Ctrl+S, then drag) yields the exact text;
    - Markdown renders while streaming.
14. **Model and effort.** `/model` opens instantly. `/effort high` and `/effort max` work on Anthropic, OpenAI and DeepSeek models without API errors.

## Integrations

15. **MCP.** A legacy SSE server, a streamable HTTP server, elicitation (answer, skip, decline), and sampling (allow, deny).
    Verify what the server received in each case.
16. **Web UI.** Run a session, rewind ("conversation"), fork, and Esc Esc.
    Verify that the server's session matches what the page shows.
17. **Automation.** `-p --output-format stream-json`, `--input-format stream-json` with three messages and one bad line, and `chanto exec`.
    Verify:
    - every stdout line parses as JSON;
    - exit codes match the outcomes.
18. **Computer use (macOS).** Take a screenshot, open an app, and click a button, approving each step.
    Verify the app state changed as asked.

## Staying power

19. **Long session.** Work until compaction happens twice.
    Verify:
    - the agent still knows the task's key facts afterwards;
    - memory (RSS) stays flat;
    - no slowdown in time to first token.
20. **Speed.** Measure, against Claude Code and Codex on the same model:
    - startup to prompt;
    - time to first token;
    - tokens and wall time for scenarios 5 and 6.
