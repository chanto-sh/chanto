//! A test `App` (no terminal, no model server) and tests of its actions:
//! submitting, the completion popup, the quit/loop/idle logic.

use super::*;
use crate::agent::tests::{make_env_with, tmpdir};
use ratatui::backend::TestBackend;

/// An App over a scratch project directory, with a provider nothing listens
/// on, and the receiving end of its message channel.
pub(super) struct TestApp {
    pub(super) app: App,
    pub(super) rx: mpsc::UnboundedReceiver<AppMsg>,
    pub(super) dir: std::path::PathBuf,
}

impl std::ops::Deref for TestApp {
    type Target = App;
    fn deref(&self) -> &App {
        &self.app
    }
}

impl std::ops::DerefMut for TestApp {
    fn deref_mut(&mut self) -> &mut App {
        &mut self.app
    }
}

impl Drop for TestApp {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

impl TestApp {
    /// Messages the app has sent itself so far.
    pub(super) fn sent(&mut self) -> Vec<AppMsg> {
        let mut v = vec![];
        while let Ok(m) = self.rx.try_recv() {
            v.push(m);
        }
        v
    }

    /// The commands queued with `AppMsg::Submit`.
    pub(super) fn submitted(&mut self) -> Vec<String> {
        self.sent()
            .into_iter()
            .filter_map(|m| match m {
                AppMsg::Submit(s) => Some(s),
                _ => None,
            })
            .collect()
    }

    /// Plain text of every transcript block's notes.
    pub(super) fn notes(&self) -> Vec<String> {
        self.blocks
            .iter()
            .filter_map(|b| match &b.kind {
                Kind::Note(s, _) => Some(s.clone()),
                _ => None,
            })
            .collect()
    }

    pub(super) fn hint_text(&self) -> String {
        self.hint.as_ref().map(|h| h.0.clone()).unwrap_or_default()
    }

    /// Draw one frame on a `w`×`h` test terminal and return its rows.
    pub(super) fn render(&mut self, w: u16, h: u16) -> Vec<String> {
        let mut term = Terminal::new(TestBackend::new(w, h)).unwrap();
        term.draw(|f| self.app.draw(f)).unwrap();
        let buf = term.backend().buffer();
        (0..h)
            .map(|y| (0..w).map(|x| buf[(x, y)].symbol()).collect::<String>())
            .collect()
    }

    /// The rendered screen as one string.
    pub(super) fn screen(&mut self, w: u16, h: u16) -> String {
        self.render(w, h).join("\n")
    }
}

pub(super) fn test_app() -> TestApp {
    test_app_with(|_| {}, false)
}

pub(super) fn test_app_with(f: impl FnOnce(&mut crate::config::Config), inline: bool) -> TestApp {
    let dir = tmpdir("tui");
    let env = make_env_with("http://127.0.0.1:9", &dir, Mode::Default, 32768, f);
    let agent = Arc::new(AsyncMutex::new(Agent::new(env.clone())));
    let (tx, rx) = mpsc::unbounded_channel();
    let completions = vec![
        ("/help".to_string(), "Show help".to_string()),
        ("/model".to_string(), "Switch model".to_string()),
        ("/mode".to_string(), "Change mode".to_string()),
        ("/clear".to_string(), "Start over".to_string()),
    ];
    let mut app = App::new(
        env,
        agent,
        tx,
        completions,
        keybindings::Keymap::default(),
        vec!["cargo build".into(), "fix the parser".into()],
        inline,
    );
    // Never write notifications or OSC status reports to the test's stdout.
    app.notify = notify::Channel::Off;
    app.term_status = None;
    TestApp { app, rx, dir }
}

pub(super) fn key(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::NONE)
}

pub(super) fn ctrl(c: char) -> KeyEvent {
    KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL)
}

pub(super) fn alt(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::ALT)
}

pub(super) fn shift(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::SHIFT)
}

pub(super) fn type_str(app: &mut App, s: &str) {
    for c in s.chars() {
        app.on_key(key(KeyCode::Char(c)));
    }
}

pub(super) fn permission(proposed: Option<&str>) -> PermissionRequest {
    PermissionRequest {
        tool: "Bash".into(),
        label: "Bash(rm -rf build)".into(),
        detail: "rm -rf build".into(),
        always_label: "Yes, and don't ask again for rm".into(),
        reason: None,
        proposed: proposed.map(String::from),
        auto_deny: None,
    }
}

// ------------------------------------------------------------------ tests

