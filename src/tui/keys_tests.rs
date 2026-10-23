//! Key handling: editing, global shortcuts, search, vim mode and the keys
//! each dialog takes.

use super::app_tests::*;
use super::*;

fn answer_permission(keys: &[KeyEvent]) -> (TestApp, Option<PermissionReply>) {
    let mut t = test_app();
    let (tx, mut rx) = oneshot::channel();
    t.on_msg(AppMsg::Permission(permission(Some("new content")), tx));
    for k in keys {
        t.on_key(*k);
    }
    let reply = rx.try_recv().ok();
    (t, reply)
}

fn question(
    t: &mut TestApp,
    options: &[&str],
    free_text: bool,
) -> oneshot::Receiver<Option<(usize, String)>> {
    let (tx, rx) = oneshot::channel();
    t.on_msg(AppMsg::Question(
        "Which one?".into(),
        options.iter().map(|s| s.to_string()).collect(),
        free_text,
        tx,
    ));
    rx
}

#[tokio::test]
async fn typing_and_line_editing() {
    let mut t = test_app();
    type_str(&mut t, "hello world");
    assert_eq!(t.input.text, "hello world");
    t.on_key(key(KeyCode::Backspace));
    t.on_key(key(KeyCode::Left));
    t.on_key(key(KeyCode::Delete));
    assert_eq!(t.input.text, "hello wor");
    t.on_key(ctrl('a'));
    assert_eq!(t.input.cursor, 0);
    t.on_key(ctrl('e'));
    assert_eq!(t.input.cursor, t.input.text.len());
    t.on_key(alt(KeyCode::Char('b')));
    assert_eq!(t.input.cursor, 6);
    t.on_key(alt(KeyCode::Char('f')));
    assert_eq!(t.input.cursor, 9);
    t.on_key(KeyEvent::new(KeyCode::Left, KeyModifiers::CONTROL));
    t.on_key(KeyEvent::new(KeyCode::Right, KeyModifiers::ALT));
    t.on_key(key(KeyCode::Home));
    t.on_key(key(KeyCode::End));
    t.on_key(ctrl('w'));
    assert_eq!(t.input.text, "hello ");
    t.on_key(alt(KeyCode::Backspace));
    assert_eq!(t.input.text, "");
    type_str(&mut t, "abc");
    t.on_key(key(KeyCode::Left));
    t.on_key(ctrl('k'));
    assert_eq!(t.input.text, "ab");
    t.on_key(ctrl('u'));
    assert_eq!(t.input.text, "");
    // Newlines: shift/alt+enter, ctrl+j, and a trailing backslash.
    type_str(&mut t, "a");
    t.on_key(shift(KeyCode::Enter));
    type_str(&mut t, "b");
    t.on_key(ctrl('j'));
    type_str(&mut t, "c\\");
    t.on_key(key(KeyCode::Enter));
    assert_eq!(t.input.text, "a\nb\nc\n");
    assert!(!t.busy, "none of these submitted");
    // Releases are ignored.
    let mut release = key(KeyCode::Char('z'));
    release.kind = KeyEventKind::Release;
    t.on_key(release);
    assert_eq!(t.input.text, "a\nb\nc\n");
    // An unhandled key does nothing.
    t.on_key(key(KeyCode::F(9)));
    assert_eq!(t.input.text, "a\nb\nc\n");
}

#[tokio::test]
async fn enter_submits() {
    let mut t = test_app();
    type_str(&mut t, "do it");
    t.on_key(key(KeyCode::Enter));
    assert!(t.busy && t.input.is_empty());
}

#[tokio::test]
async fn ctrl_c_clears_then_asks_before_quitting() {
    let mut t = test_app();
    type_str(&mut t, "draft");
    t.on_key(ctrl('c'));
    assert!(t.input.is_empty() && !t.quit);
    t.on_key(ctrl('c'));
    assert_eq!(t.hint_text(), "Press ctrl+c again to exit");
    assert!(!t.quit);
    t.on_key(ctrl('c'));
    assert!(t.quit);
    // While busy it interrupts instead.
    let mut t = test_app();
    t.busy = true;
    let c = CancellationToken::new();
    t.cancel = Some(c.clone());
    t.on_key(ctrl('c'));
    assert!(c.is_cancelled() && !t.quit);
}

#[tokio::test]
async fn ctrl_d_deletes_or_quits() {
    let mut t = test_app();
    type_str(&mut t, "ab");
    t.on_key(key(KeyCode::Home));
    t.on_key(ctrl('d'));
    assert_eq!(t.input.text, "b");
    t.input.set("");
    t.on_key(ctrl('d'));
    assert!(t.quit);
}

