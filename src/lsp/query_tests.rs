//! Queries and edit diagnostics through a scripted language server on stdio,
//! configured the way a user configures one.

use super::*;
use crate::agent::tests::{make_env_with, tmpdir};
use crate::permissions::Mode;
use crate::test_support::isolated;
use crate::tools::ToolOutput;

const SERVER: &str = r#"
import json, sys
def send(m):
    b = json.dumps(m).encode()
    sys.stdout.buffer.write(b"Content-Length: %d\r\n\r\n" % len(b) + b)
    sys.stdout.buffer.flush()
def recv():
    n = None
    while True:
        l = sys.stdin.buffer.readline()
        if not l: sys.exit(0)
        if l in (b"\r\n", b"\n"): break
        if l.lower().startswith(b"content-length:"): n = int(l.split(b":")[1])
    return json.loads(sys.stdin.buffer.read(n))
log = open(sys.argv[1], "a")
docs = {}
def publish(uri, text, version):
    items = []
    for i, line in enumerate(text.split("\n")):
        if "ERR" in line:
            items.append({"severity": 1, "message": "bad thing", "source": "fake",
              "range": {"start": {"line": i, "character": 0}, "end": {"line": i, "character": 1}}})
        if "WARN" in line:
            items.append({"severity": 2, "message": "odd thing",
              "range": {"start": {"line": i, "character": 2}, "end": {"line": i, "character": 3}}})
    send({"jsonrpc": "2.0", "method": "textDocument/publishDiagnostics",
          "params": {"uri": uri, "version": version, "diagnostics": items}})
while True:
    m = recv()
    method, mid = m.get("method"), m.get("id")
    if method is None:
        log.write("reply %s\n" % json.dumps(m)); log.flush(); continue
    p = m.get("params") or {}
    if method == "initialize":
        send({"jsonrpc": "2.0", "id": 900, "method": "workspace/configuration", "params": {"items": [{}, {}]}})
        send({"jsonrpc": "2.0", "id": 901, "method": "workspace/workspaceFolders"})
        send({"jsonrpc": "2.0", "id": 902, "method": "client/registerCapability"})
        send({"jsonrpc": "2.0", "id": 903, "method": "made/up"})
        send({"jsonrpc": "2.0", "method": "$/progress", "params": {"token": "t", "value": {"kind": "begin"}}})
        send({"jsonrpc": "2.0", "method": "$/progress", "params": {"token": "t", "value": {"kind": "report"}}})
        send({"jsonrpc": "2.0", "method": "$/progress", "params": {"token": "t", "value": {"kind": "end"}}})
        send({"jsonrpc": "2.0", "method": "experimental/serverStatus", "params": {"quiescent": True}})
        send({"jsonrpc": "2.0", "method": "window/logMessage", "params": {"message": "hi"}})
        send({"jsonrpc": "2.0", "id": mid, "result": {"capabilities": {}}})
    elif method == "textDocument/didOpen":
        d = p["textDocument"]; docs[d["uri"]] = d["text"]
        if "SILENT" not in d["text"]: publish(d["uri"], d["text"], d["version"])
    elif method == "textDocument/didChange":
        d = p["textDocument"]; docs[d["uri"]] = p["contentChanges"][0]["text"]
        publish(d["uri"], docs[d["uri"]], d["version"])
    elif method == "textDocument/definition":
        send({"jsonrpc": "2.0", "id": mid, "result": {"uri": p["textDocument"]["uri"],
              "range": {"start": {"line": 1, "character": 4}}}})
    elif method == "textDocument/references":
        log.write("references %s\n" % json.dumps(p)); log.flush()
        loc = {"uri": p["textDocument"]["uri"], "range": {"start": {"line": 0, "character": 0}}}
        send({"jsonrpc": "2.0", "id": mid, "result": [loc] * 101})
    elif method == "textDocument/implementation":
        send({"jsonrpc": "2.0", "id": mid, "result": None})
    elif method == "textDocument/typeDefinition":
        send({"jsonrpc": "2.0", "id": mid, "result": [{"targetUri": p["textDocument"]["uri"],
              "targetSelectionRange": {"start": {"line": 1, "character": 0}}}]})
    elif method == "textDocument/hover":
        pos = p["position"]
        if "CRASH" in docs[p["textDocument"]["uri"]]: sys.exit(1)
        if pos["line"] == 0:
            send({"jsonrpc": "2.0", "id": mid, "result": {"contents": {"kind": "markdown", "value": "at col %d" % pos["character"]}}})
        elif pos["line"] == 1:
            send({"jsonrpc": "2.0", "id": mid, "result": {"contents": ["one", {"language": "rs", "value": "two"}]}})
        elif pos["line"] == 2:
            send({"jsonrpc": "2.0", "id": mid, "error": {"code": -1, "message": "hover refused"}})
        else:
            send({"jsonrpc": "2.0", "id": mid, "result": {"contents": ""}})
    elif method == "textDocument/documentSymbol":
        if "NOSYMBOLS" in docs[p["textDocument"]["uri"]]:
            send({"jsonrpc": "2.0", "id": mid, "result": []})
        else:
            send({"jsonrpc": "2.0", "id": mid, "result": [
              {"name": "Outer", "kind": 5, "selectionRange": {"start": {"line": 0}}, "children": [
                {"name": "inner", "kind": 6, "range": {"start": {"line": 1}}}]},
              {"name": "flat", "kind": 12, "location": {"range": {"start": {"line": 2}}}},
              {"name": "weird", "kind": 99}]})
    elif method == "workspace/symbol":
        uri = next(iter(docs), "file:///none")
        if p["query"] == "none":
            send({"jsonrpc": "2.0", "id": mid, "result": None})
        else:
            send({"jsonrpc": "2.0", "id": mid, "result": [
              {"name": "found", "kind": 12, "location": {"uri": uri, "range": {"start": {"line": 1, "character": 0}}}},
              {"name": "uriOnly", "kind": 23, "location": {"uri": uri}}]})
    elif mid is not None:
        send({"jsonrpc": "2.0", "id": mid, "result": None})
