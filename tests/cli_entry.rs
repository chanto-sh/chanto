//! The command line's entry points, run as the binary: subcommand dispatch and
//! its usage errors, `chanto setup --model`, `chanto migrate`, and print mode's
//! output formats against a local mock model server. No real provider, no
//! network beyond 127.0.0.1, and a throwaway home for every run.

use std::collections::VecDeque;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_chanto")
}

/// A project dir with its own home (`CHANTO_HOME`, and the other agents'
/// config homes), removed on drop.
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
            std::env::temp_dir().join(format!("chanto-entry-{}-{nanos}-{n}", std::process::id()));
        std::fs::create_dir_all(dir.join("home")).expect("create sandbox");
        Sandbox { dir }
    }

    fn home(&self) -> PathBuf {
        self.dir.join("home")
    }

    fn write(&self, name: &str, content: &str) -> PathBuf {
        let p = self.dir.join(name);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(&p, content).expect("write fixture");
        p
    }

    /// A default model on the mock server, as `chanto setup` would write it.
    fn use_server(&self, base: &str) {
        self.write(
            "home/config.toml",
            &format!("model = \"mock\"\nprovider = \"openai\"\nbase_url = \"{base}\"\n"),
        );
    }

    fn command(&self, args: &[&str]) -> Command {
        let mut c = Command::new(bin());
        c.args(args)
            .current_dir(&self.dir)
            .env("CHANTO_HOME", self.home())
            .env("CHANTO_COMPAT_HOME", self.home())
            .env("CODEX_HOME", self.home().join(".codex"))
            .env("CHANTO_NO_UPDATE_CHECK", "1")
            .env("CI", "1")
            .env_remove("CHANTO_MODEL")
            .env_remove("CHANTO_BASE_URL")
            .env_remove("CHANTO_PROVIDER");
        c
    }

    fn run(&self, args: &[&str]) -> Out {
        self.run_with_stdin(args, "")
    }

    fn run_with_stdin(&self, args: &[&str], stdin: &str) -> Out {
        let mut child = self
            .command(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("run chanto");
        child
            .stdin
            .take()
            .unwrap()
            .write_all(stdin.as_bytes())
            .unwrap();
        let out = child.wait_with_output().expect("wait for chanto");
        Out {
            stdout: String::from_utf8_lossy(&out.stdout).replace("\r\n", "\n"),
            stderr: String::from_utf8_lossy(&out.stderr).replace("\r\n", "\n"),
            code: out.status.code().unwrap_or(-1),
        }
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

#[derive(Debug)]
struct Out {
    stdout: String,
    stderr: String,
    code: i32,
}

/// An OpenAI-compatible server: GET `/models` lists `models`; each POST to
/// `/chat/completions` streams the next scripted answer (then "done."). Returns
/// the base URL (with `/v1`) and the request bodies it saw.
fn model_server(models: &[&str], answers: &[&str]) -> (String, Arc<Mutex<Vec<String>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let port = listener.local_addr().unwrap().port();
    let seen: Arc<Mutex<Vec<String>>> = Arc::default();
    let rec = seen.clone();
    let list = serde_json::json!({
        "data": models.iter().map(|m| serde_json::json!({"id": m})).collect::<Vec<_>>()
    })
    .to_string();
    let answers: Mutex<VecDeque<String>> =
        Mutex::new(answers.iter().map(|s| s.to_string()).collect());
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
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
            let head = String::from_utf8_lossy(&head).to_string();
            let post = head.starts_with("POST");
            let len = head
                .lines()
                .find_map(|l| {
                    l.to_ascii_lowercase()
                        .strip_prefix("content-length:")
                        .map(|v| v.trim().parse::<usize>().unwrap_or(0))
                })
                .unwrap_or(0);
            let mut body = vec![0u8; len];
            let _ = stream.read_exact(&mut body);
            let (ctype, out) = if post {
                rec.lock()
                    .unwrap()
                    .push(String::from_utf8_lossy(&body).to_string());
                let text = answers
                    .lock()
                    .unwrap()
                    .pop_front()
                    .unwrap_or_else(|| "done.".into());
                let delta = serde_json::json!({"choices":[{"index":0,"delta":{"role":"assistant","content":text}}]});
                (
                    "text/event-stream",
                    format!(
                        "data: {delta}\n\ndata: {{\"choices\":[{{\"index\":0,\"delta\":{{}},\"finish_reason\":\"stop\"}}],\"usage\":{{\"prompt_tokens\":10,\"completion_tokens\":2}}}}\n\ndata: [DONE]\n\n"
                    ),
                )
            } else {
                ("application/json", list.clone())
            };
            let resp = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: {ctype}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{out}",
                out.len()
            );
            let _ = stream.write_all(resp.as_bytes());
        }
    });
    (format!("http://127.0.0.1:{port}/v1"), seen)
}

