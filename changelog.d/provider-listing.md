- **`chanto providers` lists your own providers.** Providers defined under
  `[providers.*]` in `config.toml` now appear with their kind and endpoint, so a model
  like `fast/…` no longer looks unknown. A model whose prefix is not a configured
  provider now says so, and points at `[providers.<id>]` plus a restart, instead of
  suggesting `ollama pull`.
