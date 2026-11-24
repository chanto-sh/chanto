---
title: 'FAQ'
section: Getting started
order: 8
description: 'What the name means, what it costs, which models it runs and what it sends where.'
---

## What does "Chanto" mean?

ちゃんと (_chanto_) is Japanese for "properly", as in a job done properly.
That's the aim: an agent that does the job properly: fast, light on your
machine, and dependable through long sessions.

## Is it free?

Yes. Chanto is open source under the Apache License 2.0 and needs no account.
You pay only your model provider, or nothing if you run a local model.

## Which models does it work with?

Local models through Ollama, llama.cpp, vLLM or any OpenAI-compatible server;
hosted APIs such as Anthropic, OpenAI, OpenRouter, Gemini and DeepSeek; your
ChatGPT plan; and your company's Amazon Bedrock, Google Vertex AI or Azure
account. Switch with `/model`. See [Models](./models.md).

## Does my code leave my machine?

Only to the model provider you pick. With a local model, nothing about your
project leaves the machine. Apart from that, Chanto makes two small requests
to chanto.sh, at most once a day each: a check for new releases and a price
list. Neither sends prompts, keys or project data, and `CHANTO_NO_UPDATE_CHECK=1`
turns both off. Feedback is sent only when you choose to, after you've seen
exactly what's in it, and automatic error reports are off unless you opt in.

## I already use Claude Code or Codex. Do I have to set things up again?

No. Chanto reads your Claude Code and Codex settings as they are: permission
rules, hooks, MCP servers, skills, commands, subagents and instruction files
like `CLAUDE.md`. `chanto doctor` shows what it picked up and from where.

## Can I use my subscription plan?

Yes for ChatGPT, Kimi Code, Z.ai GLM, MiniMax, Qwen Code, Tencent CodeBuddy,
Volcengine, StepFun and Cline Pass: run `chanto login` with the provider. See
[subscription coding plans](reference.md#subscription-coding-plans). Claude Pro
and Max plans don't work, including through Claude Code, so use an Anthropic API
key for Claude models.

## `chanto upgrade` said it upgraded, but the version didn't change. What now?

On Homebrew, versions before v0.1.13 could say `Upgraded Chanto` when nothing changed: if your copy of the tap is out of date, `brew upgrade` finds nothing to do, and Chanto took that as success. Update once by hand, and upgrades work normally after that:

```sh
brew update && brew upgrade chanto
chanto --version
```

Installed with npm? Run `npm install -g @chanto/cli@latest`. With Scoop, run `scoop update chanto`.

## How is it different from Claude Code or Codex?

It runs any model, including small local ones; it's light on CPU and memory
(see the [benchmarks](https://github.com/chanto-sh/benchmarks/blob/main/RESULTS.md));
and with `--verify` it runs your project's checks before it calls a task done.

## Which platforms does it run on?

macOS, Linux and Windows, on arm64 and x86_64 (Windows on x64). See
[Install](./reference.md#install).

## Is it safe to let it run commands?

Commands run in a sandbox that limits what they can write, and risky ones wait
for your approval. Known secrets are kept out of the model's context, and
`/rewind` undoes a turn. See [Sandbox](./reference.md#sandbox) and
[Permission modes](./reference.md#permission-modes).

## How do I report a bug or ask for a feature?

Open an issue on [GitHub](https://github.com/chanto-sh/chanto/issues), or use
`/feedback` from inside Chanto. Report security problems privately: see the
[security policy](https://github.com/chanto-sh/chanto/blob/main/SECURITY.md).