// ------------------------------------------------------------ dispatch

#[test]
fn subcommand_help_and_usage() {
    let s = Sandbox::new();
    for (args, expect) in [
        (&["setup", "--help"][..], "Usage: chanto setup [--model"),
        (&["doctor", "-h"][..], "Usage: chanto doctor [--json]"),
        (&["models", "help"][..], "Usage: chanto models [<provider>]"),
        (&["logs", "--help"][..], "usage: chanto logs [--tail]"),
        (&["memory", "--help"][..], ""),
    ] {
        let o = s.run(args);
        assert_eq!(o.code, 0, "{args:?}: {o:?}");
        assert!(o.stdout.starts_with(expect), "{args:?}: {o:?}");
    }
    for cmd in ["goal", "team"] {
        let o = s.run(&[cmd]);
        assert_eq!(o.code, 0, "{o:?}");
        assert!(o.stdout.contains(&format!("chanto {cmd}")), "{o:?}");
        assert_eq!(s.run(&[cmd, "--help"]).stdout, o.stdout);
    }
    let o = s.run(&["help"]);
    assert_eq!(o.code, 0);
    assert!(o.stdout.contains("--print"), "{o:?}");
}

#[test]
fn bad_subcommand_arguments_fail() {
    let s = Sandbox::new();
    for (args, expect, code) in [
        (
            &["github", "uninstall"][..],
            "usage: chanto github install",
            1,
        ),
        (&["logs", "--bogus"][..], "unknown argument `--bogus`", 1),
        (
            &["--mode", "sideways", "-p", "x"][..],
            "unknown mode sideways",
            1,
        ),
        (&["--sandbox", "porous", "-p", "x"][..], "", 1),
        (&["run", "x"][..], "`chanto run` needs --bg", 1),
        (
            &["--source", "x"][..],
            "--source requires chanto customize",
            1,
        ),
        (&["customize"][..], "Usage: chanto customize <request>", 1),
        (&["acp-client"][..], "Usage: chanto acp-client <command>", 1),
        (
            &["acp-client", "some-agent", "-p"][..],
            "chanto acp-client is interactive; remove -p.",
            1,
        ),
        (
            &["--init", "x"][..],
            "--init and --maintenance only work with -p",
            2,
        ),
        (
            &["--input-format", "stream-json", "-p"][..],
            "--input-format stream-json requires --output-format stream-json",
            1,
        ),
        (
            &[
                "-p",
                "--json-schema",
                "{\"type\":\"object\"}",
                "--outcome",
                "x",
            ][..],
            "--json-schema can't be combined with --outcome",
            1,
        ),
        (
            &["-p", "--session-id", "abc", "x"][..],
            "--session-id must be a valid UUID",
            1,
        ),
        (
            &[
                "-p",
                "--session-id",
                "123e4567-e89b-12d3-a456-426614174000",
                "--continue",
                "x",
            ][..],
            "--session-id names a new session",
            1,
        ),
        (
            &["-p", "--permission-prompt-tool", "Bash", "x"][..],
            "--permission-prompt-tool takes an MCP tool name",
            1,
        ),
        (
            &["--worktree", "bad/name", "-p", "x"][..],
            "--worktree names can use",
            1,
        ),
        (&["migrate"][..], "Usage: chanto migrate <tool>", 1),
        (
            &["migrate", "windsurf"][..],
            "Couldn't find windsurf's config",
            1,
        ),
    ] {
        let o = s.run(args);
        assert_eq!(o.code, code, "{args:?}: {o:?}");
        assert!(o.stderr.contains(expect), "{args:?}: {o:?}");
    }
}