#[tokio::test]
async fn literal_turns_do_not_expand_mentions_or_dispatch_commands() {
    use crate::agent::tests::mock_server;
    use serde_json::json;

    let mut t = test_app();
    let outside = tmpdir("literal-mention-outside");
    let secret = outside.join("credentials.txt");
    std::fs::write(&secret, "literal-mention-secret").unwrap();
    let prompts = [
        format!(
            "summarize @{} [Image #1] @server:resource",
            secret.display()
        ),
        "/exit".to_string(),
        format!("!touch {}", t.dir.join("executed").display()),
        "#--agents injected note".to_string(),
    ];
    let (base, seen) =
        mock_server(prompts.iter().map(|_| json!({"content": "done"})).collect()).await;
    let env = make_env_with(&base, &t.dir, Mode::Default, 32768, |_| {});
    t.env = env.clone();
    t.agent = Arc::new(AsyncMutex::new(Agent::new(env)));
    t.images.push(secret.clone());
    for prompt in &prompts {
        t.start_hidden(prompt.clone(), true);
        tokio::time::timeout(std::time::Duration::from_secs(10), async {
            loop {
                match t.rx.recv().await.unwrap() {
                    AppMsg::TurnDone(err) => {
                        assert!(err.is_none(), "{err:?}");
                        break;
                    }
                    AppMsg::Exit => panic!("literal /exit was dispatched"),
                    _ => {}
                }
            }
        })
        .await
        .unwrap();
    }
    let requests = seen.lock().unwrap();
    assert_eq!(requests.len(), prompts.len());
    for (request, prompt) in requests.iter().zip(&prompts) {
        let messages = request["messages"].as_array().unwrap();
        assert!(messages.iter().any(|m| {
            m["role"] == "user"
                && m["content"]
                    .as_str()
                    .is_some_and(|text| text.contains(prompt.as_str()))
        }));
        assert!(!request.to_string().contains("literal-mention-secret"));
    }
    assert_eq!(t.images, vec![secret]);
    assert!(!t.dir.join("executed").exists());
    assert!(!t.dir.join("AGENTS.md").exists());
    std::fs::remove_dir_all(outside).unwrap();
}

#[tokio::test]
async fn a_users_own_loop_prompt_still_runs_as_typed() {
    // `/loop 5m !cmd` fires with `literal: false`: the user typed it.
    let mut t = test_app();
    let ran = t.dir.join("ran");
    // A redirect works in sh, cmd and PowerShell alike; `touch` doesn't.
    t.start_hidden(format!("!echo x > \"{}\"", ran.display()), false);
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            if let AppMsg::TurnDone(_) = t.rx.recv().await.unwrap() {
                break;
            }
        }
    })
    .await
    .unwrap();
    assert!(ran.exists());
}

#[tokio::test]
async fn a_new_app_starts_idle_on_an_empty_prompt() {
    let t = test_app();
    assert!(!t.busy && !t.quit && t.input.is_empty());
    assert_eq!(t.model, "mock");
    assert_eq!(t.mode, Mode::Default);
    assert!(t.mouse, "full screen captures the mouse");
    assert!(t.inline.is_none());
    assert_eq!(t.input.history(), ["cargo build", "fix the parser"]);
    let inline = test_app_with(|_| {}, true);
    assert!(!inline.mouse && inline.inline.is_some());
}

#[tokio::test]
async fn submitting_a_prompt_starts_a_turn_and_records_it() {
    let mut t = test_app();
    t.input.set("hello :tada:");
    t.submit();
    assert!(t.busy && t.cancel.is_some());
    assert!(
        matches!(&t.blocks.last().unwrap().kind, Kind::User(s) if s == "hello 🎉"),
        "shortcodes expand in what was typed"
    );
    assert!(t.input.is_empty());
    // Blank input does nothing.
    let mut t = test_app();
    t.input.set("   ");
    t.submit();
    assert!(!t.busy && t.blocks.is_empty());
}

const TOO_BIG: usize = 400_000;

#[tokio::test]
async fn a_paste_that_wont_fit_warns_when_pasted_and_stays_on_enter() {
    let mut t = test_app();
    t.on_paste(&"word ".repeat(TOO_BIG / 5));
    assert!(t.hint.as_ref().unwrap().0.contains("won't fit this model"));
    let tag = t.input.text.clone();
    t.input.insert(" summarise this");
    let typed = t.input.text.clone();
    assert!(typed.starts_with(&tag));
    t.busy = true;
    t.submit_as(Deliver::Queue);
    assert!(t.queue.is_empty(), "nothing is sent");
    assert_eq!(t.input.text, typed, "the text stays in the box");
    assert_eq!(t.pastes.len(), 1);
    // Editing it means the offer no longer applies.
    t.input.insert("!");
    t.submit_as(Deliver::Queue);
    assert!(t.queue.is_empty());
    assert!(t.input.text.ends_with('!'));
}