"#;

struct Project {
    dir: PathBuf,
    log: PathBuf,
    env: Arc<Env>,
}

/// A trusted project with a Python language server for `.fk` files.
fn project(on: bool) -> Project {
    let dir = crate::util::canonical(tmpdir("lsp-query")).unwrap();
    let script = dir.join("server.py");
    let log = dir.join("server.log");
    std::fs::write(&script, SERVER).unwrap();
    crate::permissions::set_project_root(&dir);
    set_trusted(true);
    let (s, l) = (script.display().to_string(), log.display().to_string());
    let env = make_env_with("http://127.0.0.1:9", &dir, Mode::Bypass, 32768, |c| {
        c.lsp.enabled = Some(on);
        c.lsp.diagnostics_wait_ms = Some(5000);
        c.lsp.servers.insert(
            "fake".into(),
            LspServerConfig {
                command: "python3".into(),
                args: vec![s, l],
                extensions: vec![".fk".into()],
                disabled: false,
            },
        );
    });
    Project { dir, log, env }
}

impl Project {
    fn file(&self, name: &str, text: &str) -> PathBuf {
        let p = self.dir.join(name);
        std::fs::write(&p, text).unwrap();
        p
    }

    async fn ask(&self, op: &str, file: &Path, line: u64, col: u64) -> Result<String> {
        query(&self.env, op, Some(file), line, col, "").await
    }
}