#[tokio::test]
async fn toggles_and_simple_shortcuts() {
    let mut t = test_app();
    t.on_key(ctrl('t'));
    assert!(!t.show_todos);
    t.on_key(ctrl('g'));
    assert!(t.open_editor);
    t.on_key(ctrl('l'));
    t.on_key(key(KeyCode::BackTab));
    assert_eq!(t.mode, Mode::AcceptEdits);
    t.on_key(shift(KeyCode::Tab));
    assert_eq!(t.mode, Mode::Plan);
    // Inline mode leaves selection and find to the terminal.
    let mut t = test_app_with(|_| {}, true);
    t.on_key(ctrl('s'));
    assert!(t.hint_text().starts_with("Inline mode leaves the mouse"));
    t.on_key(ctrl('f'));
    assert!(t.hint_text().contains("scrollback"));
    assert!(t.search.is_none());
    // ctrl+o in inline mode reprints instead of expanding.
    t.on_key(ctrl('o'));
    assert!(t.inline.as_ref().unwrap().reprint && !t.expand_all);
}

#[tokio::test]
async fn question_mark_opens_help_until_a_key() {
    let mut t = test_app();
    t.on_key(key(KeyCode::Char('?')));
    assert!(t.help && t.input.is_empty());
    t.on_key(key(KeyCode::Modifier(
        crossterm::event::ModifierKeyCode::LeftShift,
    )));
    assert!(t.help, "a bare modifier keeps it open");
    t.on_key(key(KeyCode::Char('x')));
    assert!(
        !t.help && t.input.is_empty(),
        "the closing key is swallowed"
    );
    // Mid-prompt, `?` is just text.
    type_str(&mut t, "why?");
    assert_eq!(t.input.text, "why?");
}

#[tokio::test]
async fn ctrl_x_chords() {
    let mut t = test_app();
    t.on_key(ctrl('x'));
    assert!(t.chord_x && t.hint_text().starts_with("ctrl+x"));
    t.on_key(ctrl('e'));
    assert!(t.open_editor && !t.chord_x && t.hint.is_none());
    // ctrl+x enter queues for after the turn.
    t.busy = true;
    type_str(&mut t, "after");
    t.on_key(ctrl('x'));
    t.on_key(key(KeyCode::Enter));
    assert_eq!(t.queue[0].0, "after");
    // ctrl+x ctrl+s sends now.
    type_str(&mut t, "now");
    t.on_key(ctrl('x'));
    t.on_key(ctrl('s'));
    assert!(t.send_now);
    assert_eq!(t.queue[0].0, "now");
    // Any other key ends the chord and acts as usual.
    t.on_key(ctrl('x'));
    t.on_key(key(KeyCode::Char('q')));
    assert!(!t.chord_x);
    assert_eq!(t.input.text, "q");
}

#[tokio::test]
async fn ctrl_enter_sends_now_and_enter_on_empty_runs_the_queue() {
    let mut t = test_app();
    t.busy = true;
    let c = CancellationToken::new();
    t.cancel = Some(c.clone());
    t.queue.push_back(("queued".into(), vec![]));
    t.on_key(key(KeyCode::Enter));
    assert!(c.is_cancelled() && t.send_now);
    assert_eq!(t.queue[0].0, "queued");
    let mut t = test_app();
    t.busy = true;
    type_str(&mut t, "urgent");
    t.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::CONTROL));
    assert!(t.send_now);
    assert_eq!(t.queue[0].0, "urgent");
}

#[tokio::test]
async fn esc_closes_the_nearest_thing_first() {
    let mut t = test_app();
    type_str(&mut t, "/mo");
    assert!(t.popup.is_some());
    t.on_key(key(KeyCode::Esc));
    assert!(t.popup.is_none() && t.input.text == "/mo");
    // Cancels an edit of an earlier prompt.
    t.editing_prompt = Some((0, "both"));
    t.on_key(key(KeyCode::Esc));
    assert!(t.editing_prompt.is_none() && t.input.is_empty());
    assert_eq!(t.hint_text(), "Edit cancelled");
    // Scrolled up: back to the bottom, even while busy.
    t.scroll = 7;
    t.busy = true;
    t.on_key(key(KeyCode::Esc));
    assert_eq!(t.scroll, 0);
    // Viewing a subagent: back to main.
    t.viewing = Some("agent-1".into());
    t.on_key(key(KeyCode::Esc));
    assert!(t.viewing.is_none());
    // Busy: interrupt.
    let c = CancellationToken::new();
    t.cancel = Some(c.clone());
    t.on_key(key(KeyCode::Esc));
    assert!(c.is_cancelled());
    // At rest, a double esc opens the rewind picker.
    t.busy = false;
    t.on_key(key(KeyCode::Esc));
    assert!(t.esc_at.is_some());
    t.on_key(key(KeyCode::Esc));
    assert!(
        t.sent()
            .iter()
            .any(|m| matches!(m, AppMsg::Picker { title, .. } if title.starts_with("Rewind")))
    );
}

