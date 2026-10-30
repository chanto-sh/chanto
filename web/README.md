# chanto-web

Chanto's agent in the browser: `chanto-core`'s agent loop, file tools and
in-memory file system behind `wasm-bindgen`. The page supplies the model as a
`transport` function; the project's files never leave the page.

## Build

```sh
rustup target add wasm32-unknown-unknown
cargo install wasm-bindgen-cli --version 0.2.129 --locked   # matches Cargo.lock
cargo build -p chanto-web --target wasm32-unknown-unknown --release
wasm-bindgen --target web --out-dir web/pkg \
  target/wasm32-unknown-unknown/release/chanto_web.wasm
```

## Demo

`web/demo/` runs the agent against a model on the GPU through
[WebLLM](https://github.com/mlc-ai/web-llm) (WebGPU: desktop Chrome, Edge or
Safari 26), on a small sample project, and times every step: the model's
first token and total, its tokens per second, tool time, and Chanto's own
work. Build the package as above, then:

```sh
python3 -m http.server 8765 --directory web
# open http://127.0.0.1:8765/demo/
```

The first load of a model downloads it (about 2 GB for the default
Qwen2.5 Coder 3B) into the browser's cache; later loads take a few seconds.
Small models use the text tool protocol (`Agent::text_tools`), as the CLI's
`tool_protocol = "text"` does.