#[tokio::test]
async fn enter_again_saves_the_paste_and_keeps_what_was_typed() {
    let mut t = test_app();
    let paste = "word ".repeat(TOO_BIG / 5);
    t.on_paste(&paste);
    t.input.insert(" summarise this");
    t.busy = true;
    t.submit_as(Deliver::Queue);
    t.submit_as(Deliver::Queue);
    assert_eq!(t.queue.len(), 1);
    let sent = t.queue[0].0.clone();
    assert!(sent.ends_with(" summarise this"), "{sent}");
    let path = sent
        .strip_prefix("[Pasted text saved to ")
        .and_then(|s| s.split(';').next())
        .unwrap();
    assert_eq!(std::fs::read_to_string(path).unwrap(), paste);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }
    std::fs::remove_file(path).unwrap();
    assert!(t.pastes.is_empty());
    assert!(t.input.text.is_empty());
}

#[tokio::test]
async fn typed_text_too_large_is_saved_whole() {
    let mut t = test_app();
    let typed = "word ".repeat(TOO_BIG / 5);
    t.input.set(&typed);
    t.busy = true;
    t.submit_as(Deliver::Queue);
    assert!(t.queue.is_empty());
    t.submit_as(Deliver::Queue);
    let sent = t.queue[0].0.clone();
    let path = sent
        .strip_prefix("My message was too large to send, so it's saved to ")
        .and_then(|s| s.split(". Read").next())
        .unwrap();
    assert_eq!(
        std::fs::read_to_string(path).unwrap().trim_end(),
        typed.trim_end()
    );
    std::fs::remove_file(path).unwrap();
}

#[tokio::test]
async fn messages_that_fit_and_commands_are_not_held() {
    let mut t = test_app();
    t.busy = true;
    t.input.set("a short question");
    t.submit_as(Deliver::Queue);
    assert_eq!(t.queue[0].0, "a short question");
    t.input
        .set(&format!("/help {}", "word ".repeat(TOO_BIG / 5)));
    t.submit_as(Deliver::Queue);
    assert!(t.input.text.is_empty(), "a command isn't sent to the model");
}

#[tokio::test]
async fn submitting_expands_collapsed_pastes() {
    let mut t = test_app();
    t.pastes.push("line one\nline two".into());
    t.busy = true;
    t.input.set("look: [Pasted text #1 +2 lines] ok");
    t.submit_as(Deliver::Queue);
    assert_eq!(t.queue[0].0, "look: line one\nline two ok");
    assert!(t.pastes.is_empty());
    assert_eq!(
        t.deliver,
        Deliver::Step,
        "the delivery resets after a submit"
    );
}

#[tokio::test]
async fn a_message_sent_mid_turn_goes_to_the_inbox_and_can_be_pulled_back() {
    let mut t = test_app();
    t.busy = true;
    t.input.set("also check the tests");
    t.submit();
    assert_eq!(t.env.inbox.lock().unwrap().len(), 1);
    assert_eq!(t.steered.len(), 1);
    assert!(matches!(&t.blocks.last().unwrap().kind, Kind::User(s) if s == "also check the tests"));
    // Commands wait for the turn to end.
    t.input.set("/clear");
    t.submit();
    t.input.set("!ls");
    t.submit();
    assert_eq!(
        t.queue.iter().map(|q| q.0.as_str()).collect::<Vec<_>>(),
        ["/clear", "!ls"]
    );
    // Up on an empty prompt pulls everything not yet delivered back to edit.
    t.on_key(key(KeyCode::Up));
    assert_eq!(t.input.text, "also check the tests\n/clear\n!ls");
    assert!(t.queue.is_empty() && t.steered.is_empty());
    assert!(t.env.inbox.lock().unwrap().is_empty());
    assert!(
        !t.blocks.iter().any(|b| matches!(&b.kind, Kind::User(_))),
        "the pulled-back message leaves the transcript"
    );
    // Nothing waiting: Up is ordinary history.
    t.input.set("");
    assert!(!t.pull_back_waiting());
}

#[tokio::test]
async fn send_now_interrupts_the_turn_with_the_message() {
    let mut t = test_app();
    t.busy = true;
    let cancel = CancellationToken::new();
    t.cancel = Some(cancel.clone());
    t.queue.push_back(("later".into(), vec![]));
    t.input.set("right now");
    t.submit_as(Deliver::Now);
    assert!(cancel.is_cancelled() && t.send_now);
    assert_eq!(t.queue.front().unwrap().0, "right now");
    // An immediate settings command runs at once instead of queueing.
    t.input.set("/think off");
    t.submit();
    assert_eq!(t.queue.len(), 2, "not queued");
}

#[tokio::test]
async fn an_edited_earlier_prompt_branches_then_sends() {
    let mut t = test_app();
    t.editing_prompt = Some((2, "both"));
    t.input.set("try it another way");
    t.submit();
    assert_eq!(t.submitted(), ["/edit-prompt 2 both", "try it another way"]);
    assert!(t.editing_prompt.is_none() && !t.busy);
}

