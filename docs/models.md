---
title: 'Models'
section: Getting started
order: 7
description: 'Local models, hosted APIs, your ChatGPT plan and cloud providers.'
---

Chanto talks to models through a few provider kinds. Name a provider in `config.toml` and refer to its models as `provider/model`, so the main agent, subagents (`subagent_model`) and the auto-mode classifier (`classifier_model`) can each use a different one. Switch live with `/model`.

## Local models

Ollama works out of the box. Any OpenAI-compatible server works too: llama.cpp's `llama-server`, LM Studio, vLLM, MLX and others.

```toml
provider = "ollama"
model = "qwen3:8b"
num_ctx = 32768

[providers.lmstudio]          # use as model = "lmstudio/<model-id>"
kind = "openai"
base_url = "http://localhost:1234/v1"
```

Chanto is tuned for small local models: it parses text-format tool calls, repairs truncated JSON arguments, tolerates whitespace drift in edits, and keeps a stable prompt prefix so the server's KV cache is reused.

## Hosted APIs

| `kind`                                  | API                                   | Key                              |
| --------------------------------------- | ------------------------------------- | -------------------------------- |
| `ollama`                                | Ollama `/api/chat`                    | —                                |
| `openai`                                | OpenAI-compatible `/chat/completions` | `api_key`                        |
| `anthropic`                             | Anthropic Messages API                | `api_key` or `ANTHROPIC_API_KEY` |
| `openai-responses`                      | OpenAI Responses API                  | `api_key` or `OPENAI_API_KEY`    |
| `bedrock`, `vertex`, `azure`, `foundry` | Cloud platforms                       | the cloud's own credentials      |

Built-in providers need no config. Besides Anthropic and OpenAI, they include OpenRouter, Gemini, DeepSeek, xAI, Mistral, Groq, Cerebras, Together, Fireworks, Moonshot, Z.ai, MiniMax, DashScope, Hugging Face, NVIDIA, Vercel AI Gateway, OpenCode Zen and Baseten. Each works with its usual key variable (`ANTHROPIC_API_KEY`, `GEMINI_API_KEY`, …) or `chanto login <provider>`, and `chanto providers` shows which ones you're logged in to. For example, with `ANTHROPIC_API_KEY` set, `chanto -m anthropic/<model>` just works.

## Free models

With no key at all, the model picker's _Free_ section lists three options:

- **OpenRouter's free models:** sign in with `chanto login openrouter` and pick one. Only models with tool calls and at least 64k of context are listed. They're rate-limited (about 50 requests a day, 1,000 once the account has $10 of credit), and their providers may log or train on prompts.
- **Gemini's free tier:** paste a free key from [Google AI Studio](https://aistudio.google.com/apikey). Google may use free-tier prompts to improve its products.
- **A local model:** free and private.

Don't use the free hosted options on code you can't share.

## Your ChatGPT plan

```sh
chanto login chatgpt
```

Then run `/models` to see your account's model names and select one with `/model chatgpt/<model>`. Chanto stores this login separately from Codex, and logging in doesn't change the selected model.

## Amazon Bedrock, Google Vertex AI and Azure

Chanto uses your cloud's own credentials: your AWS credentials (SigV4) for Bedrock, Application Default Credentials for Vertex AI, and API keys or Entra ID tokens for Azure. Nothing is resolved at startup; credentials are fetched on the first request and cached until shortly before they expire. See [Cloud providers](./reference.md#cloud-providers-bedrock-vertex-ai-azure).