#[cfg(unix)]
#[tokio::test]
async fn queries_format_each_answer_shape() {
    if isolated("lsp::query_tests::queries_format_each_answer_shape") {
        return;
    }
    let p = project(true);
    let f = p.file("a.fk", "first line\nsecond line\nthird\nfourth\n");
    let ask = |op: &'static str, line, col| p.ask(op, &f, line, col);

    assert_eq!(ask("hover", 1, 3).await.unwrap(), "at col 2");
    assert_eq!(ask("hover", 2, 1).await.unwrap(), "one\n\ntwo");
    assert_eq!(ask("hover", 4, 1).await.unwrap(), "No hover information");
    let e = ask("hover", 3, 1).await.unwrap_err();
    assert!(e.to_string().contains("hover refused"), "{e}");

    assert_eq!(
        ask("definition", 1, 1).await.unwrap(),
        "a.fk:2:5: second line"
    );
    assert_eq!(
        ask("typeDefinition", 1, 1).await.unwrap(),
        "a.fk:2:1: second line"
    );
    assert_eq!(ask("implementation", 1, 1).await.unwrap(), "No results");
    let refs = ask("references", 1, 1).await.unwrap();
    assert_eq!(refs.lines().count(), 101);
    assert!(refs.ends_with("… and 1 more"), "{refs}");
    let log = std::fs::read_to_string(&p.log).unwrap();
    assert!(log.contains("\"includeDeclaration\": true"), "{log}");

    assert_eq!(
        ask("documentSymbol", 1, 1).await.unwrap(),
        "class Outer L1\n  method inner L2\nfunction flat L3\nsymbol weird L1"
    );
    let f2 = p.file("b.fk", "NOSYMBOLS\n");
    assert_eq!(
        p.ask("documentSymbol", &f2, 1, 1).await.unwrap(),
        "No symbols"
    );

    let e = ask("rename", 1, 1).await.unwrap_err();
    assert!(e.to_string().contains("unknown operation `rename`"), "{e}");
    let e = query(&p.env, "hover", None, 1, 1, "").await.unwrap_err();
    assert_eq!(e.to_string(), "file_path is required for hover");

    // The server's own requests were answered, and an unknown one refused.
    let log = std::fs::read_to_string(&p.log).unwrap();
    assert!(
        log.contains(r#""id": 900, "jsonrpc": "2.0", "result": [null, null]"#),
        "{log}"
    );
    assert!(
        log.contains(r#""id": 901, "jsonrpc": "2.0", "result": [{""#),
        "{log}"
    );
    assert!(
        log.contains(r#""id": 902, "jsonrpc": "2.0", "result": null"#),
        "{log}"
    );
    assert!(
        log.contains(r#""error": {"code": -32601, "message": "method not found"}, "id": 903"#),
        "{log}"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn workspace_symbols_search_the_given_files_server() {
    if isolated("lsp::query_tests::workspace_symbols_search_the_given_files_server") {
        return;
    }
    let p = project(true);
    let f = p.file("a.fk", "one\ntwo\n");
    p.ask("hover", &f, 1, 1).await.unwrap();
    let out = query(&p.env, "workspaceSymbol", Some(&f), 0, 0, "x")
        .await
        .unwrap();
    assert_eq!(
        out,
        "function found — a.fk:2:1: two\nstruct uriOnly — a.fk:1:1: one"
    );
    let out = query(&p.env, "workspaceSymbol", Some(&f), 0, 0, "none")
        .await
        .unwrap();
    assert_eq!(out, "No results");
    // No file and no marker in the project: nothing to ask.
    let e = query(&p.env, "workspaceSymbol", None, 0, 0, "x")
        .await
        .unwrap_err();
    assert_eq!(
        e.to_string(),
        "no language server available for this project"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn diagnostics_report_errors_and_warnings_on_request() {
    if isolated("lsp::query_tests::diagnostics_report_errors_and_warnings_on_request") {
        return;
    }
    let p = project(true);
    let f = p.file("a.fk", "fine\nERR\n  WARN\n");
    let out = p.ask("diagnostics", &f, 0, 0).await.unwrap();
    assert!(out.contains("L2:1 error: bad thing [fake]"), "{out}");
    assert!(out.contains("L3:3 warning: odd thing"), "{out}");
    // Unchanged: answered from what the server already said.
    assert_eq!(p.ask("diagnostics", &f, 0, 0).await.unwrap(), out);
    let clean = p.file("clean.fk", "fine\n");
    assert_eq!(
        p.ask("diagnostics", &clean, 0, 0).await.unwrap(),
        "No errors or warnings"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn edits_append_only_errors_to_the_tool_output() {
    if isolated("lsp::query_tests::edits_append_only_errors_to_the_tool_output") {
        return;
    }
    let p = project(true);
    let f = p.file("a.fk", "ok\nERR\n  WARN\n");
    let mut out = ToolOutput::ok("Wrote a.fk");
    let args = json!({"file_path": "a.fk"});
    after_tool(&p.env, "Write", &args, &mut out).await;
    assert!(
        out.content.contains("<diagnostics file=\"a.fk\">"),
        "{}",
        out.content
    );
    assert!(
        out.content.contains("L2:1 error: bad thing [fake]"),
        "{}",
        out.content
    );
    assert!(!out.content.contains("odd thing"), "{}", out.content);
    assert!(
        out.display.as_deref().unwrap().contains("1 error reported"),
        "{:?}",
        out.display
    );

    // A fix clears them, and two broken files in one patch both report.
    std::fs::write(&f, "ok\n").unwrap();
    let mut out = ToolOutput::ok("Edited").display("shown");
    after_tool(&p.env, "Edit", &args, &mut out).await;
    assert_eq!(out.content, "Edited");
    assert_eq!(out.display.as_deref(), Some("shown"));

    p.file("x.fk", "ERR\n");
    p.file("y.fk", "ERR\nERR\n");
    let patch = json!({"input": "*** Update File: x.fk\n*** Add File: y.fk\n"});
    let mut out = ToolOutput::ok("Patched").display("shown");
    after_tool(&p.env, "apply_patch", &patch, &mut out).await;
    assert!(
        out.content.contains("file=\"x.fk\"") && out.content.contains("file=\"y.fk\""),
        "{}",
        out.content
    );
    let shown = out.display.unwrap();
    assert!(
        shown.starts_with("shown\n") && shown.contains("3 errors reported"),
        "{shown}"
    );

    // Failed edits and other tools ask nothing.
    let mut out = ToolOutput::err("nope");
    after_tool(&p.env, "Write", &args, &mut out).await;
    assert_eq!(out.content, "nope");
    let mut out = ToolOutput::ok("listed");
    after_tool(&p.env, "Glob", &args, &mut out).await;
    assert_eq!(out.content, "listed");
}

#[cfg(unix)]
#[tokio::test]
async fn reading_a_file_warms_its_server() {
    if isolated("lsp::query_tests::reading_a_file_warms_its_server") {
        return;
    }
    let p = project(true);
    let f = p.file("a.fk", "ERR\n");
    let mut out = ToolOutput::ok("read");
    after_tool(&p.env, "Read", &json!({"file_path": "a.fk"}), &mut out).await;
    assert_eq!(out.content, "read", "a read reports nothing");
    let spec = spec_for(&p.env.config.lsp, &f).unwrap();
    for _ in 0..2000 {
        let key = (spec.name.clone(), root_for(&spec, &f, &p.dir));
        let cell = clients().lock().unwrap().get(&key).cloned();
        if let Some(Some(c)) = cell.as_ref().and_then(|c| c.get().cloned())
            && c.cached(&path_to_uri(&f)).is_some()
        {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("the read didn't open the file on a server");
}

#[cfg(unix)]
#[tokio::test]
async fn a_disabled_or_untrusted_setup_starts_nothing() {
    if isolated("lsp::query_tests::a_disabled_or_untrusted_setup_starts_nothing") {
        return;
    }
    let p = project(false);
    let f = p.file("a.fk", "ERR\n");
    let mut out = ToolOutput::ok("wrote");
    after_tool(&p.env, "Write", &json!({"file_path": "a.fk"}), &mut out).await;
    assert_eq!(out.content, "wrote");
    assert!(!p.log.exists(), "no server ran while off");

    // Servers run project code, so an untrusted folder doesn't get a custom one.
    let p = project(true);
    let f2 = p.file("b.fk", "ERR\n");
    set_trusted(false);
    let e = p.ask("hover", &f2, 1, 1).await.unwrap_err();
    assert!(
        e.to_string().contains("no language server available"),
        "{e}"
    );
    assert_eq!(diagnostics_after_edit(&p.env, &f2).await, None);
    assert!(!p.log.exists(), "no server ran untrusted");

    // Nor does a file outside the project, or one nobody serves.
    set_trusted(true);
    let e = p.ask("hover", &f, 1, 1).await.unwrap_err();
    assert!(
        e.to_string().contains("no language server available"),
        "{e}"
    );
    let other = p.file("c.unserved", "x");
    assert!(p.ask("hover", &other, 1, 1).await.is_err());
}

#[cfg(unix)]
#[tokio::test]
async fn a_crashed_server_fails_its_request_and_is_replaced() {
    if isolated("lsp::query_tests::a_crashed_server_fails_its_request_and_is_replaced") {
        return;
    }
    let p = project(true);
    let f = p.file("a.fk", "CRASH\n");
    let e = p.ask("hover", &f, 1, 1).await.unwrap_err();
    assert!(e.to_string().contains("language server exited"), "{e}");
    // Let the reader see the exit, then the next call gets a new process.
    tokio::time::sleep(Duration::from_millis(200)).await;
    std::fs::write(&f, "fine\n").unwrap();
    assert_eq!(p.ask("hover", &f, 1, 1).await.unwrap(), "at col 0");
}

#[test]
fn config_merge_only_replaces_what_is_set() {
    let mut c = LspConfig {
        enabled: Some(false),
        diagnostics_wait_ms: Some(1),
        ..Default::default()
    };
    c.merge(LspConfig::default());
    assert_eq!((c.enabled, c.diagnostics_wait_ms), (Some(false), Some(1)));
    assert!(!c.on());
    let mut servers = BTreeMap::new();
    servers.insert("x".to_string(), LspServerConfig::default());
    c.merge(LspConfig {
        enabled: Some(true),
        diagnostics_wait_ms: Some(2),
        servers,
    });
    assert_eq!((c.enabled, c.diagnostics_wait_ms), (Some(true), Some(2)));
    assert!(c.on() && c.servers.contains_key("x"));
}

#[test]
fn configured_servers_replace_builtins_and_lose_their_safety_when_changed() {
    let server = |command: &str, args: &[&str], disabled| LspServerConfig {
        command: command.into(),
        args: args.iter().map(|s| s.to_string()).collect(),
        extensions: vec![],
        disabled,
    };
    let safe = builtin().into_iter().find(|b| b.safe_untrusted).unwrap();
    let spec_of = |c: LspServerConfig| {
        let mut cfg = LspConfig::default();
        cfg.servers.insert(safe.name.clone(), c);
        specs(&cfg).into_iter().find(|s| s.name == safe.name)
    };
    let same = spec_of(server(&safe.command, &[], false)).unwrap();
    assert!(same.safe_untrusted);
    assert_eq!(
        same.extensions, safe.extensions,
        "inherited from the builtin"
    );
    // Extra flags or another binary could run what the project chooses.
    assert!(
        !spec_of(server(&safe.command, &["--query-driver=/x"], false))
            .unwrap()
            .safe_untrusted
    );
    assert!(
        !spec_of(server("/tmp/other", &[], false))
            .unwrap()
            .safe_untrusted
    );
    // Disabling or emptying an entry leaves no custom server by that command.
    let disabled = spec_of(server("/tmp/other", &[], true));
    assert!(disabled.is_none_or(|s| s.command != "/tmp/other"));
}

#[test]
fn edited_paths_read_each_tools_arguments() {
    let dir = tmpdir("lsp-paths");
    let env = make_env_with("http://127.0.0.1:9", &dir, Mode::Bypass, 32768, |_| {});
    let names = |tool: &str, args: Value| -> Vec<String> {
        edited_paths(&env, tool, &args)
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().to_string())
            .collect()
    };
    assert_eq!(names("Write", json!({"file_path": "a"})), ["a"]);
    assert_eq!(names("MultiEdit", json!({"file_path": "b"})), ["b"]);
    assert!(names("Write", json!({})).is_empty());
    assert!(names("Bash", json!({"file_path": "a"})).is_empty());
    assert_eq!(
        names(
            "apply_patch",
            json!({"input": "*** Update File: u\n*** Add File: n\n*** Move to: m\nother"})
        ),
        ["u", "n", "m"]
    );
    assert_eq!(names("ApplyPatch", json!({"patch": "+++ b/g\n"})), ["g"]);
    assert_eq!(names("patch_file", json!({"diff": "+++ b/d\n"})), ["d"]);
    assert!(names("apply_patch", json!({})).is_empty());
}

#[test]
fn a_file_location_shows_its_line_or_nothing_when_unreadable() {
    let dir = tmpdir("lsp-loc");
    std::fs::write(dir.join("a.txt"), "zero\none\n").unwrap();
    let uri = path_to_uri(&dir.join("a.txt"));
    let range = json!({"start": {"line": 1, "character": 2}});
    assert_eq!(fmt_location(&dir, &uri, &range), "a.txt:2:3: one");
    let past = json!({"start": {"line": 9}});
    assert_eq!(fmt_location(&dir, &uri, &past), "a.txt:10:1: ");
    let gone = path_to_uri(&dir.join("gone.txt"));
    assert_eq!(fmt_location(&dir, &gone, &range), "gone.txt:2:3: ");
}

#[test]
fn document_symbols_stop_at_their_limit() {
    let many: Vec<Value> = (0..400)
        .map(|i| json!({"name": format!("s{i}"), "kind": 12}))
        .collect();
    let mut out = Vec::new();
    document_symbols(&json!(many), 0, &mut out);
    assert_eq!(out.len(), 300);
    assert_eq!(kind_name(&json!("x")), "");
    assert_eq!(kind_name(&json!(26)), "type parameter");
    assert_eq!(kind_name(&json!(27)), "symbol");
}

#[test]
fn rustup_tools_are_found_in_any_toolchain() {
    if isolated("lsp::query_tests::rustup_tools_are_found_in_any_toolchain") {
        return;
    }
    let home = tmpdir("lsp-rustup");
    let bin = home.join("toolchains/stable/bin");
    std::fs::create_dir_all(&bin).unwrap();
    std::fs::write(bin.join("rust-analyzer"), "").unwrap();
    // SAFETY: the test runs alone in its own child process.
    unsafe { std::env::set_var("RUSTUP_HOME", &home) };
    assert!(rustup_component_installed("rust-analyzer"));
    assert!(!rustup_component_installed("absent-tool"));
}