#[tokio::test]
async fn local_commands_change_view_state_without_a_turn() {
    let mut t = test_app();
    t.input.set("/vim");
    t.submit();
    assert!(t.vim && t.vim_insert);
    assert!(t.notes().last().unwrap().contains("Vim mode on"));
    t.input.set("/vim off");
    t.submit();
    assert!(!t.vim);
    t.input.set("/transcript");
    t.submit();
    assert!(t.expand_all);
    t.input.set("/mcp");
    t.submit();
    assert!(matches!(t.dialog, Some(Dialog::Mcp { .. })));
    t.dialog = None;
    // `/quiet` flips the view here and spells out the state for the agent.
    t.input.set("/quiet");
    t.submit();
    assert!(t.quiet);
    assert!(
        matches!(&t.blocks.last().unwrap().kind, Kind::User(s) if s == "/quiet on"),
        "{:?}",
        t.notes()
    );
    assert!(!t.busy || t.cancel.is_some());
}

#[tokio::test]
async fn loop_commands_report_their_state() {
    let mut t = test_app();
    t.input.set("/loop stop");
    t.submit();
    assert!(t.notes().iter().any(|n| n == "No loop is running."));
    t.input.set("/loop status");
    t.submit();
    assert!(
        t.notes().last().unwrap().starts_with("No loop is running."),
        "{:?}",
        t.notes()
    );
    t.input.set("/loop 9x tidy up");
    t.submit();
    assert!(t.notes().last().unwrap().contains("isn't an interval"));
    assert!(!t.busy, "a bad loop spec starts nothing");
    // Mid-turn, a new loop waits for the turn to end.
    t.busy = true;
    t.input.set("/loop 5m check the CI");
    t.submit();
    assert!(t.env.scheduler.loop_active());
    assert!(
        t.notes()
            .last()
            .unwrap()
            .ends_with("first run when the current turn ends"),
        "{:?}",
        t.notes()
    );
    t.input.set("/loop status");
    t.submit();
    assert!(
        t.notes()
            .last()
            .unwrap()
            .ends_with("`/loop stop` or esc ends it")
    );
    // esc on an empty prompt at rest ends the loop.
    t.busy = false;
    t.on_key(key(KeyCode::Esc));
    assert!(!t.env.scheduler.loop_active());
    assert_eq!(t.notes().last().unwrap(), "Loop stopped.");
}

#[tokio::test]
async fn ampersand_starts_a_background_run() {
    let mut t = test_app();
    t.input.set("&tidy the docs");
    t.submit();
    assert!(matches!(&t.blocks.last().unwrap().kind, Kind::User(s) if s == "/bg tidy the docs"));
    assert!(t.busy);
}

#[tokio::test]
async fn btw_opens_its_panel_with_the_question() {
    let mut t = test_app();
    t.input.set("/btw what is the build command?");
    t.submit();
    assert!(matches!(t.dialog, Some(Dialog::Btw { index: 0, .. })));
    assert_eq!(
        t.btw_ask.as_ref().unwrap().question,
        "what is the build command?"
    );
    // With answers already in, a bare `/btw` reopens on the newest.
    let mut t = test_app();
    t.env
        .btw
        .lock()
        .unwrap()
        .extend([("a?".into(), "A".into()), ("b?".into(), "B".into())]);
    t.input.set("/btw");
    t.submit();
    assert!(matches!(t.dialog, Some(Dialog::Btw { index: 1, .. })));
    // Showing again retargets the open panel rather than stacking one.
    t.show_btw(0);
    assert!(matches!(t.dialog, Some(Dialog::Btw { index: 0, .. })));
    assert!(t.pending_dialogs.is_empty());
}

#[tokio::test]
async fn settings_commands_open_pickers() {
    let mut t = test_app();
    t.input.set("/config");
    t.submit();
    let Some(AppMsg::Picker {
        title,
        labels,
        values,
    }) = t.sent().pop()
    else {
        panic!("no picker")
    };
    assert!(title.starts_with("Settings"));
    assert!(labels.iter().any(|l| l == "mode: default"), "{labels:?}");
    assert!(values.contains(&"/mode acceptEdits".to_string()));
    assert!(labels.iter().any(|l| l == "vim keys: off"));
    // Nothing to choose: the command runs as text instead.
    assert!(!t.open_settings_picker("/permissions"));
    assert!(!t.open_settings_picker("/hooks"));
    assert!(!t.open_settings_picker("/nope"));
}

