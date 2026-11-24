use super::*;
use serde_json::json;

fn call(id: &str, name: &str, args: serde_json::Value) -> Message {
    let mut m = Message::assistant("");
    m.tool_calls = vec![ToolCall {
        id: id.into(),
        name: name.into(),
        arguments: args,
    }];
    m
}
fn result(id: &str, name: &str, content: &str) -> Message {
    Message::tool(
        &ToolCall {
            id: id.into(),
            name: name.into(),
            arguments: json!({}),
        },
        content.to_string(),
    )
}
fn kinds(f: &[Finding]) -> Vec<&str> {
    f.iter().map(|f| f.kind).collect()
}

#[test]
fn whole_file_read_then_small_edit() {
    let big = "x".repeat(20_000);
    let msgs = vec![
        call("1", "Read", json!({"file_path": "src/lib.rs"})),
        result("1", "Read", &big),
        call(
            "2",
            "Edit",
            json!({"file_path": "src/lib.rs", "old_string": "a - b", "new_string": "a + b"}),
        ),
        result("2", "Edit", "ok"),
    ];
    let f = analyze(&msgs, &Stats::default(), 32768);
    assert_eq!(kinds(&f), vec!["whole-file-reads"], "{f:?}");
    assert!(f[0].tokens > 4000 && f[0].example.contains("src/lib.rs"));
}

#[test]
fn large_outputs_and_noisy_commands() {
    let msgs = vec![
        call("1", "Grep", json!({"pattern": "fn"})),
        result("1", "Grep", &"m\n".repeat(4000)),
        call("2", "Bash", json!({"command": "cargo build"})),
        result("2", "Bash", &"Compiling x\n".repeat(900)),
    ];
    let f = analyze(&msgs, &Stats::default(), 32768);
    assert_eq!(
        kinds(&f),
        vec!["noisy-commands"],
        "Grep at 7% isn't flagged; the 3k-token build log is"
    );
    let f = analyze(&msgs, &Stats::default(), 8192);
    assert!(kinds(&f).contains(&"large-outputs"), "{f:?}");
    let big = f.iter().find(|f| f.kind == "large-outputs").unwrap();
    assert_eq!(big.count, 2);
    assert!(
        big.suggestion.contains("Filter command output"),
        "advice follows the largest output: {big:?}"
    );
}

#[test]
fn repeated_reads_unless_changed() {
    let body = "line\n".repeat(200);
    let msgs = vec![
        call("1", "Read", json!({"file_path": "a.py"})),
        result("1", "Read", &body),
        call("2", "Read", json!({"file_path": "a.py"})),
        result("2", "Read", &body),
        call("3", "Write", json!({"file_path": "a.py", "content": "y"})),
        result("3", "Write", "ok"),
        call("4", "Read", json!({"file_path": "a.py"})),
        result("4", "Read", &body),
        call(
            "5",
            "Read",
            json!({"file_path": "a.py", "offset": 10, "limit": 5}),
        ),
        result("5", "Read", "part"),
    ];
    let f = analyze(&msgs, &Stats::default(), 32768);
    let r = f.iter().find(|f| f.kind == "repeated-reads").unwrap();
    assert_eq!(
        r.count, 1,
        "re-read after a write and partial reads are fine"
    );
}

#[test]
fn repeated_failures_reminders_and_compaction() {
    let fail = "FAILED test_x\n[exit code 1]";
    let mut msgs = vec![
        Message::user(
            "This session is being continued from a previous conversation that ran out of context. Summary...",
        ),
        call("1", "Bash", json!({"command": "pytest"})),
        result("1", "Bash", fail),
        call("2", "Bash", json!({"command": "pytest"})),
        result("2", "Bash", fail),
        call("3", "Bash", json!({"command": "pytest"})),
        result("3", "Bash", fail),
    ];
    msgs.push(Message::user("<system-reminder>You have made the same tool call 3 times in a row with identical arguments</system-reminder>"));
    msgs.push(Message::user(
        "<system-reminder>You are not done: verification failed.\n\nx</system-reminder>",
    ));
    msgs.push(Message::user(
        "<system-reminder>Your last response was empty. Continue</system-reminder>",
    ));
    let f = analyze(&msgs, &Stats::default(), 32768);
    let ks = kinds(&f);
    for k in [
        "repeated-failures",
        "loops",
        "gate-retries",
        "empty",
        "compactions",
    ] {
        assert!(ks.contains(&k), "{k} missing from {ks:?}");
    }
    assert_eq!(
        f.iter()
            .find(|f| f.kind == "repeated-failures")
            .unwrap()
            .count,
        2
    );
    assert_eq!(f[0].kind, "compactions", "ranked by tokens");
    // An edit between failures resets the count.
    let msgs = vec![
        call("1", "Bash", json!({"command": "pytest"})),
        result("1", "Bash", fail),
        call(
            "2",
            "Edit",
            json!({"file_path": "x.py", "old_string": "a", "new_string": "b"}),
        ),
        result("2", "Edit", "ok"),
        call("3", "Bash", json!({"command": "pytest"})),
        result("3", "Bash", fail),
    ];
    assert!(analyze(&msgs, &Stats::default(), 32768).is_empty());
}

