//! Custom keybindings, in Claude Code's `keybindings.json` format: blocks of
//! `{context, bindings: {keystroke: action | null}}`. Chanto reads its own
//! `~/.chanto/keybindings.json` and, with Claude Code compatibility on,
//! `~/.claude/keybindings.json` (Chanto's wins). Edits apply without a restart.
//!
//! A binding names an action; Chanto runs it by replaying the key it already
//! uses for that action, so every remap goes through the same code as the
//! default key. `null` unbinds a key. Actions Chanto has no equivalent for
//! are accepted and do nothing, as Claude Code does for retired ones.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use serde_json::Value;
use std::path::PathBuf;
use std::time::{Duration, Instant, SystemTime};

/// A keystroke, normalised: letters lowercase, Shift as a modifier.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Key {
    code: KeyCode,
    mods: KeyModifiers,
}

impl Key {
    fn new(code: KeyCode, mods: KeyModifiers) -> Key {
        let mods = mods
            & (KeyModifiers::CONTROL
                | KeyModifiers::SHIFT
                | KeyModifiers::ALT
                | KeyModifiers::SUPER);
        match code {
            KeyCode::Char(c) if c.is_ascii_uppercase() => Key {
                code: KeyCode::Char(c.to_ascii_lowercase()),
                mods: mods | KeyModifiers::SHIFT,
            },
            KeyCode::BackTab => Key {
                code: KeyCode::Tab,
                mods: mods | KeyModifiers::SHIFT,
            },
            _ => Key { code, mods },
        }
    }

    pub fn of(e: &KeyEvent) -> Key {
        Key::new(e.code, e.modifiers)
    }

    /// The event the TUI's own handlers expect for this key.
    fn event(self) -> KeyEvent {
        let code = match self.code {
            KeyCode::Tab if self.mods.contains(KeyModifiers::SHIFT) => KeyCode::BackTab,
            c => c,
        };
        KeyEvent::new(code, self.mods)
    }

    /// Typing it in a text field shouldn't be stolen by an unbind.
    fn types_text(self) -> bool {
        matches!(self.code, KeyCode::Char(_))
            && !self
                .mods
                .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SUPER)
    }
}

/// `ctrl+k`, `shift+tab`, `meta+p`, `escape`… (case-insensitive).
pub fn parse_key(s: &str) -> Option<Key> {
    let mut mods = KeyModifiers::NONE;
    let parts: Vec<String> = s
        .trim()
        .split('+')
        .map(|p| p.trim().to_ascii_lowercase())
        .collect();
    let (name, modifiers) = parts.split_last()?;
    for m in modifiers {
        mods |= match m.as_str() {
            "ctrl" | "control" => KeyModifiers::CONTROL,
            "shift" => KeyModifiers::SHIFT,
            "alt" | "opt" | "option" | "meta" => KeyModifiers::ALT,
            "cmd" | "command" | "super" | "win" => KeyModifiers::SUPER,
            _ => return None,
        };
    }
    let code = match name.as_str() {
        "escape" | "esc" => KeyCode::Esc,
        "enter" | "return" => KeyCode::Enter,
        "tab" => KeyCode::Tab,
        "space" => KeyCode::Char(' '),
        "up" => KeyCode::Up,
        "down" => KeyCode::Down,
        "left" => KeyCode::Left,
        "right" => KeyCode::Right,
        "pageup" => KeyCode::PageUp,
        "pagedown" => KeyCode::PageDown,
        "home" => KeyCode::Home,
        "end" => KeyCode::End,
        "backspace" => KeyCode::Backspace,
        "delete" => KeyCode::Delete,
        n if n.chars().count() == 1 => KeyCode::Char(n.chars().next()?),
        n if n.starts_with('f') && n[1..].parse::<u8>().is_ok() => KeyCode::F(n[1..].parse().ok()?),
        _ => return None,
    };
    Some(Key::new(code, mods))
}

/// Where a binding applies, as in Claude Code.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Context {
    Global,
    Chat,
    Autocomplete,
    Confirmation,
    HistorySearch,
    Select,
}

fn context(name: &str) -> Option<Context> {
    Some(match name {
        "Global" => Context::Global,
        "Chat" => Context::Chat,
        "Autocomplete" => Context::Autocomplete,
        "Confirmation" => Context::Confirmation,
        "HistorySearch" => Context::HistorySearch,
        "Select" | "MessageSelector" | "ThemePicker" | "ModelPicker" | "Settings" => {
            Context::Select
        }
        _ => return None,
    })
}

