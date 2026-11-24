use super::*;

#[test]
fn print_mode_reports_warnings_on_stderr() {
    // `-p` used to drop warnings, so "Stopped after 1 model calls
    // (max_turns)" ended with empty output, exit 0 and nothing said.
    let w = Event::Warn("Stopped after 1 model calls (max_turns).".into());
    assert_eq!(
        print_mode_diagnostic(&w),
        Some((false, "Stopped after 1 model calls (max_turns)."))
    );
    assert_eq!(
        print_mode_diagnostic(&Event::Error("boom".into())),
        Some((true, "boom"))
    );
    assert_eq!(print_mode_diagnostic(&Event::Text("answer".into())), None);
}

#[test]
fn quiet_hides_steps_but_not_notices() {
    assert!(quiet_hides(&Event::Text("Let me look".into())));
    assert!(quiet_hides(&Event::Reasoning("hmm".into())));
    assert!(quiet_hides(&Event::ToolStart {
        id: "1".into(),
        label: "Bash(ls)".into(),
        parent: None
    }));
    for ev in [
        Event::Warn("spend".into()),
        Event::Error("boom".into()),
        Event::Info("✓ Verified: cargo test".into()),
        Event::Busy(Some("Working".into())),
    ] {
        assert!(!quiet_hides(&ev));
    }
}