#[tokio::test]
async fn permission_rules_picker_lists_each_rule_to_remove() {
    let mut t = test_app();
    *t.env.rules.write().unwrap() =
        crate::permissions::Rules::new(&["Bash(ls:*)".into()], &["Bash(rm:*)".into()], &[]);
    assert!(t.open_settings_picker("/permissions"));
    let Some(AppMsg::Picker { labels, values, .. }) = t.sent().pop() else {
        panic!("no picker")
    };
    assert_eq!(labels.len(), 2);
    assert!(
        values.contains(&"/unallow Bash(ls:*)".to_string()),
        "{values:?}"
    );
    assert!(
        values.contains(&"/undeny Bash(rm:*)".to_string()),
        "{values:?}"
    );
}

#[tokio::test]
async fn slash_popup_filters_completions() {
    let mut t = test_app();
    t.input.set("/mo");
    t.update_popup();
    let p = t.popup.as_ref().unwrap();
    assert!(p.kind == PopupKind::Slash);
    assert_eq!(
        p.items.iter().map(|i| i.0.as_str()).collect::<Vec<_>>(),
        ["/model", "/mode"]
    );
    // No prefix match: fall back to a substring match.
    t.input.set("/lea");
    t.update_popup();
    assert_eq!(t.popup.as_ref().unwrap().items[0].0, "/clear");
    t.input.set("/zzz");
    t.update_popup();
    assert!(t.popup.is_none());
    // An argument closes it.
    t.input.set("/model x");
    t.update_popup();
    assert!(t.popup.is_none());
}

#[tokio::test]
async fn enter_on_a_slash_popup_runs_the_selected_command() {
    let mut t = test_app();
    t.input.set("/cle");
    t.update_popup();
    t.busy = true;
    t.submit();
    assert_eq!(t.queue[0].0, "/clear", "the selection completes the word");
}

#[tokio::test]
async fn at_completion_lists_project_files_then_ignored_ones() {
    let mut t = test_app();
    std::fs::create_dir_all(t.dir.join("src")).unwrap();
    std::fs::write(t.dir.join("src/parser.rs"), "").unwrap();
    std::fs::write(t.dir.join("README.md"), "").unwrap();
    std::fs::write(t.dir.join(".ignore"), "secret.log\n").unwrap();
    std::fs::write(t.dir.join("secret.log"), "").unwrap();
    t.input.set("look at @pars");
    t.update_popup();
    let p = t.popup.as_ref().unwrap();
    assert!(p.kind == PopupKind::File);
    assert_eq!(p.items[0].0.replace('\\', "/"), "@src/parser.rs");
    t.input.set("see @secret");
    t.update_popup();
    let p = t.popup.as_ref().unwrap();
    assert_eq!(
        p.items[0],
        ("@secret.log".to_string(), "gitignored".to_string())
    );
    // Accepting replaces only the word being typed.
    t.accept_popup();
    assert_eq!(t.input.text, "see @secret.log ");
    t.input.set("@nothing-like-this");
    t.update_popup();
    assert!(t.popup.is_none());
}

#[tokio::test]
async fn a_recalled_command_does_not_open_the_popup() {
    let mut t = test_app();
    t.input = Input::with_history(vec!["/model".into()]);
    t.on_key(key(KeyCode::Up));
    assert_eq!(t.input.text, "/model");
    assert!(t.popup.is_none());
}

#[tokio::test]
async fn quitting_asks_first_only_when_background_work_runs() {
    let mut t = test_app();
    t.request_quit();
    assert!(t.quit);
}

#[tokio::test]
async fn open_dialog_queues_behind_the_open_one() {
    let mut t = test_app();
    t.open_dialog(Dialog::Btw {
        index: 0,
        scroll: 0,
    });
    t.open_mcp();
    assert!(matches!(t.dialog, Some(Dialog::Btw { .. })));
    assert_eq!(t.pending_dialogs.len(), 1);
}

#[tokio::test]
async fn attention_is_dropped_once_nothing_waits() {
    let mut t = test_app();
    t.want_attention("question".into());
    assert!(t.attention.is_none(), "no dialog: nothing to notify about");
    t.dialog = Some(Dialog::Btw {
        index: 0,
        scroll: 0,
    });
    t.want_attention("Has a question".into());
    assert!(t.attention.is_some(), "waits for the delay");
    t.focused = Some(true);
    t.check_attention();
    assert!(t.attention.is_some(), "never while focused");
    t.focused = Some(false);
    t.check_attention();
    assert!(
        t.attention.is_none(),
        "sent once the window is in the background"
    );
}

#[tokio::test]
async fn a_ready_recap_lands_in_the_transcript() {
    let mut t = test_app();
    let (tx, rx) = tokio::sync::oneshot::channel();
    t.recap = Some(rx);
    tx.send("you were fixing the parser".into()).unwrap();
    t.idle_tick();
    assert!(t.recap.is_none());
    assert_eq!(t.notes(), ["※ recap: you were fixing the parser"]);
    // A dropped recap is forgotten quietly.
    let (tx, rx) = tokio::sync::oneshot::channel::<String>();
    t.recap = Some(rx);
    drop(tx);
    t.idle_tick();
    assert!(t.recap.is_none());
    // Idle, but recently: nothing fires.
    t.idle_since = Some(Instant::now());
    t.idle_tick();
    assert!(!t.idle_notified);
}