/// What an action does in Chanto: replay keys it already handles, or one of
/// the few actions with no key of their own.
#[derive(Clone, Debug, PartialEq)]
pub enum Effect {
    Keys(Vec<KeyEvent>),
    ModelPicker,
    KillAgents,
    ToggleThinking,
    CopySelection,
    Exit,
    /// Known Claude Code action with no Chanto equivalent.
    Nothing,
}

fn keys(spec: &[&str]) -> Effect {
    Effect::Keys(
        spec.iter()
            .filter_map(|s| parse_key(s))
            .map(Key::event)
            .collect(),
    )
}

pub fn effect(action: &str) -> Option<Effect> {
    Some(match action {
        "app:interrupt" => keys(&["esc"]),
        "app:exit" => Effect::Exit,
        "app:toggleTodos" => keys(&["ctrl+t"]),
        "app:toggleTranscript" | "transcript:toggleShowAll" => keys(&["ctrl+o"]),
        "chat:cancel"
        | "autocomplete:dismiss"
        | "confirm:no"
        | "select:cancel"
        | "historySearch:cancel"
        | "help:dismiss"
        | "transcript:exit" => keys(&["esc"]),
        "chat:cycleMode" | "confirm:cycleMode" => keys(&["shift+tab"]),
        "chat:modelPicker" => Effect::ModelPicker,
        "chat:thinkingToggle" => Effect::ToggleThinking,
        "chat:submit" | "confirm:yes" | "select:accept" | "historySearch:execute" => {
            keys(&["enter"])
        }
        "chat:queueSubmit" => keys(&["ctrl+x", "enter"]),
        "chat:sendNow" => keys(&["ctrl+x", "ctrl+s"]),
        "chat:newline" => keys(&["ctrl+j"]),
        "chat:externalEditor" => keys(&["ctrl+g"]),
        "chat:imagePaste" => keys(&["ctrl+v"]),
        "history:search" | "historySearch:next" => keys(&["ctrl+r"]),
        "history:previous" | "autocomplete:previous" | "confirm:previous" | "select:previous" => {
            keys(&["up"])
        }
        "history:next" | "autocomplete:next" | "confirm:next" | "select:next" => keys(&["down"]),
        "autocomplete:accept" | "historySearch:accept" => keys(&["tab"]),
        "select:pageUp" | "scroll:pageUp" | "scroll:halfPageUp" | "scroll:fullPageUp" => {
            keys(&["pageup"])
        }
        "select:pageDown" | "scroll:pageDown" | "scroll:halfPageDown" | "scroll:fullPageDown" => {
            keys(&["pagedown"])
        }
        "voice:pushToTalk" => keys(&["ctrl+space"]),
        "chat:killAgents" => Effect::KillAgents,
        "selection:copy" => Effect::CopySelection,
        a if ELSEWHERE.contains(&a) => Effect::Nothing,
        _ => return None,
    })
}