#[tokio::test]
async fn paging_scrolls_by_the_transcript_height() {
    let mut t = test_app();
    t.transcript_area = Rect::new(0, 0, 80, 12);
    t.on_key(key(KeyCode::PageUp));
    assert_eq!(t.scroll, 10);
    t.on_key(key(KeyCode::PageUp));
    t.on_key(key(KeyCode::PageDown));
    assert_eq!(t.scroll, 10);
    // Arrows on an empty prompt walk history; past it, they scroll.
    t.scroll = 0;
    t.input = Input::default();
    t.on_key(key(KeyCode::Up));
    assert_eq!(t.scroll, 1);
    t.on_key(key(KeyCode::Down));
    assert_eq!(t.scroll, 0);
}

#[tokio::test]
async fn popup_navigation_wraps_and_tab_accepts() {
    let mut t = test_app();
    type_str(&mut t, "/m");
    assert_eq!(t.popup.as_ref().unwrap().items.len(), 2);
    t.on_key(key(KeyCode::Up));
    assert_eq!(t.popup.as_ref().unwrap().sel, 1);
    t.on_key(key(KeyCode::Down));
    assert_eq!(t.popup.as_ref().unwrap().sel, 0);
    t.on_key(key(KeyCode::Down));
    t.on_key(key(KeyCode::Tab));
    assert_eq!(t.input.text, "/mode ");
    assert!(t.popup.is_none());
    // A file popup takes Enter too.
    std::fs::write(t.dir.join("notes.txt"), "").unwrap();
    t.input.set("");
    type_str(&mut t, "@note");
    assert!(t.popup.as_ref().unwrap().kind == PopupKind::File);
    t.on_key(key(KeyCode::Enter));
    assert_eq!(t.input.text, "@notes.txt ");
    assert!(!t.busy);
}

#[tokio::test]
async fn tab_takes_the_suggestion() {
    let mut t = test_app();
    t.on_msg(AppMsg::Suggestion("run the tests".into()));
    t.on_key(key(KeyCode::Tab));
    assert_eq!(t.input.text, "run the tests");
    assert!(t.suggestion.is_none());
    // Not offered while typing.
    t.on_msg(AppMsg::Suggestion("other".into()));
    assert!(t.suggestion.is_none());
}

#[tokio::test]
async fn history_search_finds_older_matches_and_restores_on_esc() {
    let mut t = test_app();
    t.input = Input::with_history(vec![
        "cargo build".into(),
        "fix the parser".into(),
        "cargo test".into(),
    ]);
    type_str(&mut t, "draft");
    t.on_key(ctrl('r'));
    assert!(t.search.is_some());
    type_str(&mut t, "cargo");
    assert_eq!(t.input.text, "cargo test");
    t.on_key(ctrl('r'));
    assert_eq!(t.input.text, "cargo build");
    t.on_key(key(KeyCode::Backspace));
    assert_eq!(t.search.as_ref().unwrap().query, "carg");
    type_str(&mut t, "zzz");
    assert!(!t.search.as_ref().unwrap().found);
    t.on_key(key(KeyCode::Esc));
    assert!(t.search.is_none());
    assert_eq!(t.input.text, "draft", "esc restores what was typed");
    // Enter keeps the match.
    t.on_key(ctrl('r'));
    type_str(&mut t, "parser");
    t.on_key(key(KeyCode::Enter));
    assert!(t.search.is_none());
    assert_eq!(t.input.text, "fix the parser");
    // ctrl+c leaves without restoring; other keys are ignored.
    t.on_key(ctrl('r'));
    t.on_key(key(KeyCode::F(5)));
    t.on_key(ctrl('c'));
    assert!(t.search.is_none());
    // No history: never found.
    let mut t = test_app();
    t.input = Input::default();
    t.on_key(ctrl('r'));
    type_str(&mut t, "x");
    assert!(!t.search.as_ref().unwrap().found);
}