#[test]
fn completions_example_config_and_risk_tools() {
    let s = Sandbox::new();
    let o = s.run(&["--completions", "bash"]);
    assert_eq!(o.code, 0);
    assert!(o.stdout.contains("_chanto()"), "{o:?}");
    let o = s.run(&["--example-config"]);
    assert_eq!(o.code, 0);
    assert!(o.stdout.contains("model ="), "{o:?}");
    s.write(
        "cases.jsonl",
        "{\"cmd\":\"ls -la\",\"risky\":0}\n\n{\"cmd\":\"rm -rf /\",\"risky\":1}\nnot json\n",
    );
    let o = s.run(&["--eval-risk", "cases.jsonl", "scored.jsonl"]);
    assert_eq!(o.code, 0, "{o:?}");
    assert!(
        o.stdout.starts_with("commands: 2\nfast-allowed: ")
            && o.stdout.contains("labelled routine fast-allowed: 1/1"),
        "{o:?}"
    );
    let scored = std::fs::read_to_string(s.dir.join("scored.jsonl")).unwrap();
    assert_eq!(scored.lines().count(), 2);
    assert!(scored.contains("\"fast_allow\":true"), "{scored}");
}

#[test]
fn dump_config_redacts_secrets() {
    let s = Sandbox::new();
    s.write(
        "home/config.toml",
        "model = \"m\"\napi_key = \"sk-secret-value\"\n[providers.corp]\nkind = \"openai\"\nbase_url = \"http://127.0.0.1:1/v1\"\napi_key = \"another-secret\"\n",
    );
    let o = s.run(&["--dump-config"]);
    assert_eq!(o.code, 0, "{o:?}");
    assert!(
        !o.stdout.contains("sk-secret-value") && !o.stdout.contains("another-secret"),
        "{o:?}"
    );
    assert_eq!(o.stdout.matches("<redacted>").count(), 2, "{o:?}");
}

#[test]
fn init_only_runs_setup_and_exits() {
    let (base, seen) = model_server(&["mock"], &[]);
    let s = Sandbox::new();
    s.use_server(&base);
    let o = s.run(&["--init-only"]);
    assert_eq!(o.code, 0, "{o:?}");
    assert!(seen.lock().unwrap().is_empty(), "no model turn");
}

// ------------------------------------------------------------ setup

#[test]
fn setup_with_a_model_saves_it_and_checks_the_server() {
    let (base, _) = model_server(&["mock", "other"], &[]);
    let s = Sandbox::new();
    let o = s.run(&[
        "setup",
        "--model",
        "mock",
        "--provider",
        "openai",
        "--base-url",
        &base,
    ]);
    assert_eq!(o.code, 0, "{o:?}");
    assert!(
        o.stdout.starts_with("✓ Connected. Using mock.")
            && o.stdout.contains("Change it anytime with /model"),
        "{o:?}"
    );
    let cfg: toml::Table =
        toml::from_str(&std::fs::read_to_string(s.home().join("config.toml")).unwrap()).unwrap();
    assert_eq!(cfg["model"].as_str(), Some("mock"));
    assert_eq!(cfg["provider"].as_str(), Some("openai"));
    assert_eq!(cfg["base_url"].as_str(), Some(base.as_str()));

    let o = s.run(&["setup", "--model", "missing-model"]);
    assert_eq!(o.code, 0, "{o:?}");
    assert!(
        o.stdout
            .starts_with("Connected, but missing-model isn't available. Try mock, other"),
        "{o:?}"
    );
}

#[test]
fn setup_reports_an_unreachable_server_and_bad_input() {
    let s = Sandbox::new();
    let o = s.run(&[
        "setup",
        "--model",
        "m",
        "--provider",
        "openai",
        "--base-url",
        "http://127.0.0.1:1/v1",
    ]);
    assert_eq!(o.code, 0, "{o:?}");
    assert!(
        o.stdout
            .starts_with("Using m. Saved the default, but couldn't reach"),
        "{o:?}"
    );
    let o = s.run(&["setup", "--model", "m", "--provider", "carrier-pigeon"]);
    assert_eq!(o.code, 1);
    assert!(o.stderr.contains("Unknown provider"), "{o:?}");
    let o = s.run(&["setup", "--model", "m", "--base-url", "ftp://x"]);
    assert_eq!(o.code, 1);
    assert!(
        o.stderr.contains("Base URL must use http or https"),
        "{o:?}"
    );
}