/// Claude Code actions Chanto has no screen or feature for: accepted (a shared
/// keybindings.json stays valid) and do nothing. Anything not listed here or
/// above is reported as unknown, so typos show up.
const ELSEWHERE: &[&str] = &[
    "app:redraw",
    "chat:clearInput",
    "chat:clearScreen",
    "chat:fastMode",
    "chat:undo",
    "chat:stash",
    "confirm:nextField",
    "confirm:previousField",
    "confirm:toggle",
    "permission:toggleDebug",
    "historySearch:cycleScope",
    "task:background",
    "theme:toggleSyntaxHighlighting",
    "tabs:next",
    "tabs:previous",
    "attachments:next",
    "attachments:previous",
    "attachments:remove",
    "attachments:exit",
    "footer:next",
    "footer:previous",
    "footer:up",
    "footer:down",
    "footer:openSelected",
    "footer:clearSelection",
    "footer:close",
    "footer:dismiss",
    "messageSelector:up",
    "messageSelector:down",
    "messageSelector:top",
    "messageSelector:bottom",
    "messageSelector:select",
    "diff:dismiss",
    "diff:previousSource",
    "diff:nextSource",
    "diff:previousFile",
    "diff:nextFile",
    "diff:back",
    "diff:viewDetails",
    "app:toggleReplTab",
    "app:cycleDiffBase",
    "app:diffFileListUp",
    "app:diffFileListDown",
    "app:toggleDiffNoiseFilter",
    "app:toggleDiffPreSession",
    "modelPicker:decreaseEffort",
    "modelPicker:increaseEffort",
    "modelPicker:thisSessionOnly",
    "effortSlider:decreaseEffort",
    "effortSlider:increaseEffort",
    "effortSlider:toggleUltracode",
    "effortSlider:thisSessionOnly",
    "select:first",
    "select:last",
    "plugin:toggle",
    "plugin:install",
    "plugin:favorite",
    "abovePrompt:toggle",
    "abovePrompt:focus",
    "abovePrompt:next",
    "abovePrompt:previous",
    "abovePrompt:press",
    "abovePrompt:leave",
    "abovePrompt:highlightNext",
    "abovePrompt:highlightPrevious",
    "pane:scrollUp",
    "pane:scrollDown",
    "pane:pageUp",
    "pane:pageDown",
    "pane:top",
    "pane:bottom",
    "pane:grow",
    "pane:shrink",
    "pane:close",
    "pane:next",
    "pane:previous",
    "settings:search",
    "settings:retry",
    "agents:switchView",
    "agents:togglePin",
    "agents:find",
    "agents:rename",
    "agents:previousGroup",
    "agents:nextGroup",
    "scroll:lineUp",
    "scroll:lineDown",
    "scroll:top",
    "scroll:bottom",
    "selection:clear",
    "selection:extendLeft",
    "selection:extendRight",
    "selection:extendUp",
    "selection:extendDown",
    "selection:extendLineStart",
    "selection:extendLineEnd",
];

struct Binding {
    context: Context,
    keys: Vec<Key>,
    /// None: unbound.
    action: Option<String>,
}

/// What a keystroke means given the bindings.
#[derive(Debug, PartialEq)]
pub enum Lookup {
    Run(Effect),
    /// The start of a chord: wait for the next key.
    Pending,
    /// Unbound: do nothing.
    Swallow,
    /// No binding: the TUI's own handling applies.
    Default,
}

#[derive(Default)]
pub struct Keymap {
    bindings: Vec<Binding>,
    files: Vec<(PathBuf, Option<SystemTime>)>,
    checked: Option<Instant>,
    pending: Vec<Key>,
    pending_since: Option<Instant>,
}

/// Where keybindings are read from, lowest precedence first.
pub fn files(claude_compat: bool) -> Vec<PathBuf> {
    let mut out = Vec::new();
    // `--safe-mode`: the default keys only.
    if crate::launch::get().safe_mode {
        return out;
    }
    if claude_compat {
        out.push(crate::compat::claude_dir().join("keybindings.json"));
    }
    out.push(crate::config::home_dir().join("keybindings.json"));
    out
}

fn modified(p: &PathBuf) -> Option<SystemTime> {
    std::fs::metadata(p).and_then(|m| m.modified()).ok()
}

impl Keymap {
    /// Load the files; returns the keymap and problems worth telling the user.
    pub fn load(files: Vec<PathBuf>) -> (Keymap, Vec<String>) {
        let mut km = Keymap::default();
        let mut problems = Vec::new();
        for f in &files {
            let Ok(text) = std::fs::read_to_string(f) else {
                continue;
            };
            match serde_json::from_str::<Value>(&text) {
                Ok(v) => km
                    .bindings
                    .extend(parse(&v, &f.display().to_string(), &mut problems)),
                Err(e) => problems.push(format!("{}: not valid JSON ({e})", f.display())),
            }
        }
        km.files = files
            .into_iter()
            .map(|f| {
                let m = modified(&f);
                (f, m)
            })
            .collect();
        (km, problems)
    }

    /// Reload if a file changed (checked at most once a second). Returns the
    /// problems from the new files, or None when nothing changed.
    pub fn refresh(&mut self) -> Option<Vec<String>> {
        if self
            .checked
            .is_some_and(|t| t.elapsed() < Duration::from_secs(1))
        {
            return None;
        }
        self.checked = Some(Instant::now());
        if self.files.iter().all(|(f, m)| modified(f) == *m) {
            return None;
        }
        let (fresh, problems) = Keymap::load(self.files.iter().map(|(f, _)| f.clone()).collect());
        *self = Keymap {
            checked: self.checked,
            ..fresh
        };
        Some(problems)
    }

