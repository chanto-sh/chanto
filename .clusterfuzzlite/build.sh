#!/bin/bash -eu
# Builds the fuzz targets in fuzz/ for ClusterFuzzLite, each with its seed
# corpus (fuzz/seeds/<target>).

cd "$SRC/chanto/fuzz"
cargo fuzz build -O --debug-assertions

for target in shell_permission redact url_checks path_checks sandbox_profile; do
    cp "target/x86_64-unknown-linux-gnu/release/$target" "$OUT/"
    (cd "seeds/$target" && zip -q -r "$OUT/${target}_seed_corpus.zip" .)
done
