//! Golden tests for the hidden debug paths that inspect the tool protocol
//! without a model: `--run-tool`, `--risk` and `--triage-commands`.
//!
//! They pin the exact stdout framing (`is_error:` / `images:` / `display:` /
//! `---` / content), the exit-code contract, and the static permission fast
//! path, so a change to tool output or the routine set is caught in CI rather
//! than after a release. Only deterministic inputs are used: no network, no
//! model, no clock. `--classify` is deliberately absent — it calls the model.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_chanto")
}

/// One throwaway project dir with its own `CHANTO_HOME`, removed on drop.
struct Sandbox {
    dir: PathBuf,
}

impl Sandbox {
    fn new() -> Sandbox {
        static N: AtomicUsize = AtomicUsize::new(0);
        let n = N.fetch_add(1, Ordering::SeqCst);
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let dir =
            std::env::temp_dir().join(format!("chanto-golden-{}-{nanos}-{n}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("create sandbox");
        Sandbox { dir }
    }

    fn write(&self, name: &str, content: &str) -> PathBuf {
        let p = self.dir.join(name);
        std::fs::write(&p, content).expect("write fixture");
        p
    }

    /// Run `chanto` with `args` from the sandbox dir and return its output.
    fn run(&self, args: &[&str]) -> Out {
        let out = Command::new(bin())
            .args(args)
            .current_dir(&self.dir)
            .env("CHANTO_HOME", self.dir.join("home"))
            .env("CI", "1")
            .output()
            .expect("run chanto");
        Out {
            stdout: self.normalize(&String::from_utf8_lossy(&out.stdout)),
            stderr: self.normalize(&String::from_utf8_lossy(&out.stderr)),
            code: out.status.code().unwrap_or(-1),
        }
    }

    /// Replace every spelling of the sandbox path with `<DIR>` so golden output
    /// is machine-independent (`/tmp` is `/private/tmp` on macOS, and Windows
    /// canonicalizes to a `\\?\` prefix).
    fn normalize(&self, s: &str) -> String {
        let mut cands: Vec<String> = vec![self.dir.display().to_string()];
        if let Ok(c) = std::fs::canonicalize(&self.dir) {
            let c = c.display().to_string();
            if let Some(stripped) = c.strip_prefix(r"\\?\") {
                cands.push(stripped.to_string());
            }
            cands.push(c);
        }
        cands.sort_by_key(|c| std::cmp::Reverse(c.len()));
        let mut out = s.replace("\r\n", "\n");
        for c in cands {
            out = out.replace(&c, "<DIR>");
        }
        // Windows joins the file names under it with `\`.
        out.replace(r"<DIR>\", "<DIR>/")
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

struct Out {
    stdout: String,
    stderr: String,
    code: i32,
}

/// Find the `---`-delimited body of a `--run-tool` result.
fn body(stdout: &str) -> &str {
    stdout.split_once("---\n").expect("a --- separator").1
}

#[test]
fn read_prints_the_numbered_file_in_a_frame() {
    let s = Sandbox::new();
    s.write("a.txt", "hello world\n");
    let o = s.run(&["--run-tool", "Read", r#"{"file_path":"a.txt"}"#]);
    assert_eq!(o.stderr, "");
    assert_eq!(o.code, 0);
    assert_eq!(
        o.stdout,
        "is_error: false\nimages: 0\ndisplay: Read 1 line\n---\n1\thello world\n\n\n"
    );
}

#[test]
fn a_tool_error_is_reported_in_the_frame_not_the_exit_code() {
    let s = Sandbox::new();
    let o = s.run(&["--run-tool", "Read", r#"{"file_path":"nope.txt"}"#]);
    // A missing file is the model's problem to see, not a failed process.
    assert_eq!(o.code, 0);
    assert_eq!(
        o.stdout,
        "is_error: true\nimages: 0\ndisplay: \n---\nFile does not exist: <DIR>/nope.txt\n\n"
    );
}

#[test]
fn write_creates_the_file_and_says_so() {
    let s = Sandbox::new();
    let o = s.run(&[
        "--run-tool",
        "Write",
        r#"{"file_path":"b.txt","content":"line1\nline2\n"}"#,
    ]);
    assert_eq!(o.code, 0);
    assert!(
        o.stdout
            .starts_with("is_error: false\nimages: 0\ndisplay: Wrote 2 lines to b.txt\n---\n")
    );
    assert_eq!(body(&o.stdout), "File created: <DIR>/b.txt (2 lines).\n\n");
    assert_eq!(
        std::fs::read_to_string(s.dir.join("b.txt")).unwrap(),
        "line1\nline2\n"
    );
}

#[test]
fn write_to_an_unread_existing_file_is_refused() {
    let s = Sandbox::new();
    s.write("b.txt", "old\n");
    let o = s.run(&[
        "--run-tool",
        "Write",
        r#"{"file_path":"b.txt","content":"new\n"}"#,
    ]);
    assert!(o.stdout.contains("is_error: true"), "{}", o.stdout);
    assert!(
        body(&o.stdout).contains("You must Read <DIR>/b.txt before modifying it"),
        "{}",
        o.stdout
    );
    // The guard held: the file is untouched.
    assert_eq!(
        std::fs::read_to_string(s.dir.join("b.txt")).unwrap(),
        "old\n"
    );
}

#[test]
fn one_argument_array_runs_each_call_in_the_same_session() {
    let s = Sandbox::new();
    s.write("a.txt", "hello world\n");
    s.write("c.txt", "aaa\nbbb\n");
    let o = s.run(&[
        "--run-tool",
        "Read",
        r#"[{"file_path":"a.txt"},{"file_path":"c.txt"}]"#,
    ]);
    assert_eq!(o.code, 0);
    assert_eq!(
        o.stdout,
        "is_error: false\nimages: 0\ndisplay: Read 1 line\n---\n1\thello world\n\n\n\
         is_error: false\nimages: 0\ndisplay: Read 2 lines\n---\n1\taaa\n2\tbbb\n\n\n"
    );
}

#[test]
fn an_unknown_tool_fails_the_process() {
    let s = Sandbox::new();
    let o = s.run(&["--run-tool", "Nope", "{}"]);
    assert_eq!(o.code, 1);
    assert!(o.stderr.contains("no tool Nope"), "{}", o.stderr);
}

#[test]
fn the_risk_verdict_is_stable() {
    let s = Sandbox::new();
    let o = s.run(&["--risk", "ls -la"]);
    assert_eq!(o.code, 0);
    assert_eq!(
        o.stdout,
        "features: [\"bin:ls\", \"flag:ls -a\", \"flag:ls -l\"]\nverdict: Allow { p: 0.002197436 }\n"
    );
}

#[test]
fn triage_counts_the_static_path_and_defers_the_rest() {
    let s = Sandbox::new();
    s.write(
        "cmds.json",
        r#"["ls","cat a.txt","rm -rf build","pip install requests"]"#,
    );
    let o = s.run(&["--triage-commands", "cmds.json", "rest.json"]);
    assert_eq!(o.code, 0);
    assert_eq!(
        o.stdout,
        "model classifier:      1 (25.0%)\n    static allow:      3 (75.0%)\n"
    );
    // The deferred commands are handed to the caller to classify.
    assert_eq!(
        std::fs::read_to_string(s.dir.join("rest.json")).unwrap(),
        r#"["pip install requests"]"#
    );
}

/// A throwaway HTTP server that records the request path of everything it
/// receives and answers each with a minimal OpenAI-style model list. Used to
/// prove a no-model path never fetches model metadata over the network.
fn recording_server() -> (u16, Arc<Mutex<Vec<String>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let port = listener.local_addr().unwrap().port();
    let paths: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let rec = paths.clone();
    // Detached: the accept loop blocks forever and is reaped when the test
    // process exits. One leaked thread per call is fine for a test binary.
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let mut buf = [0u8; 2048];
            let n = stream.read(&mut buf).unwrap_or(0);
            let req = String::from_utf8_lossy(&buf[..n]);
            if let Some(path) = req.lines().next().and_then(|l| l.split_whitespace().nth(1)) {
                rec.lock().unwrap().push(path.to_string());
            }
            let body = r#"{"data":[{"id":"testmodel","max_model_len":4096}]}"#;
            let resp = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n\
                 Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = stream.write_all(resp.as_bytes());
        }
    });
    (port, paths)
}

/// `--run-tool` and `--dump-prompt` exit without a model turn, so they must not
/// pay the startup `model_info` fetch — a hosted provider's `/models` lookup
/// otherwise blocks these paths ~600ms over the network for nothing.
#[test]
fn no_model_paths_skip_the_model_info_fetch() {
    let (port, paths) = recording_server();
    let base = format!("http://127.0.0.1:{port}");
    let hit_model =
        |p: &Arc<Mutex<Vec<String>>>| p.lock().unwrap().iter().any(|s| s.contains("model"));

    let s = Sandbox::new();
    s.write("a.txt", "hi\n");

    // --run-tool executes a tool and exits; it ignores vision/num_ctx entirely.
    let o = s.run(&[
        "--provider",
        "openai",
        "--base-url",
        &base,
        "--model",
        "testmodel",
        "--run-tool",
        "Read",
        r#"{"file_path":"a.txt"}"#,
    ]);
    assert_eq!(o.code, 0, "{}", o.stderr);
    assert!(
        !hit_model(&paths),
        "run-tool fetched model info: {:?}",
        paths.lock().unwrap()
    );

    // --dump-prompt only prints the assembled prompt; its output is independent
    // of the model's window and vision support.
    let o = s.run(&[
        "--provider",
        "openai",
        "--base-url",
        &base,
        "--model",
        "testmodel",
        "--dump-prompt",
    ]);
    assert_eq!(o.code, 0, "{}", o.stderr);
    assert!(
        !hit_model(&paths),
        "dump-prompt fetched model info: {:?}",
        paths.lock().unwrap()
    );

    // Positive control: the recorder really captures a `/models` request, so the
    // assertions above mean "never asked", not "the server was unreachable".
    let mut stream = TcpStream::connect(("127.0.0.1", port)).expect("connect");
    stream
        .write_all(b"GET /v1/models HTTP/1.1\r\nHost: x\r\n\r\n")
        .unwrap();
    let _ = stream.read_to_end(&mut Vec::new());
    for _ in 0..200 {
        if hit_model(&paths) {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    assert!(
        hit_model(&paths),
        "recorder never saw /models: {:?}",
        paths.lock().unwrap()
    );
}

/// An OpenAI-compatible mock: records "METHOD path" for every request, answers
/// GET `/models` with a catalog carrying a context window (so model_info
/// resolves and is cacheable) and POST `/chat/completions` with a one-token SSE
/// so `-p` finishes. One request per connection (`Connection: close`).
fn model_server() -> (u16, Arc<Mutex<Vec<String>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let port = listener.local_addr().unwrap().port();
    let seen: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let rec = seen.clone();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            // Read the request head one byte at a time up to the blank line.
            let mut head = Vec::new();
            let mut b = [0u8; 1];
            while head.len() < 1 << 16 {
                if stream.read(&mut b).unwrap_or(0) == 0 {
                    break;
                }
                head.push(b[0]);
                if head.ends_with(b"\r\n\r\n") {
                    break;
                }
            }
            let head = String::from_utf8_lossy(&head);
            let mut words = head.lines().next().unwrap_or("").split_whitespace();
            let method = words.next().unwrap_or("").to_string();
            let path = words.next().unwrap_or("").to_string();
            rec.lock().unwrap().push(format!("{method} {path}"));
            // Drain the POST body so the client's write completes.
            let len = head
                .lines()
                .find_map(|l| {
                    l.to_ascii_lowercase()
                        .strip_prefix("content-length:")
                        .map(|v| v.trim().parse::<usize>().unwrap_or(0))
                })
                .unwrap_or(0);
            if len > 0 {
                let _ = stream.read_exact(&mut vec![0u8; len]);
            }
            let (ctype, body) = if method == "POST" {
                ("text/event-stream",
                 "data: {\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"done.\"}}]}\n\n\
                  data: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n\
                  data: [DONE]\n\n".to_string())
            } else {
                (
                    "application/json",
                    "{\"data\":[{\"id\":\"fake\",\"context_length\":128000}]}".to_string(),
                )
            };
            let resp = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: {ctype}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = stream.write_all(resp.as_bytes());
        }
    });
    (port, seen)
}

/// model_info is fetched over the network on the first start and cached on disk,
/// so a second start with the same home skips the ~600ms `/models` round-trip.
#[test]
fn model_info_is_cached_so_only_the_first_start_fetches_it() {
    let (port, seen) = model_server();
    let base = format!("http://127.0.0.1:{port}/v1");
    let s = Sandbox::new();
    let args = [
        "-p",
        "--provider",
        "openai",
        "--base-url",
        &base,
        "--model",
        "fake",
        "say hi",
    ];
    s.run(&args);
    s.run(&args);

    let reqs = seen.lock().unwrap();
    let model_gets = reqs.iter().filter(|r| r.contains("models")).count();
    let chat_posts = reqs.iter().filter(|r| r.starts_with("POST")).count();
    // Fetched once (first run); the second run reads the on-disk cache.
    assert_eq!(model_gets, 1, "model_info should be fetched once: {reqs:?}");
    // Both runs still reached the model.
    assert!(
        chat_posts >= 2,
        "each run should make a chat call: {reqs:?}"
    );
    assert!(
        s.dir.join("home/model-info-cache.json").exists(),
        "the cache file should be written"
    );
}

/// Guard the golden mechanism itself: every spelling of the sandbox path (raw
/// and canonicalized) must collapse to `<DIR>`, so the expectations above can't
/// silently stop matching on another platform.
#[test]
fn normalization_replaces_the_sandbox_path() {
    let s = Sandbox::new();
    let real = std::fs::canonicalize(&s.dir).unwrap();
    assert_eq!(s.normalize(&real.display().to_string()), "<DIR>");
    assert_eq!(s.normalize(&s.dir.display().to_string()), "<DIR>");
}