    pub fn is_empty(&self) -> bool {
        self.bindings.is_empty()
    }

    /// Look up a keystroke in the active contexts (most specific first).
    /// Later files and later blocks win.
    pub fn lookup(&mut self, active: &[Context], key: Key) -> Lookup {
        if self
            .pending_since
            .is_some_and(|t| t.elapsed() > Duration::from_secs(3))
        {
            self.pending.clear();
        }
        let mut seq = self.pending.clone();
        seq.push(key);
        let live = |b: &&Binding| active.contains(&b.context);
        if let Some(b) = self
            .bindings
            .iter()
            .rev()
            .filter(live)
            .find(|b| b.keys == seq)
        {
            self.pending.clear();
            self.pending_since = None;
            return match &b.action {
                None if key.types_text() && seq.len() == 1 => Lookup::Default,
                None => Lookup::Swallow,
                Some(a) => Lookup::Run(effect(a).unwrap_or(Effect::Nothing)),
            };
        }
        if self
            .bindings
            .iter()
            .filter(live)
            .any(|b| b.action.is_some() && b.keys.len() > seq.len() && b.keys.starts_with(&seq))
        {
            self.pending = seq;
            self.pending_since = Some(Instant::now());
            return Lookup::Pending;
        }
        let was_pending = !self.pending.is_empty();
        self.pending.clear();
        self.pending_since = None;
        // A chord that didn't complete: the key that broke it acts as usual.
        let _ = was_pending;
        Lookup::Default
    }
}

fn parse(v: &Value, source: &str, problems: &mut Vec<String>) -> Vec<Binding> {
    let mut out = Vec::new();
    let Some(blocks) = v["bindings"].as_array() else {
        problems.push(format!("{source}: expected {{\"bindings\": [ ... ]}}"));
        return out;
    };
    for block in blocks {
        let name = block["context"].as_str().unwrap_or("");
        let Some(ctx) = context(name) else {
            // Contexts Chanto has no screen for (DiffPanel, Pane, Agents…) are fine to have.
            continue;
        };
        for (keystroke, action) in block["bindings"].as_object().into_iter().flatten() {
            let Some(keys) = keystroke
                .split_whitespace()
                .map(parse_key)
                .collect::<Option<Vec<Key>>>()
                .filter(|k| !k.is_empty())
            else {
                problems.push(format!("{source}: can't read the keystroke `{keystroke}`"));
                continue;
            };
            let reserved = keys.len() == 1
                && keys[0].mods == KeyModifiers::CONTROL
                && matches!(
                    keys[0].code,
                    KeyCode::Char('c' | 'd' | 'm' | 'i' | 'h' | '[')
                );
            if reserved {
                problems.push(format!(
                    "{source}: `{keystroke}` is reserved and can't be rebound"
                ));
                continue;
            }
            let action = match action {
                Value::Null => None,
                Value::String(a) if effect(a).is_some() => Some(a.clone()),
                Value::String(a) => {
                    problems.push(format!("{source}: unknown action `{a}` for `{keystroke}`"));
                    continue;
                }
                _ => {
                    problems.push(format!(
                        "{source}: `{keystroke}` must map to an action name or null"
                    ));
                    continue;
                }
            };
            out.push(Binding {
                context: ctx,
                keys,
                action,
            });
        }
    }
    out
}

/// The file `/keybindings` creates.
pub const TEMPLATE: &str = r#"{
  "$schema": "https://www.schemastore.org/claude-code-keybindings.json",
  "$docs": "https://code.claude.com/docs/en/keybindings",
  "bindings": [
    {
      "context": "Chat",
      "bindings": {}
    }
  ]
}
"#;

#[cfg(test)]
mod tests {
    use super::*;

    fn map(json: &str) -> (Keymap, Vec<String>) {
        let v: Value = serde_json::from_str(json).unwrap();
        let mut problems = Vec::new();
        let km = Keymap {
            bindings: parse(&v, "test", &mut problems),
            ..Default::default()
        };
        (km, problems)
    }

    fn k(s: &str) -> Key {
        parse_key(s).unwrap()
    }