#[tokio::test]
async fn transcript_search_highlights_and_scrolls_to_hits() {
    let mut t = test_app();
    for i in 0..30 {
        t.push(Kind::Markdown(format!("line {i}")));
    }
    t.push(Kind::Markdown("needle near the bottom".into()));
    t.push(Kind::Markdown("needle at the bottom".into()));
    t.transcript_area = Rect::new(0, 0, 60, 10);
    t.on_key(ctrl('f'));
    type_str(&mut t, "line 3");
    let s = t.search.as_ref().unwrap();
    assert!(s.found);
    let hl = t.highlight.unwrap();
    assert!(t.scroll > 0 && hl > 4, "scrolled to the hit (hl {hl})");
    // The empty query clears the highlight.
    for _ in 0..6 {
        t.on_key(key(KeyCode::Backspace));
    }
    assert!(t.highlight.is_none());
    type_str(&mut t, "needle");
    let newest = t.highlight.unwrap();
    t.on_key(key(KeyCode::Up));
    let older = t.highlight.unwrap();
    assert!(older > newest, "up finds the older hit");
    t.on_key(key(KeyCode::Down));
    assert_eq!(t.highlight, Some(newest));
    type_str(&mut t, "zzz");
    assert!(t.highlight.is_none() && !t.search.as_ref().unwrap().found);
    t.on_key(key(KeyCode::Esc));
    assert!(t.search.is_none() && t.input.is_empty());
}

#[tokio::test]
async fn vim_mode_motions_and_operators() {
    let mut t = test_app();
    t.vim = true;
    // No history, so up/down (and j/k) only move within the prompt.
    t.input = Input::default();
    type_str(&mut t, "one two three");
    t.on_key(key(KeyCode::Esc));
    assert!(!t.vim_insert);
    // Motions.
    t.on_key(key(KeyCode::Char('0')));
    assert_eq!(t.input.cursor, 0);
    t.on_key(key(KeyCode::Char('w')));
    assert_eq!(t.input.cursor, 3);
    t.on_key(key(KeyCode::Char('l')));
    t.on_key(key(KeyCode::Char('h')));
    t.on_key(key(KeyCode::Char('b')));
    assert_eq!(t.input.cursor, 0);
    t.on_key(key(KeyCode::Char('$')));
    assert_eq!(t.input.cursor, 13);
    for k in [
        KeyCode::Left,
        KeyCode::Right,
        KeyCode::Up,
        KeyCode::Down,
        KeyCode::Home,
        KeyCode::End,
        KeyCode::Backspace,
        KeyCode::Tab,
        KeyCode::Char('j'),
        KeyCode::Char('k'),
        KeyCode::Char('Z'),
    ] {
        t.on_key(key(k));
    }
    assert_eq!(t.input.text, "one two three", "motions don't edit");
    // x deletes, u undoes.
    t.on_key(key(KeyCode::Char('0')));
    t.on_key(key(KeyCode::Char('x')));
    assert_eq!(t.input.text, "ne two three");
    t.on_key(key(KeyCode::Char('u')));
    assert_eq!(t.input.text, "one two three");
    // dw, d$, D, dd.
    t.on_key(key(KeyCode::Char('$')));
    t.on_key(key(KeyCode::Char('d')));
    t.on_key(key(KeyCode::Char('w')));
    assert_eq!(t.input.text, "one two ");
    t.on_key(key(KeyCode::Char('0')));
    t.on_key(key(KeyCode::Char('w')));
    t.on_key(key(KeyCode::Char('d')));
    t.on_key(key(KeyCode::Char('$')));
    assert_eq!(t.input.text, "one");
    t.on_key(key(KeyCode::Char('0')));
    t.on_key(key(KeyCode::Char('D')));
    assert_eq!(t.input.text, "");
    t.on_key(key(KeyCode::Char('u')));
    t.on_key(key(KeyCode::Char('d')));
    t.on_key(key(KeyCode::Char('d')));
    assert_eq!(t.input.text, "");
    t.on_key(key(KeyCode::Char('u')));
    assert_eq!(t.input.text, "one");
    // An unknown operator target cancels the operator.
    t.on_key(key(KeyCode::Char('d')));
    t.on_key(key(KeyCode::Char('q')));
    assert_eq!(t.input.text, "one");
    // Entering insert mode: i, I, a, A.
    t.on_key(key(KeyCode::Char('I')));
    assert!(t.vim_insert && t.input.cursor == 0);
    t.on_key(key(KeyCode::Esc));
    t.on_key(key(KeyCode::Char('A')));
    assert_eq!(t.input.cursor, 3);
    type_str(&mut t, "more");
    t.on_key(key(KeyCode::Esc));
    t.on_key(key(KeyCode::Char('0')));
    t.on_key(key(KeyCode::Char('a')));
    assert!(t.vim_insert && t.input.cursor == 1);
    t.on_key(key(KeyCode::Esc));
    t.on_key(key(KeyCode::Char('i')));
    assert!(t.vim_insert);
    // Ctrl chords still work in normal mode.
    t.on_key(key(KeyCode::Esc));
    t.on_key(ctrl('t'));
    assert!(!t.show_todos);
    // Enter in normal mode submits.
    t.on_key(key(KeyCode::Enter));
    assert!(t.busy && t.input.is_empty());
}

