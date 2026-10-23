//! The event loop's handlers: pastes, the tick, prompts answered elsewhere
//! and what comes back from the editor.

use super::app_tests::*;
use super::*;

#[tokio::test]
async fn pastes_go_into_the_prompt() {
    let mut t = test_app();
    assert!(!t.on_paste("a\r\nb"));
    assert_eq!(t.input.text, "a\nb");
    // A long paste collapses to a tag.
    let mut t = test_app();
    let long = (0..20)
        .map(|i| format!("l{i}"))
        .collect::<Vec<_>>()
        .join("\n");
    t.on_paste(&long);
    assert_eq!(t.input.text, "[Pasted text #1 +20 lines]");
    assert_eq!(t.pastes, std::slice::from_ref(&long));
    // Unless that's turned off.
    let mut t = test_app_with(|c| c.collapse_pastes = Some(false), false);
    t.on_paste(&long);
    assert_eq!(t.input.text, long);
    // A file path becomes a mention, spaces escaped.
    let mut t = test_app();
    let f = t.dir.join("my notes.txt");
    std::fs::write(&f, "x").unwrap();
    t.on_paste(&format!("'{}'", f.display()));
    assert_eq!(
        t.input.text,
        format!("@{} ", f.display().to_string().replace(' ', "\\ "))
    );
    // A slash at the start opens the command popup.
    let mut t = test_app();
    t.on_paste("/mo");
    assert!(t.popup.is_some());
}

#[tokio::test]
async fn dropped_images_become_tags() {
    let mut t = test_app();
    let img = t.dir.join("shot.png");
    std::fs::write(&img, b"\x89PNG\r\n\x1a\n").unwrap();
    assert!(t.on_paste(&img.display().to_string()));
    assert_eq!(t.input.text, "[Image #1] ");
    assert_eq!(t.images.len(), 1);
}

#[tokio::test]
async fn the_tick_asks_for_a_redraw_only_when_something_moves() {
    let mut t = test_app();
    assert!(!t.on_tick(), "nothing animating");
    t.hint = Some(("hi".into(), Instant::now()));
    assert!(t.on_tick());
    t.hint = None;
    let (tx, _rx) = oneshot::channel();
    let mut req = permission(None);
    req.auto_deny = Some(Duration::from_secs(30));
    t.on_msg(AppMsg::Permission(req, tx));
    assert!(t.on_tick(), "the auto-deny countdown ticks");
    t.dialog = None;
    // A Monitor report waiting between turns starts one: redraw.
    t.env
        .notifications
        .lock()
        .unwrap()
        .push("<system-reminder>Monitor build: ok</system-reminder>".into());
    assert!(t.on_tick());
    assert!(t.busy);
}

#[tokio::test]
async fn prompts_answered_elsewhere_close_and_the_next_opens() {
    let mut t = test_app();
    assert!(!t.drop_answered_dialogs());
    let (tx1, rx1) = oneshot::channel();
    let (tx2, rx2) = oneshot::channel();
    let (tx3, _rx3) = oneshot::channel();
    t.on_msg(AppMsg::Permission(permission(None), tx1));
    t.on_msg(AppMsg::Question("q2".into(), vec![], false, tx2));
    t.on_msg(AppMsg::Question("q3".into(), vec![], false, tx3));
    assert_eq!(t.pending_dialogs.len(), 2);
    drop(rx1);
    drop(rx2);
    assert!(t.drop_answered_dialogs());
    assert!(matches!(&t.dialog, Some(Dialog::Question { q, .. }) if q == "q3"));
    assert!(t.pending_dialogs.is_empty());
    assert!(!t.drop_answered_dialogs());
}

#[tokio::test]
async fn the_editor_result_answers_or_fills_the_prompt() {
    // Unchanged proposal: a plain yes.
    let mut t = test_app();
    let (tx, mut rx) = oneshot::channel();
    t.on_msg(AppMsg::Permission(permission(Some("body")), tx));
    t.apply_edit(Some("body".into()), Ok("body".into()));
    assert_eq!(rx.try_recv().unwrap(), PermissionReply::Allow);
    assert!(t.dialog.is_none());
    // Edited: allowed with the new content.
    let (tx, mut rx) = oneshot::channel();
    t.on_msg(AppMsg::Permission(permission(Some("body")), tx));
    t.apply_edit(Some("body".into()), Ok("better".into()));
    assert_eq!(
        rx.try_recv().unwrap(),
        PermissionReply::AllowEdited("better".into())
    );
    // The prompt itself.
    t.apply_edit(None, Ok("edited prompt\n\n".into()));
    assert_eq!(t.input.text, "edited prompt");
    t.apply_edit(None, Err(anyhow::anyhow!("vi exited with 1")));
    assert_eq!(t.hint_text(), "Editor failed: vi exited with 1");
}

#[cfg(unix)]
#[test]
fn the_editor_round_trips_the_text() {
    let _g = crate::compat::TEST_HOME_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let prev = std::env::var_os("VISUAL");
    let with = |editor: &str| {
        // SAFETY: test-only, under TEST_HOME_LOCK.
        unsafe { std::env::set_var("VISUAL", editor) };
        edit_in_editor("keep me")
    };
    // `true` leaves the file as written; `false` fails.
    let kept = with("true");
    let failed = with("false");
    // SAFETY: as above.
    unsafe {
        match prev {
            Some(v) => std::env::set_var("VISUAL", v),
            None => std::env::remove_var("VISUAL"),
        }
    }
    assert_eq!(kept.unwrap(), "keep me");
    assert!(
        failed
            .unwrap_err()
            .to_string()
            .starts_with("false exited with")
    );
}
