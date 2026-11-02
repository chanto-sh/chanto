use super::*;

#[test]
fn setup_command_quotes_paths_with_spaces() {
    #[cfg(not(windows))]
    {
        assert_eq!(
            setup_command(Some(PathBuf::from("/usr/local/bin/chanto"))),
            "/usr/local/bin/chanto setup"
        );
        assert_eq!(
            setup_command(Some(PathBuf::from("/My Apps/chanto"))),
            "'/My Apps/chanto' setup"
        );
    }
    assert_eq!(setup_command(None), "chanto setup");
}

#[test]
fn token_is_injected_only_into_the_daemons_origin() {
    let s = daemon::Server {
        url: "http://127.0.0.1:4141".into(),
        token: "t'\"</script>".into(),
    };
    let js = init_script(&s);
    assert!(js.starts_with("(() => { if (location.origin !== \"http://127.0.0.1:4141\") return;"));
    // JSON-encoded, so a token can't break out of the string.
    assert!(js.contains(r#"sessionStorage.setItem('chanto-token', "t'\"</script>")"#));
    assert!(!js.contains("#token="));
}

#[test]
fn init_keeps_a_token_the_page_already_has() {
    let s = daemon::Server {
        url: "http://127.0.0.1:4141".into(),
        token: "abc".into(),
    };
    let js = init_script(&s);
    // A daemon restarted outside the app hands the page a new token in its
    // fragment; a later reload must keep that one, not this startup copy.
    assert!(js.contains("if (!sessionStorage.getItem('chanto-token'))"));
}

#[test]
fn navigation_stays_on_the_daemon() {
    let o = "http://127.0.0.1:4141";
    for ok in [
        "http://127.0.0.1:4141",
        "http://127.0.0.1:4141/",
        "http://127.0.0.1:4141/#run=abc",
        "http://127.0.0.1:4141/app.js",
    ] {
        assert!(same_origin(o, ok), "{ok}");
    }
    for bad in [
        "http://127.0.0.1:41410/",
        "http://127.0.0.1:4141.evil.com/",
        "https://github.com/",
        "file:///etc/passwd",
    ] {
        assert!(!same_origin(o, bad), "{bad}");
    }
    // Before a daemon exists only the shell's error page may load.
    assert!(navigation_allowed(None, "about:blank"));
    assert!(navigation_allowed(None, "data:text/html,hi"));
    assert!(!navigation_allowed(None, "https://example.com/"));
    assert!(navigation_allowed(Some(o), "http://127.0.0.1:4141/app.js"));
    assert!(!navigation_allowed(Some(o), "https://example.com/"));
    // The error page may talk to the shell; other pages may not.
    assert!(ipc_trusted(None, "about:blank"));
    assert!(ipc_trusted(Some(o), "http://127.0.0.1:4141/"));
    assert!(!ipc_trusted(Some(o), "https://example.com/"));
}

#[test]
fn a_launch_without_a_live_project_asks_for_one() {
    assert!(needs_project(None));
    assert!(needs_project(Some("")));
    assert!(needs_project(Some("/definitely/not/here")));
    let dir = std::env::temp_dir();
    assert!(!needs_project(Some(dir.to_str().unwrap())));
}

#[test]
fn hooks_and_titles() {
    assert_eq!(
        hook("openProject", Some("/a \"b\"")),
        r#"window.chanto && window.chanto.openProject("/a \"b\"");"#
    );
    assert_eq!(
        hook("newSession", None),
        "window.chanto && window.chanto.newSession();"
    );
    assert_eq!(title("", 0), "chanto");
    assert_eq!(title("/Users/me/api", 2), "api — chanto · 2 waiting");
    assert!(error_page("<b>x</b>").contains("&lt;b&gt;x&lt;/b&gt;"));
}

#[test]
fn a_project_named_chanto_does_not_repeat_in_the_title() {
    assert_eq!(title("/Users/me/chanto", 0), "chanto");
    assert_eq!(title("/Users/me/chanto", 1), "chanto · 1 waiting");
}

#[test]
fn the_error_page_offers_a_retry_and_the_log() {
    let page = error_page_with_log("<b>x</b>", None);
    assert!(page.contains("&lt;b&gt;x&lt;/b&gt;"));
    // The error page can start the daemon without reopening the app.
    assert!(page.contains("id=retry"));
    assert!(page.contains("Try again"));
    assert!(page.contains("type: 'retry'"));
    // Commands in backticks read as code, not as literal backticks.
    let page = error_page_with_log("`chanto daemon start` failed", None);
    assert!(page.contains("<code>chanto daemon start</code>"));
    assert!(!page.contains("`chanto"));
    // A log that exists gets a button that opens it.
    assert!(!page.contains("id=logs"));
    let logged = error_page_with_log("it broke", Some(Path::new("/tmp/daemon.log")));
    assert!(logged.contains("id=logs"));
    assert!(logged.contains("Open log"));
    assert!(logged.contains("type: 'logs'"));
    // It reports itself, so CHANTO_DESKTOP_SNAPSHOT doesn't wait forever.
    assert!(page.contains("type: 'error-page'"));
}

#[test]
fn the_error_page_carries_the_app_mark() {
    let page = error_page("it broke");
    assert!(page.contains("class=mark"));
    assert!(page.contains(r#"aria-label="chanto logo""#));
}

#[test]
fn a_page_url_carries_the_project_and_a_recovered_one_its_token() {
    let server = daemon::Server {
        url: "http://127.0.0.1:4141".into(),
        token: "abc".into(),
    };
    assert_eq!(page_url(&server, None), "http://127.0.0.1:4141/");
    let dir = std::env::temp_dir();
    let dir = dir.to_str().unwrap();
    assert_eq!(
        page_url(&server, Some(dir)),
        format!("http://127.0.0.1:4141/#project={dir}")
    );
    // A folder that no longer exists is not reopened.
    assert_eq!(
        page_url(&server, Some("/definitely/not/here")),
        "http://127.0.0.1:4141/"
    );
    assert_eq!(
        retry_url(&server, Some(dir)),
        format!("http://127.0.0.1:4141/#token=abc&project={dir}")
    );
    assert_eq!(retry_url(&server, None), "http://127.0.0.1:4141/#token=abc");
    assert_eq!(fragment_value("/Users/me/My App"), "/Users/me/My%20App");
    assert_eq!(fragment_value("a/b?c#d&e"), "a/b%3Fc%23d%26e");
}
