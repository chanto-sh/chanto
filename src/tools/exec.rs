//! Exec: run a short Python script in which the read-only tools are functions.
//!
//! The model batches work (read 50 files, grep them, print the three lines that
//! matter) inside the interpreter, so the bulk never enters the context — only
//! what the script `print`s comes back.
//!
//! SECURITY: prototype. The harness restricts imports to a whitelist and strips
//! dangerous builtins, but it is not a kernel sandbox. It runs as
//! [`ToolKind::Execute`], so the permission model still asks before it runs;
//! production must run it inside the same sandbox as `Bash`.

use super::{Ctx, Tool, ToolKind, ToolOutput, arg_str, arg_u64};
use anyhow::Result;
use async_trait::async_trait;
use serde_json::{Value, json};
use std::process::Stdio;
use std::sync::OnceLock;
use tokio::io::AsyncWriteExt;

const MAX_OUTPUT: usize = 20_000;

const HARNESS: &str = r#"
import sys, re, json, math, pathlib, importlib.abc

ROOT = pathlib.Path(sys.argv[1]).resolve()

def _p(p):
    q = (ROOT / p).resolve()
    if q != ROOT and ROOT not in q.parents:
        raise ValueError("path outside workspace: %s" % p)
    return q

def read(path):
    return _p(path).read_text(encoding="utf-8", errors="replace")

def glob(pattern):
    return sorted(str(q.relative_to(ROOT)) for q in ROOT.glob(pattern) if q.is_file())

def grep(pattern, paths):
    rx = re.compile(pattern)
    out = []
    for p in paths:
        for i, line in enumerate(read(p).splitlines(), 1):
            if rx.search(line):
                out.append("%s:%d: %s" % (p, i, line.strip()[:120]))
    return out

ALLOWED = {"re", "json", "math", "collections", "itertools", "statistics", "functools"}

class Block(importlib.abc.MetaPathFinder):
    def find_spec(self, name, path, target=None):
        if name.split(".")[0] not in ALLOWED:
            raise ImportError("module not allowed in Exec: %s" % name)
        return None

sys.meta_path.insert(0, Block())

src = sys.stdin.read()
g = {
    "__name__": "__main__",
    "read": read, "glob": glob, "grep": grep, "print": print,
    "len": len, "range": range, "enumerate": enumerate, "zip": zip,
    "sorted": sorted, "min": min, "max": max, "sum": sum,
    "any": any, "all": all, "str": str, "int": int, "float": float,
    "bool": bool, "list": list, "dict": dict, "set": set, "tuple": tuple,
}
exec(compile(src, "<exec>", "exec"), g)
"#;

pub struct Exec;

fn on_path(name: &str) -> bool {
    let Some(path) = std::env::var_os("PATH") else {
        return false;
    };
    let exts: &[&str] = if cfg!(windows) {
        &["", ".exe", ".cmd", ".bat"]
    } else {
        &[""]
    };
    for dir in std::env::split_paths(&path) {
        for ext in exts {
            let cand = dir.join(format!("{name}{ext}"));
            if !cand.is_file() {
                continue;
            }
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                if let Ok(m) = cand.metadata()
                    && m.permissions().mode() & 0o111 != 0
                {
                    return true;
                }
            }
            #[cfg(not(unix))]
            return true;
        }
    }
    false
}

static PROGRAM: OnceLock<Option<&'static str>> = OnceLock::new();

/// The Python interpreter Exec will run, if one is installed.
pub fn program() -> Option<&'static str> {
    *PROGRAM.get_or_init(|| ["python3", "python"].into_iter().find(|n| on_path(n)))
}

/// Whether Exec can run at all. The registry only advertises the tool when
/// this is true, so the model never sees a tool that cannot work here.
pub fn available() -> bool {
    program().is_some()
}

fn truncate(s: &str) -> String {
    if s.chars().count() <= MAX_OUTPUT {
        s.to_string()
    } else {
        let mut out: String = s.chars().take(MAX_OUTPUT).collect();
        out.push_str("\n... (output truncated)");
        out
    }
}

#[async_trait]
impl Tool for Exec {
    fn name(&self) -> String {
        "Exec".into()
    }
    fn description(&self) -> String {
        "Run a short Python script where `read(path)`, `glob(pattern)` and \
         `grep(pattern, paths)` are available. Use it for bulk work — reading or \
         grepping many files — because only what the script `print`s enters the \
         context, not the files themselves. Returning 10 lines beats reading 1,000. \
         Do NOT use it for a single symbol lookup (Grep is cheaper) or when the \
         answer fits in one `Read`."
            .into()
    }
    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "script": {"type": "string", "description": "Python source to run; print() what you want to see"},
                "timeout": {"type": "integer", "description": "Timeout in milliseconds (default 30000)"}
            },
            "required": ["script"]
        })
    }
    fn kind(&self) -> ToolKind {
        ToolKind::Execute
    }
    fn label(&self, args: &Value) -> String {
        let n = args["script"]
            .as_str()
            .map(|s| s.lines().count())
            .unwrap_or(0);
        format!("Exec({n} lines)")
    }
    async fn run(&self, args: Value, ctx: &Ctx) -> Result<ToolOutput> {
        let script = arg_str(&args, "script")?.to_string();
        let cwd = ctx.cwd();
        let timeout_ms = arg_u64(&args, "timeout")
            .unwrap_or(30_000)
            .clamp(1_000, 120_000);

        let Some(prog) = program() else {
            return Ok(ToolOutput::err(
                "Exec needs python3 (or python) on PATH; it is disabled on this machine.",
            ));
        };

        let harness =
            std::env::temp_dir().join(format!("chanto-exec-{}.py", uuid::Uuid::new_v4().simple()));
        std::fs::write(&harness, HARNESS)?;

        let mut cmd = tokio::process::Command::new(prog);
        cmd.arg(&harness)
            .arg(&cwd)
            .current_dir(&cwd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = match cmd.spawn() {
            Ok(c) => c,
            Err(e) => {
                let _ = std::fs::remove_file(&harness);
                return Ok(ToolOutput::err(format!("could not run {prog}: {e}")));
            }
        };
        if let Some(mut stdin) = child.stdin.take()
            && let Err(e) = stdin.write_all(script.as_bytes()).await
        {
            let _ = std::fs::remove_file(&harness);
            return Ok(ToolOutput::err(format!("could not send script: {e}")));
        }

        let waited = tokio::time::timeout(
            std::time::Duration::from_millis(timeout_ms),
            child.wait_with_output(),
        )
        .await;
        let _ = std::fs::remove_file(&harness);

        match waited {
            Err(_) => Ok(ToolOutput::err(format!(
                "Exec timed out after {timeout_ms} ms"
            ))),
            Ok(Err(e)) => Ok(ToolOutput::err(format!("Exec failed: {e}"))),
            Ok(Ok(out)) => {
                let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
                let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
                if out.status.success() {
                    let body = if stdout.trim().is_empty() {
                        "(no output — print() what you want to see)".to_string()
                    } else {
                        truncate(&stdout)
                    };
                    Ok(ToolOutput::ok(body).display("Exec done"))
                } else {
                    Ok(ToolOutput::err(truncate(&format!("{stdout}\n{stderr}"))))
                }
            }
        }
    }
}