#[tokio::test]
async fn monitor_notifications_are_shown_and_handed_to_the_model() {
    let mut t = test_app();
    t.env
        .notifications
        .lock()
        .unwrap()
        .push("<system-reminder>Monitor CI run 42: passed</system-reminder>".into());
    // Not between turns: waits.
    t.busy = true;
    t.loop_tick();
    assert_eq!(t.env.notifications.lock().unwrap().len(), 1);
    t.busy = false;
    t.loop_tick();
    assert!(t.env.notifications.lock().unwrap().is_empty());
    assert!(t.notes().iter().any(|n| n == "◉ Monitor CI run 42: passed"));
    assert!(t.busy, "a turn started to deliver it");
    assert!(
        !t.blocks.iter().any(|b| matches!(b.kind, Kind::User(_))),
        "the hidden prompt has no user block"
    );
}

#[tokio::test]
async fn key_effects_run_their_actions() {
    let mut t = test_app();
    let think = t.env.think();
    t.run_key_effect(keybindings::Effect::ToggleThinking);
    assert_eq!(t.env.think(), !think);
    t.run_key_effect(keybindings::Effect::KillAgents);
    assert_eq!(t.hint_text(), "Stopped 0 background agent(s)");
    t.run_key_effect(keybindings::Effect::Exit);
    assert!(matches!(t.sent().pop(), Some(AppMsg::Exit)));
    t.run_key_effect(keybindings::Effect::Keys(vec![ctrl('t')]));
    assert!(!t.show_todos && !t.replaying);
    t.run_key_effect(keybindings::Effect::Nothing);
}

#[tokio::test]
async fn keybindings_file_remaps_a_key() {
    let mut t = test_app();
    let file = t.dir.join("keybindings.json");
    std::fs::write(
        &file,
        r#"{"bindings": [{"context": "Chat", "bindings": {"ctrl+e": "chat:externalEditor", "ctrl+y": null}}]}"#,
    )
    .unwrap();
    let (keymap, problems) = keybindings::Keymap::load(vec![file]);
    assert!(problems.is_empty(), "{problems:?}");
    t.keymap = keymap;
    t.on_key(ctrl('e'));
    assert!(t.open_editor, "ctrl+e now opens the editor");
    t.input.set("abc");
    t.on_key(ctrl('y'));
    assert_eq!(t.input.text, "abc", "an unbound key is swallowed");
}

#[tokio::test]
async fn cycle_mode_steps_through_the_modes() {
    let mut t = test_app();
    t.cycle_mode();
    assert_eq!(t.mode, Mode::AcceptEdits);
    assert_eq!(t.env.mode(), Mode::AcceptEdits);
}

#[tokio::test]
async fn the_frontend_round_trips_prompts_through_the_app() {
    let mut t = test_app();
    let fe = Arc::new(TuiFrontend { tx: t.tx.clone() });
    fe.emit(Event::Info("hi".into()));
    assert!(matches!(t.rx.recv().await, Some(AppMsg::Agent(Event::Info(s))) if s == "hi"));
    let f = fe.clone();
    let ask = tokio::spawn(async move { f.permission(permission(None)).await });
    let Some(AppMsg::Permission(req, tx)) = t.rx.recv().await else {
        panic!("no permission")
    };
    assert_eq!(req.tool, "Bash");
    tx.send(PermissionReply::AllowAlways).unwrap();
    assert_eq!(ask.await.unwrap(), PermissionReply::AllowAlways);
    let f = fe.clone();
    let q = tokio::spawn(async move { f.question("Which?", &["a".into(), "b".into()]).await });
    let Some(AppMsg::Question(_, opts, free, tx)) = t.rx.recv().await else {
        panic!("no question")
    };
    assert!(free && opts.len() == 2);
    tx.send(Some((1, "b".into()))).unwrap();
    assert_eq!(q.await.unwrap().as_deref(), Some("b"));
    let f = fe.clone();
    let c = tokio::spawn(async move { f.choose("Pick", &["x".into()]).await });
    let Some(AppMsg::Question(_, _, free, tx)) = t.rx.recv().await else {
        panic!("no choice")
    };
    assert!(!free);
    drop(tx);
    assert_eq!(c.await.unwrap(), None, "a dropped answer is no answer");
    // Nobody listening: a permission is denied.
    let (tx, rx) = mpsc::unbounded_channel();
    drop(rx);
    let fe = TuiFrontend { tx };
    assert_eq!(
        fe.permission(permission(None)).await,
        PermissionReply::Deny(None)
    );
}

