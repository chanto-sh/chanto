use super::*;

#[test]
fn reads_the_devtools_port_from_the_profile() {
    let dir = std::env::temp_dir().join(format!("chanto-cdp-{}", uuid::Uuid::new_v4().simple()));
    std::fs::create_dir_all(&dir).unwrap();
    let profile = dir.display().to_string();
    assert_eq!(devtools_port(&profile), None, "no file yet");
    // Chrome writes the port on the first line and a ws path on the second.
    std::fs::write(
        dir.join("DevToolsActivePort"),
        "54321\n/devtools/browser/abc\n",
    )
    .unwrap();
    assert_eq!(devtools_port(&profile), Some(54321));
    std::fs::write(dir.join("DevToolsActivePort"), "not-a-port\n").unwrap();
    assert_eq!(devtools_port(&profile), None);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_headless_user_agent_is_presented_as_a_windowed_one() {
    let headless = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 \
                        (KHTML, like Gecko) HeadlessChrome/156.0.0.0 Safari/537.36";
    assert_eq!(
        normal_user_agent(headless).as_deref(),
        Some(
            "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 \
                 (KHTML, like Gecko) Chrome/156.0.0.0 Safari/537.36"
        )
    );
    let normal = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 \
                      (KHTML, like Gecko) Chrome/156.0.0.0 Safari/537.36";
    assert_eq!(normal_user_agent(normal), None);
    assert_eq!(normal_user_agent(""), None);
}