#[tokio::test]
async fn vim_undo_history_is_bounded_and_deduplicated() {
    let mut t = test_app();
    t.input.set("x");
    t.vim_snapshot();
    t.vim_snapshot();
    assert_eq!(t.vim_undo.len(), 1);
    for i in 0..150 {
        t.input.set(&i.to_string());
        t.vim_snapshot();
    }
    assert_eq!(t.vim_undo.len(), 100);
    assert_eq!(t.vim_undo[0], "50");
}

#[tokio::test]
async fn permission_keys_answer_the_prompt() {
    let (t, r) = answer_permission(&[key(KeyCode::Char('y'))]);
    assert_eq!(r, Some(PermissionReply::Allow));
    assert!(t.dialog.is_none());
    let (_, r) = answer_permission(&[key(KeyCode::Char('2'))]);
    assert_eq!(r, Some(PermissionReply::AllowAlways));
    let (_, r) = answer_permission(&[
        key(KeyCode::Down),
        key(KeyCode::Down),
        key(KeyCode::Up),
        key(KeyCode::Enter),
    ]);
    assert_eq!(r, Some(PermissionReply::AllowAlways));
    // Esc (or ctrl+c) denies and drops what was queued.
    let (t, r) = answer_permission(&[key(KeyCode::Esc)]);
    assert_eq!(r, Some(PermissionReply::Deny(None)));
    assert!(t.queue.is_empty());
    let (_, r) = answer_permission(&[ctrl('c')]);
    assert_eq!(r, Some(PermissionReply::Deny(None)));
    // Paging scrolls the detail; other keys do nothing.
    let (t, r) = answer_permission(&[
        key(KeyCode::PageDown),
        key(KeyCode::PageDown),
        key(KeyCode::PageUp),
        key(KeyCode::F(3)),
    ]);
    assert_eq!(r, None);
    assert!(matches!(
        t.dialog,
        Some(Dialog::Permission { scroll: 5, .. })
    ));
    // `e` asks to edit the proposed content.
    let (t, r) = answer_permission(&[key(KeyCode::Char('e'))]);
    assert!(t.edit_proposal && r.is_none());
}

#[tokio::test]
async fn saying_no_can_carry_feedback() {
    let mut keys = vec![key(KeyCode::Char('n'))];
    keys.extend("use mv".chars().map(|c| key(KeyCode::Char(c))));
    keys.extend([
        key(KeyCode::Left),
        key(KeyCode::Right),
        key(KeyCode::Char('x')),
        key(KeyCode::Backspace),
        key(KeyCode::Enter),
    ]);
    let (t, r) = answer_permission(&keys);
    assert_eq!(r, Some(PermissionReply::Deny(Some("use mv".into()))));
    assert!(t.dialog.is_none());
    // Empty feedback is a plain no.
    let (_, r) = answer_permission(&[key(KeyCode::Char('3')), key(KeyCode::Enter)]);
    assert_eq!(r, Some(PermissionReply::Deny(None)));
    // Esc leaves the feedback box, not the prompt.
    let (t, r) = answer_permission(&[key(KeyCode::Char('n')), key(KeyCode::Esc)]);
    assert!(r.is_none());
    assert!(matches!(
        t.dialog,
        Some(Dialog::Permission { feedback: None, .. })
    ));
    // A paste goes into the open feedback box.
    let mut t = test_app();
    let (tx, _rx) = oneshot::channel();
    t.on_msg(AppMsg::Permission(permission(None), tx));
    t.on_key(key(KeyCode::Char('n')));
    assert!(!t.on_paste("pasted reason"));
    let Some(Dialog::Permission {
        feedback: Some(fb), ..
    }) = &t.dialog
    else {
        panic!()
    };
    assert_eq!(fb.text, "pasted reason");
}

#[tokio::test]
async fn shift_tab_on_a_prompt_switches_mode_and_may_approve() {
    // Default → accept edits approves the prompt.
    let (t, r) = answer_permission(&[key(KeyCode::BackTab)]);
    assert_eq!(r, Some(PermissionReply::Allow));
    assert_eq!(t.mode, Mode::AcceptEdits);
    // Accept edits → plan is stricter: the prompt stays.
    let mut t = test_app();
    t.env.set_mode(Mode::AcceptEdits);
    let (tx, mut rx) = oneshot::channel();
    t.on_msg(AppMsg::Permission(permission(None), tx));
    t.on_key(key(KeyCode::BackTab));
    assert_eq!(t.mode, Mode::Plan);
    assert!(rx.try_recv().is_err() && t.dialog.is_some());
}

