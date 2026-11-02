use super::*;

#[test]
fn tracks_activity_tokens_and_log() {
    let r = Registry::default();
    r.start(
        "call_1",
        "agent-abc",
        "Explore",
        "Find the parser",
        "Look for the parser",
        false,
    );
    assert_eq!(r.rows(Instant::now())[0].activity, "Find the parser");
    r.tool_start("call_1", "t1", "Grep(fn parse)");
    assert_eq!(r.rows(Instant::now())[0].activity, "Grep(fn parse)");
    r.tool_end("call_1", "t1", "3 matches", false);
    r.text("call_1", "Found it.\n## Reading ");
    r.text("call_1", "parser.rs");
    r.tokens("call_1", 1234);
    let row = &r.rows(Instant::now())[0];
    assert_eq!(row.activity, "Reading parser.rs");
    assert_eq!(row.tokens, 1234);
    let a = &r.snapshot()[0];
    assert_eq!(a.log.len(), 3);
    assert!(
        matches!(&a.log[1], LogItem::Tool { done: Some((_, false, d)), .. } if d == "3 matches")
    );
    // Events for unknown keys are ignored.
    r.text("other", "x");
    assert_eq!(r.snapshot().len(), 1);
}

#[test]
fn finished_rows_leave_or_linger() {
    let r = Registry::default();
    r.start("a", "agent-a", "general-purpose", "ok", "p", false);
    r.start("b", "agent-b", "general-purpose", "bad", "p", true);
    r.finish("a", true);
    r.finish("b", false);
    let now = Instant::now();
    let rows = r.rows(now);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].status, RowStatus::Failed);
    assert!(r.rows(now + LINGER).is_empty());
    r.dismiss("b");
    assert!(r.rows(now).is_empty());
    // Still viewable after it leaves the panel.
    assert!(r.get_if_newer("a", None).is_some());
    // Resuming under the same key brings it back.
    r.start("b", "agent-b", "general-purpose", "bad", "more", true);
    assert_eq!(r.rows(Instant::now()).len(), 1);
}

#[test]
fn log_is_bounded() {
    let r = Registry::default();
    r.start("a", "agent-a", "x", "d", "p", false);
    for i in 0..MAX_LOG + 50 {
        r.tool_start("a", &format!("t{i}"), "Read");
    }
    assert_eq!(r.snapshot()[0].log.len(), MAX_LOG);
}
