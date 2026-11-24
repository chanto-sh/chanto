/// Everything released from this repository reports the CLI's version: the
/// desktop app (About, `--version`, the bundle) and both editor plugins.
/// A stale manifest makes an installed copy look like no update arrived.
#[test]
fn shipped_manifests_agree_on_the_version() {
    let version = env!("CARGO_PKG_VERSION");
    let field = |text: &str, prefix: &str| {
        text.lines()
            .find_map(|l| l.trim().strip_prefix(prefix))
            .and_then(|v| v.trim_end_matches(',').strip_suffix('"'))
            .unwrap_or_else(|| panic!("no {prefix} field"))
            .to_string()
    };
    assert_eq!(
        field(include_str!("../desktop/Cargo.toml"), "version = \""),
        version,
        "desktop/Cargo.toml"
    );
    assert_eq!(
        field(
            include_str!("../ide/vscode/package.json"),
            "\"version\": \""
        ),
        version,
        "ide/vscode/package.json"
    );
    assert_eq!(
        field(
            include_str!("../ide/jetbrains/build.gradle.kts"),
            "version = \""
        ),
        version,
        "ide/jetbrains/build.gradle.kts"
    );
}

/// Every surface that can be installed on its own carries the app mark:
/// the VS Code extension's store icon and the JetBrains plugin icon. A
/// missing or malformed one ships as a blank square.
#[test]
fn shipped_marks_are_present() {
    let png = include_bytes!("../ide/vscode/icon.png");
    assert_eq!(
        &png[..8],
        b"\x89PNG\r\n\x1a\n",
        "ide/vscode/icon.png is a PNG"
    );
    // Width and height lead the IHDR chunk, big-endian.
    assert_eq!(
        &png[16..24],
        &[0, 0, 0, 128, 0, 0, 0, 128],
        "ide/vscode/icon.png is 128×128"
    );
    assert!(include_str!("../ide/vscode/package.json").contains("\"icon\": \"icon.png\""));
    assert!(
        include_str!("../ide/jetbrains/src/main/resources/META-INF/pluginIcon.svg")
            .contains("<svg")
    );
    assert!(include_str!("../assets/logo.svg").contains("<svg"));
}