#[tokio::test]
async fn question_keys_pick_or_type_an_answer() {
    let mut t = test_app();
    let mut rx = question(&mut t, &["red", "green"], false);
    t.on_key(key(KeyCode::Down));
    t.on_key(key(KeyCode::Down));
    t.on_key(key(KeyCode::Up));
    t.on_key(key(KeyCode::PageDown));
    t.on_key(key(KeyCode::PageUp));
    t.on_key(key(KeyCode::Char('9')));
    t.on_key(key(KeyCode::Char('x')));
    assert!(t.dialog.is_some());
    t.on_key(key(KeyCode::Char('2')));
    assert_eq!(rx.try_recv().unwrap(), Some((1, "green".into())));
    assert!(t.dialog.is_none());
    // Esc answers nothing.
    let mut rx = question(&mut t, &["red"], false);
    t.on_key(key(KeyCode::Esc));
    assert_eq!(rx.try_recv().unwrap(), None);
    // Free text: a letter starts an answer.
    let mut rx = question(&mut t, &["red"], true);
    for c in "blue".chars() {
        t.on_key(key(KeyCode::Char(c)));
    }
    t.on_key(key(KeyCode::Left));
    t.on_key(key(KeyCode::Right));
    t.on_key(key(KeyCode::Backspace));
    t.on_key(key(KeyCode::Char('e')));
    t.on_key(key(KeyCode::F(1)));
    t.on_key(key(KeyCode::Enter));
    assert_eq!(rx.try_recv().unwrap(), Some((usize::MAX, "blue".into())));
    // The last option ("Type something else…") opens the box; esc closes it.
    let mut rx = question(&mut t, &["red"], true);
    t.on_key(key(KeyCode::Char('2')));
    assert!(matches!(
        t.dialog,
        Some(Dialog::Question {
            typing: Some(_),
            ..
        })
    ));
    t.on_key(key(KeyCode::Esc));
    assert!(matches!(
        t.dialog,
        Some(Dialog::Question { typing: None, .. })
    ));
    // An empty typed answer is no answer.
    t.on_key(key(KeyCode::Char('2')));
    t.on_key(key(KeyCode::Enter));
    assert_eq!(rx.try_recv().unwrap(), None);
    // No options and free text: the box is open from the start.
    let _rx = question(&mut t, &[], true);
    assert!(!t.on_paste("typed"));
    let Some(Dialog::Question {
        typing: Some(fb),
        options,
        ..
    }) = &t.dialog
    else {
        panic!()
    };
    assert_eq!(fb.text, "typed");
    assert_eq!(options, &["Type something else…"]);
}

#[tokio::test]
async fn the_quit_confirmation_quits_on_its_first_option() {
    let mut t = test_app();
    t.open_dialog(Dialog::Question {
        q: quit_warning(2),
        options: vec!["Quit anyway".into(), "Keep working".into()],
        sel: 0,
        tx: None,
        typing: None,
        scroll: 0,
        free_text: false,
        search: None,
        action: Some(QuestionAction::Quit),
    });
    t.on_key(key(KeyCode::Char('2')));
    assert!(!t.quit && t.dialog.is_none());
    t.open_dialog(Dialog::Question {
        q: quit_warning(2),
        options: vec!["Quit anyway".into(), "Keep working".into()],
        sel: 0,
        tx: None,
        typing: None,
        scroll: 0,
        free_text: false,
        search: None,
        action: Some(QuestionAction::Quit),
    });
    t.on_key(key(KeyCode::Enter));
    assert!(t.quit);
}

fn picker_dialog(
    t: &mut TestApp,
    labels: &[&str],
    sessions: bool,
) -> oneshot::Receiver<Option<(usize, String)>> {
    let labels: Vec<String> = labels.iter().map(|s| s.to_string()).collect();
    let search = PickerSearch::new(&labels, labels.clone(), sessions);
    let (tx, rx) = oneshot::channel();
    t.open_dialog(Dialog::Question {
        q: search.heading(),
        options: labels,
        sel: 0,
        tx: Some(tx),
        typing: None,
        scroll: 0,
        free_text: false,
        search: Some(search),
        action: None,
    });
    rx
}