#[test]
fn small_labels_and_titles() {
    assert_eq!(title_icon(true, true), "❖");
    assert_eq!(title_icon(false, true), "✓");
    assert_eq!(title_icon(false, false), "❯");
    assert_eq!(window_title("❯", Some("app")), "❯ app — chanto");
    assert_eq!(window_title("❯", Some("chanto")), "❯ chanto");
    assert_eq!(window_title("❯", Some("  ")), "❯ chanto");
    assert_eq!(busy_frame(true, 999), "❖");
    assert_eq!(busy_frame(false, 0), "◰");
    assert_eq!(busy_frame(false, 80), "◳");
    assert_eq!(thinking_label(None), "❖ thinking…");
    assert_eq!(
        thinking_label(Some(Duration::from_millis(200))),
        "❖ thinking for 1s"
    );
    assert_eq!(thinking_verb(5), "thinking");
    assert_eq!(thinking_verb(12), "still thinking");
    assert_eq!(thinking_verb(25), "thinking more");
    assert_eq!(thinking_verb(35), "thinking some more");
    assert_eq!(thinking_verb(60), "deep in thought");
    assert_eq!(
        effort_label(false, Some("high".into())).as_deref(),
        Some("thinking off")
    );
    assert_eq!(effort_label(true, Some(String::new())), None);
    assert_eq!(
        effort_label(true, Some("low".into())).as_deref(),
        Some("low")
    );
    assert_eq!(normalize_paste("a\r\nb\rc"), "a\nb\nc");
    assert_eq!(mode_arg(Mode::Bypass), "bypass");
    assert_eq!(mode_arg(Mode::Plan), "plan");
    assert_eq!(mode_arg(Mode::Auto), "auto");
    assert_eq!(mode_arg(Mode::Default), "default");
}

#[test]
fn selections_order_their_ends() {
    let s = Selection {
        anchor: (5, 2),
        head: (1, 9),
    };
    assert_eq!(s.ordered(), ((1, 9), (5, 2)));
    let s = Selection {
        anchor: (1, 1),
        head: (1, 4),
    };
    assert_eq!(s.ordered(), ((1, 1), (1, 4)));
}

#[test]
fn prompt_rows_color_commands_and_mentions() {
    let spans = input_row_spans("/model fast @src/a.rs  done", true);
    let text: Vec<&str> = spans.iter().map(|s| s.content.as_ref()).collect();
    assert_eq!(text, ["/model", " fast ", "@src/a.rs", "  done"]);
    assert_eq!(spans[0].style.fg, Some(pal().accent));
    assert_eq!(spans[2].style.fg, Some(pal().link));
    // A path isn't a command, and later rows never are.
    let spans = input_row_spans("/usr/bin/env", true);
    assert_eq!(spans.len(), 1);
    assert_eq!(spans[0].style, Style::default());
    assert_eq!(input_row_spans("/help", false)[0].style, Style::default());
    assert!(input_row_spans("", true).is_empty());
    assert!(is_command_token("/a-b_c"));
}

#[test]
fn option_rows_window_around_the_selection() {
    let options: Vec<String> = (1..=20).map(|i| format!("option {i}")).collect();
    let rows = option_rows(&options, 15, 40);
    let text: Vec<String> = rows
        .iter()
        .map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect())
        .collect();
    assert_eq!(text.len(), 9);
    assert!(text[0].contains("12. option 12"), "{text:?}");
    assert!(text[4].starts_with(" ❯ 16."), "{text:?}");
    // A long option wraps under its text rather than being cut.
    let rows = option_rows(&["word ".repeat(20)], 0, 30);
    assert!(rows.len() > 1);
}

#[test]
fn mcp_actions_depend_on_the_state() {
    use crate::commands::{McpRow, McpState};
    let row = |state, http, can_enable| McpRow {
        name: "docs".into(),
        state,
        detail: String::new(),
        http,
        can_enable,
    };
    let labels = |r: &McpRow| mcp_actions(r).into_iter().map(|a| a.0).collect::<Vec<_>>();
    assert_eq!(
        labels(&row(McpState::Connected, true, false)),
        [
            "Reconnect",
            "Disable for this project",
            "Clear authentication"
        ]
    );
    assert_eq!(labels(&row(McpState::Connected, false, false)).len(), 2);
    assert_eq!(
        labels(&row(McpState::NeedsLogin, false, false))[0],
        "Authenticate"
    );
    assert_eq!(labels(&row(McpState::Failed, false, false))[0], "Reconnect");
    assert_eq!(
        mcp_actions(&row(McpState::Off, false, true)),
        [("Enable for this project", "/mcp enable docs".to_string())]
    );
    assert!(mcp_actions(&row(McpState::Off, false, false)).is_empty());
    assert_eq!(labels(&row(McpState::Pending, false, false))[1], "Reject");
    assert!(mcp_actions(&row(McpState::Rejected, false, false)).is_empty());
    assert_eq!(plain("`disabled = true`"), "disabled = true");
}

