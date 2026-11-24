//! The `chanto` command-line binary. All behaviour lives in the `chanto`
//! library crate (`src/lib.rs`); this is just the async entry point, so the
//! same core can be embedded by other programs.
fn main() {
    // What it took to get here (loading the binary and its libraries) shows
    // as this checkpoint's CPU time.
    chanto::timing("main entered");
    let mut rt = tokio::runtime::Builder::new_multi_thread();
    // The agent mostly waits on the model and on child processes, and work
    // that blocks runs on tokio's blocking pool, so a few workers keep up;
    // one per core (ten or more on a laptop) only costs startup time and
    // memory. `TOKIO_WORKER_THREADS` still sets the count.
    if std::env::var_os("TOKIO_WORKER_THREADS").is_none() {
        let cores = std::thread::available_parallelism().map_or(4, |n| n.get());
        rt.worker_threads(cores.min(4));
    }
    rt.enable_all()
        .build()
        .expect("tokio runtime")
        .block_on(chanto::run());
}