// ------------------------------------------------------------ migrate

#[test]
fn migrate_proposes_then_applies_a_config() {
    let answer = "```toml\nmodel = \"openai/gpt-5\"\n[permissions]\nallow = [\"Bash(npm test:*)\"]\n```\n\n## What mapped\n- the model\n\n## Skipped\n- rules";
    let (base, seen) = model_server(&["mock"], &[answer, answer]);
    let s = Sandbox::new();
    s.use_server(&base);
    s.write("aider.yml", "model: gpt-5\n");
    let o = s.run(&["migrate", "aider", "--path", "aider.yml"]);
    assert_eq!(o.code, 0, "{o:?}");
    assert!(
        o.stdout
            .starts_with("Proposed chanto config from your aider setup:")
            && o.stdout.contains("model = \"openai/gpt-5\"")
            && o.stdout.contains("## What mapped")
            && o.stdout.contains("re-run with `--apply`"),
        "{o:?}"
    );
    assert!(
        seen.lock().unwrap()[0].contains("model: gpt-5"),
        "the source config is sent to the model"
    );
    // Nothing was written yet.
    let before = std::fs::read_to_string(s.home().join("config.toml")).unwrap();
    assert!(!before.contains("gpt-5"));

    let o = s.run(&["migrate", "--path", "aider.yml", "--apply"]);
    assert_eq!(o.code, 0, "{o:?}");
    assert!(
        o.stdout
            .starts_with("✓ Merged your another agent settings into")
            && o.stdout
                .contains("(previous config saved as config.toml.bak)"),
        "{o:?}"
    );
    let cfg: toml::Table =
        toml::from_str(&std::fs::read_to_string(s.home().join("config.toml")).unwrap()).unwrap();
    assert_eq!(cfg["model"].as_str(), Some("openai/gpt-5"));
    assert_eq!(
        cfg["provider"].as_str(),
        Some("openai"),
        "other keys are kept"
    );
    assert_eq!(
        std::fs::read_to_string(s.home().join("config.toml.bak")).unwrap(),
        before
    );
}

#[test]
fn migrate_explains_what_did_not_map() {
    let (base, _) = model_server(
        &["mock"],
        &[
            "## What mapped\n- nothing\n\n## Skipped\n- instructions: put them in AGENTS.md",
            "No settings here.",
            "```toml\nthis is = not toml [\n```",
            "```toml\nmode = 42\n```",
            "```toml\nmode = 42\n```",
        ],
    );
    let s = Sandbox::new();
    s.use_server(&base);
    s.write(".cursorrules", "Always use tabs.\n");
    let o = s.run(&["migrate", "cursor"]);
    assert_eq!(o.code, 0, "{o:?}");
    assert!(
        o.stdout.starts_with(
            "Nothing in your cursor setup maps to a chanto config.toml.\n\n## What mapped"
        ),
        "{o:?}"
    );
    let o = s.run(&["migrate", "cursor"]);
    assert!(o.stdout.ends_with("No settings here.\n"), "{o:?}");
    let o = s.run(&["migrate", "cursor"]);
    assert_eq!(o.code, 1);
    assert!(o.stderr.contains("isn't valid TOML"), "{o:?}");
    // Valid TOML that isn't a chanto config: shown with a warning, never applied.
    let o = s.run(&["migrate", "cursor"]);
    assert_eq!(o.code, 0, "{o:?}");
    assert!(o.stdout.contains("didn't fully validate"), "{o:?}");
    let o = s.run(&["migrate", "cursor", "--apply"]);
    assert_eq!(o.code, 1);
    assert!(o.stderr.contains("Won't apply"), "{o:?}");
}