#[test]
fn panel_rows_are_clamped_with_an_ellipsis() {
    let line = Line::from(vec![Span::raw("abc"), Span::raw("defgh")]);
    let cut = clamp_row(line.clone(), 5);
    let text: String = cut.spans.iter().map(|s| s.content.as_ref()).collect();
    assert_eq!(text, "abcd…");
    assert_eq!(clamp_row(line, 20).width(), 8);
}

#[tokio::test]
async fn ctrl_o_expands_a_paste_tag_under_the_cursor() {
    let mut t = test_app();
    t.pastes.push("the\nwhole\npaste".into());
    t.input.set("see [Pasted text #1 +3 lines]");
    t.on_key(ctrl('o'));
    assert_eq!(t.input.text, "see the\nwhole\npaste");
    assert!(!t.expand_all, "not the transcript toggle");
    // No tag: ctrl+o toggles the transcript.
    t.on_key(ctrl('o'));
    assert!(t.expand_all);
    // A tag without its paste is left alone.
    t.input.set("[Pasted text #4 +3 lines]");
    assert!(!t.expand_paste_at_cursor());
}

#[tokio::test]
async fn interrupt_cancels_and_drops_the_queue() {
    let mut t = test_app();
    let c = CancellationToken::new();
    t.cancel = Some(c.clone());
    t.queue.push_back(("x".into(), vec![]));
    t.interrupt();
    assert!(c.is_cancelled() && t.queue.is_empty());
    // Send-now with nothing queued does nothing.
    t.send_queued_now();
    assert!(!t.send_now);
}

#[tokio::test]
async fn btw_overlay_lists_context_and_keys() {
    let btw = vec![
        ("first?".to_string(), "one".to_string()),
        ("second?".to_string(), "**two**".to_string()),
    ];
    let text = |lines: Vec<Line<'static>>| {
        lines
            .iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    };
    let out = text(btw_overlay_lines(&btw, None, 1, 60));
    assert!(out.contains(" 1 ↳ first?"), "{out}");
    assert!(out.contains("│ second?") && out.contains("two") && !out.contains("**"));
    assert!(out.contains("2 of 2 · ←/→ switch"), "{out}");
    let out = text(btw_overlay_lines(&btw[..1], None, 0, 60));
    assert!(
        out.ends_with("f fork with tools · x clear · esc close"),
        "{out}"
    );
    assert!(text(btw_overlay_lines(&[], None, 0, 60)).contains("No side questions yet"));
    let mut ask = BtwAsk {
        question: "third?".into(),
        error: None,
        started: Instant::now(),
    };
    let out = text(btw_overlay_lines(&btw, Some(&ask), 2, 60));
    assert!(out.contains("Asking the side question") && out.ends_with("esc close"));
    ask.error = Some("offline".into());
    assert!(
        text(btw_overlay_lines(&btw, Some(&ask), 2, 60)).contains("Side question failed: offline")
    );
}

#[tokio::test]
async fn mcp_overlay_shows_each_server_and_its_menu() {
    let text = |lines: Vec<Line<'static>>| {
        lines
            .iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    };
    let t = test_app();
    assert!(
        text(mcp_overlay_lines(&t.env, 0, None, None, 80)).contains("No MCP servers configured")
    );
    let t = test_app_with(
        |c| {
            c.mcp_servers.insert(
                "docs".into(),
                crate::config::McpServerConfig {
                    command: "docs-server".into(),
                    ..Default::default()
                },
            );
            c.mcp_servers.insert(
                "old".into(),
                crate::config::McpServerConfig {
                    command: "old-server".into(),
                    disabled: true,
                    ..Default::default()
                },
            );
        },
        false,
    );
    let out = text(mcp_overlay_lines(&t.env, 0, None, None, 80));
    assert!(
        out.contains(" ▸ ") && out.contains("docs") && out.contains("old"),
        "{out}"
    );
    assert!(
        out.contains("can: reconnect · disable for this project"),
        "{out}"
    );
    let out = text(mcp_overlay_lines(&t.env, 0, Some(1), Some("Done."), 80));
    assert!(
        out.contains(" ▸ Disable for this project") && out.contains("Done."),
        "{out}"
    );
    assert!(out.ends_with("↑/↓ choose · enter run · esc back"));
    let out = text(mcp_overlay_lines(&t.env, 1, Some(0), None, 80));
    assert!(out.contains("Nothing to do here: disabled = true"), "{out}");
}