#[tokio::test]
async fn picker_search_filters_as_you_type() {
    let mut t = test_app();
    let mut rx = picker_dialog(&mut t, &["gpt-5", "claude-opus", "qwen3"], false);
    type_str(&mut t, "qwn");
    let Some(Dialog::Question { options, q, .. }) = &t.dialog else {
        panic!()
    };
    assert_eq!(options, &["qwen3"]);
    assert!(q.contains("Search: qwn"));
    t.on_key(key(KeyCode::Enter));
    assert_eq!(rx.try_recv().unwrap(), Some((2, String::new())));
    // No match: Enter uses the query as a model name.
    let mut rx = picker_dialog(&mut t, &["gpt-5"], false);
    type_str(&mut t, "local/llama");
    t.on_key(key(KeyCode::Backspace));
    t.on_key(key(KeyCode::Enter));
    assert_eq!(
        rx.try_recv().unwrap(),
        Some((usize::MAX, "local/llam".into()))
    );
    // ctrl+s picks for this session only.
    let mut rx = picker_dialog(&mut t, &["gpt-5"], false);
    t.on_key(ctrl('s'));
    assert_eq!(rx.try_recv().unwrap(), Some((0, "session".into())));
}

#[tokio::test]
async fn resume_picker_row_actions() {
    let mut t = test_app();
    let mut rx = picker_dialog(&mut t, &["a", "b"], true);
    t.on_key(key(KeyCode::Down));
    t.on_key(ctrl('d'));
    assert_eq!(rx.try_recv().unwrap(), Some((1, "delete".into())));
    let mut rx = picker_dialog(&mut t, &["a", "b"], true);
    t.on_key(ctrl('a'));
    assert_eq!(rx.try_recv().unwrap(), Some((0, "archive".into())));
    let mut rx = picker_dialog(&mut t, &["a", "b"], true);
    t.on_key(key(KeyCode::Tab));
    assert_eq!(
        rx.try_recv().unwrap(),
        Some((usize::MAX, "toggle-archived".into()))
    );
    // Enter with nothing matching in a session list does nothing.
    let mut rx = picker_dialog(&mut t, &["a"], true);
    type_str(&mut t, "zz");
    t.on_key(key(KeyCode::Enter));
    assert!(rx.try_recv().is_err() && t.dialog.is_some());
}

#[tokio::test]
async fn btw_panel_keys() {
    let mut t = test_app();
    t.env.btw.lock().unwrap().extend([
        ("a?".to_string(), "A".to_string()),
        ("b?".to_string(), "B".to_string()),
    ]);
    t.show_btw(0);
    t.on_key(key(KeyCode::Right));
    t.on_key(key(KeyCode::Char(']')));
    assert!(matches!(t.dialog, Some(Dialog::Btw { index: 1, .. })));
    t.on_key(key(KeyCode::PageDown));
    t.on_key(key(KeyCode::Down));
    assert!(matches!(t.dialog, Some(Dialog::Btw { scroll: 6, .. })));
    t.on_key(key(KeyCode::Up));
    t.on_key(key(KeyCode::PageUp));
    t.on_key(key(KeyCode::Left));
    assert!(matches!(
        t.dialog,
        Some(Dialog::Btw {
            index: 0,
            scroll: 0
        })
    ));
    t.on_key(key(KeyCode::Char('z')));
    t.on_key(key(KeyCode::Char('x')));
    assert!(t.dialog.is_none() && t.env.btw.lock().unwrap().is_empty());
    assert_eq!(t.hint_text(), "Side questions cleared");
    t.env.btw.lock().unwrap().push(("a?".into(), "A".into()));
    t.show_btw(0);
    t.on_key(key(KeyCode::Char('q')));
    assert!(t.dialog.is_none());
    // `f` forks from an answered question.
    t.show_btw(0);
    t.on_key(key(KeyCode::Char('f')));
    assert!(t.dialog.is_none());
}