#[test]
fn migrate_needs_a_source_and_a_model() {
    let s = Sandbox::new();
    s.write("empty.json", "  \n");
    let o = s.run(&["migrate", "--path", "empty.json"]);
    assert_eq!(o.code, 1);
    assert!(o.stderr.contains("empty.json is empty"), "{o:?}");
    s.write("cfg.json", "{}");
    let o = s.run(&["migrate", "--path", "cfg.json"]);
    assert_eq!(o.code, 1);
    assert!(o.stderr.contains("No model is set up"), "{o:?}");
    let o = s.run(&["migrate", "--path", "nope.json"]);
    assert_eq!(o.code, 1);
    assert!(o.stderr.contains("Reading nope.json"), "{o:?}");
}

// ------------------------------------------------------------ print mode

#[test]
fn print_mode_formats() {
    let (base, seen) = model_server(&["mock"], &["The answer is 4."]);
    let s = Sandbox::new();
    s.use_server(&base);
    let o = s.run(&["-p", "what is 2+2?"]);
    assert_eq!(o.code, 0, "{o:?}");
    assert_eq!(o.stdout, "The answer is 4.\n");
    assert!(seen.lock().unwrap()[0].contains("what is 2+2?"));

    let o = s.run(&["-p", "again", "--output-format", "json"]);
    assert_eq!(o.code, 0, "{o:?}");
    let v: serde_json::Value = serde_json::from_str(o.stdout.trim()).unwrap();
    assert_eq!(v["type"], "result");
    assert_eq!(v["result"], "done.");
    assert_eq!(v["is_error"], false);
    assert_eq!(v["model"], "mock");
    assert!(v["session_id"].is_string());

    let o = s.run(&["exec", "streamed", "--json"]);
    assert_eq!(o.code, 0, "{o:?}");
    let last: serde_json::Value =
        serde_json::from_str(o.stdout.trim().lines().last().unwrap()).unwrap();
    assert_eq!(last["type"], "result");
    assert!(
        o.stdout.lines().count() > 1,
        "events stream before the result"
    );

    // `--verbose` shows the live transcript.
    let o = s.run(&["-p", "--verbose", "show your work"]);
    assert_eq!(o.code, 0, "{o:?}");
    assert!(o.stdout.contains("done."), "{o:?}");
}

#[test]
fn print_mode_reads_the_prompt_from_stdin() {
    let (base, seen) = model_server(&["mock"], &[]);
    let s = Sandbox::new();
    s.use_server(&base);
    let o = s.run_with_stdin(&["-p", "summarize this:"], "line one\nline two\n");
    assert_eq!(o.code, 0, "{o:?}");
    let body = seen.lock().unwrap()[0].clone();
    assert!(
        body.contains("summarize this:\\n\\nline one\\nline two"),
        "{body}"
    );
    let o = s.run_with_stdin(&["-p"], "   \n");
    assert_eq!(o.code, 1);
    assert!(o.stderr.contains("no prompt given"), "{o:?}");
}

#[test]
fn print_mode_validates_json_schema_output() {
    let (base, seen) = model_server(&["mock"], &["not json at all", "{\"n\": 4}"]);
    let s = Sandbox::new();
    s.use_server(&base);
    let schema = r#"{"type":"object","properties":{"n":{"type":"integer"}},"required":["n"]}"#;
    let o = s.run(&["-p", "count", "--json-schema", schema]);
    assert_eq!(o.code, 0, "{o:?}");
    assert_eq!(o.stdout.trim(), "{\"n\":4}");
    assert_eq!(seen.lock().unwrap().len(), 2, "one repair turn");
    let o = s.run(&["-p", "x", "--json-schema", "{\"type\": 5}"]);
    assert_eq!(o.code, 1);
    assert!(
        o.stderr
            .contains("--json-schema is not a valid JSON Schema"),
        "{o:?}"
    );
}

