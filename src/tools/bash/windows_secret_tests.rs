use super::*;

#[test]
fn expands_windows_private_keys_and_credential_paths() {
    let home = std::env::temp_dir().join(format!("chanto-win-secrets-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&home).unwrap();
    // Denied paths are canonical; the temp dir may be an 8.3 short name (RUNNER~1).
    let home = crate::util::canonical(&home).unwrap();
    for name in [
        ".ssh/id_ed25519",
        ".ssh/id_ed25519_sk",
        ".ssh/id_ed25519.pub",
        ".aws/credentials",
        "AppData/Roaming/gcloud/credentials.db",
    ] {
        let path = home.join(name);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, "fixture").unwrap();
    }
    let denied = read_denied_paths(&home, &[]);
    assert!(denied.contains(&home.join(".ssh/id_ed25519")));
    assert!(denied.contains(&home.join(".ssh/id_ed25519_sk")));
    assert!(!denied.contains(&home.join(".ssh/id_ed25519.pub")));
    assert!(denied.contains(&home.join(".aws/credentials")));
    assert!(denied.contains(&home.join("AppData/Roaming/gcloud/credentials.db")));
    std::fs::remove_dir_all(home).unwrap();
}