    #[test]
    fn parses_claude_codes_key_syntax() {
        assert_eq!(
            k("ctrl+K"),
            Key::new(KeyCode::Char('k'), KeyModifiers::CONTROL)
        );
        assert_eq!(
            k("shift+tab"),
            Key::of(&KeyEvent::new(KeyCode::BackTab, KeyModifiers::SHIFT))
        );
        assert_eq!(k("meta+p"), Key::new(KeyCode::Char('p'), KeyModifiers::ALT));
        assert_eq!(k("K"), k("k"), "case-insensitive, as in Claude Code");
        assert_eq!(
            k("shift+k"),
            Key::of(&KeyEvent::new(KeyCode::Char('K'), KeyModifiers::SHIFT))
        );
        assert_eq!(k("escape"), k("esc"));
        assert!(parse_key("ctl+k").is_none());
    }

    #[test]
    fn remaps_unbinds_and_chords() {
        let (mut km, problems) = map(r#"{"bindings": [
            {"context": "Chat", "bindings": {"ctrl+e": "chat:externalEditor", "ctrl+s": null, "ctrl+k ctrl+t": "app:toggleTodos", "ctrl+q": "chat:fastMode"}},
            {"context": "Confirmation", "bindings": {"y": "confirm:yes"}},
            {"context": "DiffPanel", "bindings": {"ctrl+x b": "app:cycleDiffBase"}}
        ]}"#);
        assert!(problems.is_empty(), "{problems:?}");
        let chat = [Context::Chat, Context::Global];
        assert_eq!(
            km.lookup(&chat, k("ctrl+e")),
            Lookup::Run(keys(&["ctrl+g"]))
        );
        assert_eq!(km.lookup(&chat, k("ctrl+s")), Lookup::Swallow);
        assert_eq!(km.lookup(&chat, k("ctrl+k")), Lookup::Pending);
        assert_eq!(
            km.lookup(&chat, k("ctrl+t")),
            Lookup::Run(keys(&["ctrl+t"]))
        );
        assert_eq!(
            km.lookup(&chat, k("ctrl+q")),
            Lookup::Run(Effect::Nothing),
            "a Claude Code action Chanto lacks"
        );
        assert_eq!(
            km.lookup(&chat, k("y")),
            Lookup::Default,
            "Confirmation bindings only in dialogs"
        );
        assert_eq!(
            km.lookup(&[Context::Confirmation, Context::Global], k("y")),
            Lookup::Run(keys(&["enter"]))
        );
        assert_eq!(km.lookup(&chat, k("ctrl+a")), Lookup::Default);
    }

    #[test]
    fn reports_typos_and_reserved_keys() {
        let (_, problems) = map(
            r#"{"bindings": [{"context": "Chat", "bindings": {"ctrl+c": "chat:submit", "ctl+k": "chat:submit", "ctrl+u": "chat:sumbit"}}]}"#,
        );
        assert_eq!(problems.len(), 3, "{problems:?}");
        assert!(
            problems.iter().any(|p| p.contains("reserved"))
                && problems.iter().any(|p| p.contains("chat:sumbit"))
        );
    }

    #[test]
    fn chantos_own_file_wins_and_edits_reload() {
        let dir =
            std::env::temp_dir().join(format!("chanto-keys-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&dir).unwrap();
        let (claude, ours) = (dir.join("claude.json"), dir.join("chanto.json"));
        std::fs::write(
            &claude,
            r#"{"bindings": [{"context": "Chat", "bindings": {"ctrl+e": "chat:externalEditor"}}]}"#,
        )
        .unwrap();
        std::fs::write(
            &ours,
            r#"{"bindings": [{"context": "Chat", "bindings": {"ctrl+e": null}}]}"#,
        )
        .unwrap();
        let (mut km, _) = Keymap::load(vec![claude, ours.clone()]);
        assert_eq!(km.lookup(&[Context::Chat], k("ctrl+e")), Lookup::Swallow);
        std::thread::sleep(Duration::from_millis(1100));
        std::fs::write(
            &ours,
            r#"{"bindings": [{"context": "Chat", "bindings": {"ctrl+e": "chat:newline"}}]}"#,
        )
        .unwrap();
        km.checked = None;
        assert!(km.refresh().is_some(), "the edit is picked up");
        assert_eq!(
            km.lookup(&[Context::Chat], k("ctrl+e")),
            Lookup::Run(keys(&["ctrl+j"]))
        );
        let _ = std::fs::remove_dir_all(dir);
    }
}