#[test]
fn stream_json_input_runs_each_message() {
    let (base, _) = model_server(&["mock"], &["first", "second"]);
    let s = Sandbox::new();
    s.use_server(&base);
    let input = [
        r#"{"type":"control_request","request_id":"r1","request":{"subtype":"initialize"}}"#,
        r#"{"type":"user","message":{"role":"user","content":"one"}}"#,
        "",
        r#"{"type":"assistant"}"#,
        r#"{"type":"user","message":{"role":"user","content":[{"type":"text","text":"two"}]}}"#,
    ]
    .join("\n");
    let o = s.run_with_stdin(
        &[
            "-p",
            "--input-format",
            "stream-json",
            "--output-format",
            "stream-json",
        ],
        &input,
    );
    assert_eq!(o.code, 1, "a bad line fails the run: {o:?}");
    let events: Vec<serde_json::Value> = o
        .stdout
        .lines()
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect();
    let of = |t: &str| events.iter().filter(|e| e["type"] == t).collect::<Vec<_>>();
    assert_eq!(of("control_response").len(), 1);
    assert_eq!(of("error").len(), 1);
    let results = of("result");
    assert_eq!(results.len(), 2, "{o:?}");
    assert_eq!(results[0]["result"], "first");
    assert_eq!(results[1]["result"], "second");
    let o = s.run(&[
        "-p",
        "prompt",
        "--input-format",
        "stream-json",
        "--output-format",
        "stream-json",
    ]);
    assert_eq!(o.code, 1);
    assert!(o.stderr.contains("don't pass a prompt argument"), "{o:?}");
}

#[test]
fn print_mode_session_flags() {
    let (base, _) = model_server(&["mock"], &[]);
    let s = Sandbox::new();
    s.use_server(&base);
    let o = s.run(&["-p", "--continue", "hello"]);
    assert_eq!(o.code, 0, "{o:?}");
    assert!(o.stderr.contains("No previous session to resume."), "{o:?}");
    let o = s.run(&["-p", "--resume", "no-such-session", "hello"]);
    assert_eq!(o.code, 1);
    assert!(
        o.stderr
            .contains("Could not resume session no-such-session"),
        "{o:?}"
    );
    let o = s.run(&["-p", "--from-pr", "42", "hello"]);
    assert_eq!(o.code, 1);
    assert!(
        o.stderr.contains("No session is linked to pull request 42"),
        "{o:?}"
    );
    let o = s.run(&["-p", "--agent", "nobody", "hello"]);
    assert_eq!(o.code, 1);
    assert!(
        o.stderr.contains("--agent: no subagent named nobody"),
        "{o:?}"
    );
    let o = s.run(&["-p", "--system-prompt-file", "missing.md", "hello"]);
    assert_eq!(o.code, 1, "{o:?}");
    let o = s.run(&["-p", "--include-partial-messages", "hello"]);
    assert_eq!(o.code, 0, "{o:?}");
    assert!(
        o.stderr.contains("--include-partial-messages only applies"),
        "{o:?}"
    );
    // A `name/model` spec goes to that provider, so `--base-url` can't apply.
    let o = s.run(&[
        "-p",
        "--base-url",
        &base,
        "--model",
        "openrouter/some-model",
        "hello",
    ]);
    assert_eq!(o.code, 1);
    assert!(
        o.stderr.contains("doesn't apply to openrouter/some-model"),
        "{o:?}"
    );
}

#[test]
fn print_mode_continues_the_latest_session() {
    let (base, seen) = model_server(&["mock"], &["noted", "you said kumquat"]);
    let s = Sandbox::new();
    s.use_server(&base);
    let o = s.run(&["-p", "remember kumquat", "--output-format", "json"]);
    assert_eq!(o.code, 0, "{o:?}");
    let id = serde_json::from_str::<serde_json::Value>(o.stdout.trim()).unwrap()["session_id"]
        .as_str()
        .unwrap()
        .to_string();
    let o = s.run(&[
        "-p",
        "--resume",
        &id,
        "what did I say?",
        "--output-format",
        "json",
    ]);
    assert_eq!(o.code, 0, "{o:?}");
    let v: serde_json::Value = serde_json::from_str(o.stdout.trim()).unwrap();
    assert_eq!(v["session_id"], id.as_str());
    assert!(seen.lock().unwrap()[1].contains("remember kumquat"));
    let o = s.run(&["-p", "--continue", "and now?"]);
    assert_eq!(o.code, 0, "{o:?}");
    assert!(!o.stderr.contains("No previous session"), "{o:?}");
}
