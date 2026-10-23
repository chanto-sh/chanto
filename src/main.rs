//! The `chanto` command-line binary. All behaviour lives in the `chanto`
//! library crate (`src/lib.rs`); this is just the async entry point, so the
//! same core can be embedded by other programs.
#[tokio::main]
async fn main() {
    chanto::run().await;
}
