// Runs a wasm32-wasip1 program, such as a test binary, under Node's WASI:
// `CARGO_TARGET_WASM32_WASIP1_RUNNER="node scripts/wasi-run.mjs" cargo test
// -p chanto-core --target wasm32-wasip1`. The host's root is visible as `/`.
import { readFile } from "node:fs/promises";
import { WASI } from "node:wasi";
import { argv, env, exit } from "node:process";

const [, , file, ...args] = argv;
const wasi = new WASI({
  version: "preview1",
  args: [file, ...args],
  env,
  preopens: { "/": "/" },
  returnOnExit: true,
});
const module = await WebAssembly.compile(await readFile(file));
const instance = await WebAssembly.instantiate(module, wasi.getImportObject());
exit(wasi.start(instance));
