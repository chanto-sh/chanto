use super::app_tests::*;
use super::*;

fn checkpoint(prompt: &str) -> crate::agent::Checkpoint {
    crate::agent::Checkpoint {
        id: String::new(),
        timestamp: String::new(),
        tree: None,
        msg_index: 1,
        prompt: prompt.into(),
        files: Default::default(),
    }
}

fn picker(t: &mut TestApp) -> (String, Vec<String>, Vec<String>) {
    match t.sent().pop() {
        Some(AppMsg::Picker {
            title,
            labels,
            values,
        }) => (title, labels, values),
        _ => panic!("no picker"),
    }
}

#[tokio::test]
async fn rewind_and_fork_list_checkpoints_newest_first() {
    let mut t = test_app();
    t.env
        .checkpoints
        .lock()
        .unwrap()
        .extend([checkpoint("first"), checkpoint("second")]);
    assert!(t.open_picker("/rewind"));
    let (title, labels, values) = picker(&mut t);
    assert!(title.starts_with("Rewind"));
    assert_eq!(labels, ["second", "first"]);
    assert_eq!(values, ["/rewind 1", "/rewind 0"]);
    assert!(t.open_picker("/fork"));
    let (title, labels, values) = picker(&mut t);
    assert_eq!(title, "Fork before a prompt");
    assert_eq!(labels[0], "Current conversation");
    assert_eq!(
        values,
        ["/fork --at-current", "/fork --at 1", "/fork --at 0"]
    );
}

#[tokio::test]
async fn rewinding_to_a_checkpoint_asks_what_to_restore() {
    let mut t = test_app();
    t.env.checkpoints.lock().unwrap().push(checkpoint("first"));
    assert!(t.open_picker("/rewind 0"));
    let (title, labels, values) = picker(&mut t);
    assert_eq!(title, "Rewind: File-edit fallback");
    assert_eq!(labels.len(), 5);
    assert_eq!(values[0], "/rewind 0 both");
    assert_eq!(values[3], "/edit-prompt 0 both --edit");
    // `/rewind undo` is a command, not a picker.
    assert!(!t.open_picker("/rewind undo"));
}

#[tokio::test]
async fn editing_an_earlier_prompt_puts_it_in_the_input() {
    let mut t = test_app();
    t.env.checkpoints.lock().unwrap().push(checkpoint(
        "fix it\n\n<system-reminder>ctx</system-reminder>",
    ));
    assert!(t.open_picker("/edit-prompt 0 conversation --edit"));
    assert_eq!(t.input.text, "fix it");
    assert_eq!(t.editing_prompt, Some((0, "conversation")));
    assert!(t.hint_text().contains("files stay as they are"));
    assert!(t.open_picker("/edit-prompt 0 both --edit"));
    assert!(t.hint_text().contains("files go back to that point too"));
    assert!(t.open_picker("/edit-prompt 7 both --edit"));
    assert_eq!(
        t.notes(),
        ["That prompt is no longer in this session's history."]
    );
}

#[tokio::test]
async fn theme_and_resume_pickers() {
    let mut t = test_app();
    assert!(t.open_picker("/theme"));
    let (title, labels, values) = picker(&mut t);
    assert_eq!(title, "Theme");
    assert_eq!(labels[0], "auto (match the terminal)");
    assert_eq!(values[0], "/theme auto");
    assert_eq!(labels.len(), values.len());
    // No saved sessions for this directory.
    assert!(t.open_picker("/resume"));
    assert!(t.notes().last().unwrap().starts_with("No saved sessions"));
    assert!(t.open_picker("/resume --all"));
    assert_eq!(
        t.notes().last().unwrap(),
        "No saved sessions for this directory."
    );
    assert!(!t.open_picker("/help"));
}

#[test]
fn model_search_heading_offers_the_typed_name() {
    let labels = vec!["gpt-5".to_string(), "qwen3".to_string()];
    let mut s = PickerSearch::new(&labels, labels.clone(), false);
    s.query = "q3".into();
    s.refilter();
    assert_eq!(s.indices, vec![1]);
    assert!(s.heading().ends_with("Search: q3"));
    s.query = "zzz".into();
    s.refilter();
    assert!(s.heading().contains("Enter uses `zzz` as the model name"));
    let mut s = PickerSearch::new(&labels, labels.clone(), true);
    assert!(s.heading().starts_with("**Resume a session"));
    s.query = "gpt".into();
    s.refilter();
    assert!(s.heading().ends_with("Search: gpt"));
}

#[test]
fn fuzzy_scores_prefer_contiguous_basename_matches() {
    assert_eq!(fuzzy_score("anything", ""), Some(0));
    assert!(fuzzy_score("src/main.rs", "main") > fuzzy_score("main/src.rs", "main"));
    assert!(fuzzy_score("src/main.rs", "smr").is_some());
    assert!(fuzzy_score("src/main.rs", "mn").unwrap() < fuzzy_score("src/main.rs", "ma").unwrap());
    assert_eq!(fuzzy_score("abc", "x"), None);
}

#[test]
fn file_listing_respects_ignore_files() {
    let dir = crate::agent::tests::tmpdir("tui-files");
    std::fs::create_dir_all(dir.join("src")).unwrap();
    std::fs::write(dir.join("src/lib.rs"), "").unwrap();
    std::fs::write(dir.join(".ignore"), "build.log\n").unwrap();
    std::fs::write(dir.join("build.log"), "").unwrap();
    let files = list_files(&dir);
    assert!(files.contains(&"src/".to_string()) && files.contains(&"src/lib.rs".to_string()));
    assert!(!files.iter().any(|f| f == "build.log"));
    let ignored = list_ignored_files(&dir, &files);
    assert_eq!(ignored, ["build.log"]);
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn resume_search_filters_by_title_and_first_prompt() {
    let labels = vec![
        "10:00 · Auth refactor".to_string(),
        "11:00 · write the README".to_string(),
    ];
    let hay = vec![
        "Auth refactor\nmove login to OAuth tokens\nid-1".to_string(),
        "\nwrite the README\nid-2".to_string(),
    ];
    let mut s = PickerSearch::new(&labels, hay, true);
    s.query = "oauth LOGIN".into();
    s.refilter();
    assert_eq!(s.indices, vec![0]);
    assert!(s.heading().contains("Search: oauth LOGIN") && s.heading().contains("ctrl+d delete"));
    s.query = "readme".into();
    s.refilter();
    assert_eq!(s.indices, vec![1]);
    s.query = "nothing".into();
    s.refilter();
    assert!(s.indices.is_empty() && s.heading().contains("No session"));
    s.query.clear();
    s.refilter();
    assert_eq!(s.indices, vec![0, 1]);
}
