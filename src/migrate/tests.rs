use super::*;

#[test]
fn the_toml_block_is_preferred_over_other_fences() {
    let reply = "```json\n{}\n```\nthen\n```toml\nmodel = \"m\"\n```\n## What mapped\n- model";
    assert_eq!(extract_toml(reply).as_deref(), Some("model = \"m\""));
    assert_eq!(
        extract_toml("```\nmode = \"plan\"\n```").as_deref(),
        Some("mode = \"plan\"")
    );
    // An empty or unterminated block is no config. (The empty ```toml block
    // used to come back as the config "toml": its language tag.)
    assert_eq!(extract_toml("```toml\n\n```"), None);
    assert_eq!(extract_toml("```yaml\na: 1\n```").as_deref(), Some("a: 1"));
    assert_eq!(extract_toml("```toml\nmodel = 1"), None);
    assert_eq!(extract_toml("no code at all"), None);
}

#[test]
fn notes_start_at_the_first_summary_heading() {
    assert_eq!(
        notes("```toml\nx = 1\n```\n\n## What mapped\n- x\n## Skipped\n- y\n"),
        "## What mapped\n- x\n## Skipped\n- y"
    );
    assert_eq!(notes("prose\n## Skipped\n- all"), "## Skipped\n- all");
    assert_eq!(notes("nothing to report"), "");
}

#[test]
fn home_relative_locations_expand() {
    let home = dirs::home_dir().unwrap();
    assert_eq!(expand("~/.aider.conf.yml"), home.join(".aider.conf.yml"));
    assert_eq!(
        expand(".cursorrules"),
        std::path::PathBuf::from(".cursorrules")
    );
    // Every known tool has somewhere to look.
    assert!(KNOWN.iter().all(|(_, locs)| !locs.is_empty()));
}
