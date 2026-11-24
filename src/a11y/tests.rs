use super::resolve;

fn none(_: &str) -> bool {
    false
}

#[test]
fn screen_reader_comes_from_config_or_env() {
    assert!(resolve(Some(true), None, none, || false).0);
    assert!(resolve(None, None, |k| k == "CLAUDE_AX_SCREEN_READER", || false).0);
    assert!(resolve(None, None, |k| k == "CHANTO_AX_SCREEN_READER", || false).0);
    assert!(!resolve(Some(false), None, none, || false).0);
}

#[test]
fn reduced_motion_follows_config_env_and_the_system() {
    // An explicit config value wins; `animations = true` overrides the system.
    assert!(
        !resolve(None, Some(true), none, || true).1,
        "explicit on beats the system setting"
    );
    assert!(resolve(None, Some(false), none, || false).1);
    assert!(resolve(None, None, |k| k == "CHANTO_REDUCE_MOTION", || false).1);
    assert!(
        resolve(None, None, none, || true).1,
        "the system Reduce motion setting is honored"
    );
    assert!(!resolve(None, None, none, || false).1);
}

#[test]
fn a_screen_reader_forces_motion_off() {
    // Even with animations explicitly on, a screen reader turns motion off.
    assert!(resolve(Some(true), Some(true), none, || false).1);
}
