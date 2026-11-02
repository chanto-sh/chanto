use super::*;

#[test]
fn counting_files_stops_at_the_limit() {
    let d = std::env::temp_dir().join(format!("chanto-lsp-count-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    for i in 0..5 {
        std::fs::write(d.join(format!("f{i}.c")), "int x;\n").unwrap();
    }
    assert!(more_files_than(&d, 4));
    assert!(!more_files_than(&d, 5));
    let _ = std::fs::remove_dir_all(&d);
}

fn builtin_spec(name: &str) -> Spec {
    builtin().into_iter().find(|s| s.name == name).unwrap()
}

#[tokio::test]
async fn a_small_project_is_counted_once() {
    let d = std::env::temp_dir().join(format!("chanto-lsp-small-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    std::fs::write(d.join("main.c"), "int main;\n").unwrap();
    assert!(!large_project(&d).await);
    assert_eq!(project_sizes().lock().unwrap().get(&d), Some(&false));
    // The answer is remembered, not recounted.
    project_sizes().lock().unwrap().insert(d.clone(), true);
    assert!(large_project(&d).await);
    project_sizes().lock().unwrap().remove(&d);
    let _ = std::fs::remove_dir_all(&d);
}

#[tokio::test]
async fn clangd_needs_a_compile_database_in_a_large_project() {
    let d = std::env::temp_dir().join(format!("chanto-lsp-large-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    let sub = d.join("drivers/net");
    std::fs::create_dir_all(&sub).unwrap();
    let clangd = builtin_spec("clangd");
    // A small project: clangd may run without a database.
    assert!(may_start(&clangd, &sub, &d).await);
    // A large one (marked so, rather than writing 20,000 files): only with one.
    project_sizes().lock().unwrap().insert(d.clone(), true);
    assert!(!may_start(&clangd, &sub, &d).await);
    std::fs::write(d.join("compile_commands.json"), "[]").unwrap();
    assert!(may_start(&clangd, &sub, &d).await);
    // Other servers aren't held back by size.
    let other = builtin().into_iter().find(|s| s.name != "clangd").unwrap();
    assert!(may_start(&other, &sub, &d).await);
    project_sizes().lock().unwrap().remove(&d);
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn a_compile_database_lets_clangd_run_anywhere() {
    let d = std::env::temp_dir().join(format!("chanto-lsp-db-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    let sub = d.join("kernel/sched");
    std::fs::create_dir_all(&sub).unwrap();
    assert!(!has_compile_db(&sub, &d));
    std::fs::write(d.join("compile_commands.json"), "[]").unwrap();
    assert!(has_compile_db(&sub, &d));
    let _ = std::fs::remove_dir_all(&d);
}

#[tokio::test]
async fn framing_roundtrip() {
    let mut buf: Vec<u8> = Vec::new();
    write_message(&mut buf, &json!({"a": "é"})).await.unwrap();
    write_message(&mut buf, &json!({"b": 2})).await.unwrap();
    let mut r = BufReader::new(&buf[..]);
    assert_eq!(read_message(&mut r).await.unwrap().unwrap()["a"], "é");
    assert_eq!(read_message(&mut r).await.unwrap().unwrap()["b"], 2);
    assert!(read_message(&mut r).await.unwrap().is_none());
}

#[test]
fn uri_roundtrip_and_positions() {
    let p = Path::new("/tmp/a b/ü.rs");
    assert_eq!(uri_to_path(&path_to_uri(p)), p);
    assert_eq!(
        position("x\n😀ab", 2, 2),
        json!({"line": 1, "character": 2})
    );
}

#[test]
fn formats_errors_only() {
    let items = vec![
        json!({"severity": 2, "message": "unused", "range": {"start": {"line": 0, "character": 0}}}),
        json!({"severity": 1, "message": "mismatched types\nmore", "source": "rustc", "code": "E0308", "range": {"start": {"line": 4, "character": 8}}}),
    ];
    let (t, n) = format_diags(&items, 1).unwrap();
    assert_eq!(n, 1);
    assert_eq!(t, "L5:9 error: mismatched types [rustc E0308]");
    assert!(format_diags(&items[..1], 1).is_none());
}

const FAKE: &str = r#"
import sys, json
def read():
    n = None
    while True:
        line = sys.stdin.buffer.readline()
        if not line: sys.exit(0)
        line = line.strip()
        if not line: break
        k, v = line.split(b':', 1)
        if k.lower() == b'content-length': n = int(v)
    return json.loads(sys.stdin.buffer.read(n))
def send(m):
    b = json.dumps(m).encode()
    sys.stdout.buffer.write(b'Content-Length: %d\r\n\r\n' % len(b) + b); sys.stdout.buffer.flush()
while True:
    m = read()
    meth = m.get('method')
    if meth == 'initialize':
        send({'jsonrpc': '2.0', 'id': 99, 'method': 'workspace/configuration', 'params': {'items': [{}]}})
        send({'jsonrpc': '2.0', 'id': m['id'], 'result': {'capabilities': {}}})
    elif meth in ('textDocument/didOpen', 'textDocument/didChange'):
        td = m['params']['textDocument']
        text = m['params']['textDocument'].get('text') or m['params']['contentChanges'][0]['text']
        diags = [{'severity': 1, 'message': 'bad thing', 'source': 'fake', 'range': {'start': {'line': i, 'character': 0}, 'end': {'line': i, 'character': 1}}}
                 for i, l in enumerate(text.split('\n')) if 'ERR' in l]
        send({'jsonrpc': '2.0', 'method': 'textDocument/publishDiagnostics', 'params': {'uri': td['uri'], 'version': td['version'], 'diagnostics': diags}})
    elif meth == 'textDocument/hover':
        send({'jsonrpc': '2.0', 'id': m['id'], 'result': {'contents': {'kind': 'markdown', 'value': 'fake hover %d' % m['params']['position']['line']}}})
    elif 'id' in m and meth:
        send({'jsonrpc': '2.0', 'id': m['id'], 'result': None})
"#;

#[tokio::test]
async fn fake_server_diagnostics_and_hover() {
    let dir = std::env::temp_dir().join(format!("chanto-lsp-{}", uuid::Uuid::new_v4().simple()));
    std::fs::create_dir_all(&dir).unwrap();
    let script = dir.join("fake_lsp.py");
    std::fs::write(&script, FAKE).unwrap();
    let spec = Spec {
        name: "fake".into(),
        command: "python3".into(),
        args: vec![script.display().to_string()],
        extensions: vec!["fk".into()],
        markers: vec![],
        wait_ms: 3000,
        settle_ms: 100,
        safe_untrusted: true,
    };
    let file = dir.join("a.fk");
    std::fs::write(&file, "ok\nERR here\n").unwrap();
    let c = client(&spec, &dir, Duration::from_secs(10))
        .await
        .expect("fake server starts");
    let after = *c.seq.borrow();
    let (uri, v, changed) = c.sync_disk(&file).await.unwrap();
    assert!(changed);
    let items = c
        .wait_diags(
            &uri,
            after,
            v,
            Instant::now() + Duration::from_secs(3),
            Duration::from_millis(100),
        )
        .await
        .unwrap();
    assert_eq!(
        format_diags(&items, 1).unwrap().0,
        "L2:1 error: bad thing [fake]"
    );
    // Fix the file: diagnostics clear.
    std::fs::write(&file, "ok\nfine\n").unwrap();
    let after = *c.seq.borrow();
    let (uri, v, _) = c.sync_disk(&file).await.unwrap();
    assert_eq!(v, 2);
    let items = c
        .wait_diags(
            &uri,
            after,
            v,
            Instant::now() + Duration::from_secs(3),
            Duration::from_millis(100),
        )
        .await
        .unwrap();
    assert!(format_diags(&items, 1).is_none());
    let h = c
        .request(
            "textDocument/hover",
            json!({"textDocument": {"uri": uri}, "position": {"line": 1, "character": 0}}),
            Duration::from_secs(3),
        )
        .await
        .unwrap();
    assert_eq!(hover_text(&h["contents"]), "fake hover 1");
    c.kill();
}