#[test]
fn cache_breaks_from_prefix_changes() {
    let mut s = Stats::default();
    let mut msgs = vec![Message::system("sys"), Message::user("hi")];
    s.before_call("m", "[tools]", &msgs);
    msgs.push(Message::assistant("hello"));
    msgs.push(Message::user("more"));
    s.before_call("m", "[tools]", &msgs);
    assert!(s.cache_breaks.is_empty(), "appending keeps the prefix");
    msgs[0] = Message::system("sys v2");
    s.before_call("m", "[tools]", &msgs);
    s.before_call("m", "[tools, new]", &msgs);
    msgs[2].set_content("[old tool result cleared]");
    s.expect_break("old tool results cleared");
    s.before_call("m", "[tools, new]", &msgs);
    // A different effort re-renders the prompt even with the same messages.
    s.before_call("m low", "[tools, new]", &msgs);
    let causes: Vec<_> = s.cache_breaks.iter().map(|b| b.cause.as_str()).collect();
    assert_eq!(
        causes,
        vec![
            "system prompt changed",
            "tool list changed",
            "old tool results cleared",
            "request settings changed"
        ]
    );
    let f = analyze(&[], &s, 32768);
    assert_eq!(kinds(&f), vec!["cache-breaks"]);
    assert_eq!(f[0].count, 4);
    let r = report("test", &f, &s, 0);
    assert!(
        r.contains("Prompt-cache breaks") && r.contains("system prompt changed ×1"),
        "{r}"
    );
}

#[test]
fn live_hint_once() {
    let mut s = Stats::default();
    assert!(s.live_hint("Read", "small", 32768).is_none());
    let big = "x".repeat(40_000);
    assert!(
        s.live_hint("Read", &big, 32768)
            .unwrap()
            .contains("offset/limit")
    );
    assert!(
        s.live_hint("Read", &big, 32768).is_none(),
        "only once per session"
    );
}

#[test]
fn usage_tallies_by_window_day_and_model() {
    let now = chrono::DateTime::parse_from_rfc3339("2026-10-14T12:00:00+00:00")
        .unwrap()
        .timestamp();
    let sess = |cwd: &str, model: &str, updated: &str, cost: f64| {
        let mut s = crate::session::Session::new(std::path::Path::new(cwd), model);
        s.updated = updated.into();
        s.stats.total_cost_usd = cost;
        s.stats.prompt_tokens = 1000;
        (s, 2usize)
    };
    let all = vec![
        sess("/p", "a", "2026-10-14T10:00:00+00:00", 1.0),
        sess("/q", "b", "2026-10-12T10:00:00+00:00", 2.0),
        sess("/p", "a", "2026-09-30T10:00:00+00:00", 4.0),
        sess("/p", "a", "2026-08-01T10:00:00+00:00", 8.0),
    ];
    let out = usage(&all, std::path::Path::new("/p"), now);
    assert!(
        out.contains("| This project, last 7 days | 1 | 2 | 1.0k / 0 | $1.00 |"),
        "{out}"
    );
    assert!(
        out.contains("| This project, last 30 days | 2 | 4 | 2.0k / 0 | $5.00 |"),
        "{out}"
    );
    assert!(
        out.contains("| All projects, last 7 days | 2 | 4 | 2.0k / 0 | $3.00 |"),
        "{out}"
    );
    assert!(
        out.contains("| All projects, last 30 days | 3 | 6 | 3.0k / 0 | $7.00 |"),
        "{out}"
    );
    assert!(
        out.contains("| 2026-10-14 | 1 |")
            && out.contains("| 2026-10-12 | 1 |")
            && !out.contains("| 2026-09-30 |"),
        "{out}"
    );
    // By model, costliest first; the August session is out of range.
    assert!(
        out.find("| a | 2 |").unwrap() < out.find("| b | 1 |").unwrap(),
        "{out}"
    );
    // Under a dollar, cents would round cheap models to $0.00.
    let cheap = usage(
        &[sess("/p", "a", "2026-10-14T10:00:00+00:00", 0.0123)],
        std::path::Path::new("/p"),
        now,
    );
    assert!(cheap.contains("| $0.0123 |"), "{cheap}");
}