#[tokio::test]
async fn mcp_panel_keys_navigate_and_run_actions() {
    let mut t = test_app_with(
        |c| {
            for name in ["alpha", "beta"] {
                c.mcp_servers.insert(
                    name.into(),
                    crate::config::McpServerConfig {
                        command: format!("{name}-server"),
                        ..Default::default()
                    },
                );
            }
        },
        false,
    );
    t.open_mcp();
    t.on_key(key(KeyCode::Char('j')));
    t.on_key(key(KeyCode::Down));
    assert!(matches!(t.dialog, Some(Dialog::Mcp { sel: 1, .. })));
    t.on_key(key(KeyCode::Char('k')));
    t.on_key(key(KeyCode::PageDown));
    t.on_key(key(KeyCode::PageUp));
    assert!(matches!(
        t.dialog,
        Some(Dialog::Mcp {
            sel: 0,
            scroll: 0,
            ..
        })
    ));
    // Opening /mcp again keeps the panel and closes its menu.
    t.on_key(key(KeyCode::Enter));
    assert!(matches!(t.dialog, Some(Dialog::Mcp { menu: Some(0), .. })));
    t.open_mcp();
    assert!(matches!(t.dialog, Some(Dialog::Mcp { menu: None, .. })));
    t.on_key(key(KeyCode::Char('l')));
    t.on_key(key(KeyCode::Down));
    t.on_key(key(KeyCode::Down));
    t.on_key(key(KeyCode::Char('k')));
    t.on_key(key(KeyCode::Char('j')));
    t.on_key(key(KeyCode::F(2)));
    assert!(matches!(t.dialog, Some(Dialog::Mcp { menu: Some(1), .. })));
    t.on_key(key(KeyCode::Esc));
    assert!(matches!(t.dialog, Some(Dialog::Mcp { menu: None, .. })));
    // Running an action notes it and reports back with McpNote.
    t.on_key(key(KeyCode::Right));
    t.on_key(key(KeyCode::Down));
    t.on_key(key(KeyCode::Enter));
    assert!(matches!(
        &t.dialog,
        Some(Dialog::Mcp { note: Some(n), .. }) if n == "Disable for this project…"
    ));
    let note = loop {
        match t.rx.recv().await {
            Some(AppMsg::McpNote(n)) => break n,
            Some(_) => {}
            None => panic!("no note"),
        }
    };
    t.on_msg(AppMsg::McpNote(note.clone()));
    assert!(matches!(
        &t.dialog,
        Some(Dialog::Mcp { note: Some(n), menu: None, .. }) if *n == note
    ));
    t.on_key(key(KeyCode::Esc));
    assert!(t.dialog.is_none());
}

#[tokio::test]
async fn down_on_an_empty_prompt_selects_the_agents_panel() {
    let mut t = test_app();
    t.env
        .live_agents
        .start("k1", "agent-1", "explore", "look around", "find it", true);
    t.on_key(key(KeyCode::Down));
    assert_eq!(t.work_sel, Some(0));
    assert!(t.hint_text().contains("enter view"));
    t.on_key(key(KeyCode::Down));
    assert_eq!(t.work_sel, Some(1));
    t.on_key(ctrl('p'));
    t.on_key(ctrl('n'));
    t.on_key(key(KeyCode::Up));
    t.on_key(key(KeyCode::Down));
    t.on_key(key(KeyCode::Enter));
    assert_eq!(t.viewing.as_deref(), Some("k1"));
    assert!(t.hint_text().starts_with("Viewing explore (agent-1)"));
    assert!(t.work_sel.is_none());
    // Selecting main goes back.
    t.work_sel = Some(0);
    t.on_key(key(KeyCode::Enter));
    assert!(t.viewing.is_none());
    // Esc leaves the panel; `x` on main does nothing.
    t.work_sel = Some(0);
    t.on_key(key(KeyCode::Char('x')));
    assert_eq!(t.work_sel, Some(0));
    t.on_key(key(KeyCode::F(4)));
    t.on_key(key(KeyCode::Esc));
    assert!(t.work_sel.is_none());
    // `x` stops a running background agent; a finished one is dismissed.
    t.work_sel = Some(1);
    t.on_key(key(KeyCode::Char('x')));
    assert_eq!(t.hint_text(), "Stopped agent-1");
    t.env.live_agents.finish("k1", false);
    t.work_sel = Some(1);
    t.on_key(key(KeyCode::Char('x')));
    assert_eq!(t.hint_text(), "Dismissed agent-1");
    // A stale selection is dropped.
    t.work_sel = Some(9);
    t.on_key(key(KeyCode::Char('a')));
    assert!(t.work_sel.is_none());
}

#[tokio::test]
async fn a_foreground_agent_cannot_be_stopped_or_messaged() {
    let mut t = test_app();
    t.env
        .live_agents
        .start("k2", "agent-2", "general", "work", "do it", false);
    t.work_sel = Some(1);
    t.on_key(key(KeyCode::Char('x')));
    assert!(
        t.hint_text()
            .starts_with("A foreground agent stops with the turn")
    );
    t.view_agent(Some("k2".into()));
    assert_eq!(
        t.prompt_target(),
        Some(("general".into(), "agent-2".into(), false))
    );
    t.input.set("hello agent");
    t.submit();
    assert_eq!(t.input.text, "hello agent", "kept in the prompt");
    assert!(t.hint_text().contains("runs in the foreground"));
    assert!(!t.busy);
    // An unknown agent: the prompt goes to the main conversation.
    assert!(!t.send_to_agent("gone", "x"));
}
