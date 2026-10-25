use super::*;

fn scratch(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!(
        "chanto-check-{name}-{}-{}",
        std::process::id(),
        uuid::Uuid::new_v4().simple()
    ));
    std::fs::create_dir_all(&d).unwrap();
    d
}

async fn output(mut c: tokio::process::Command) -> (bool, String) {
    let o = c.output().await.unwrap();
    (
        o.status.success(),
        String::from_utf8_lossy(&o.stdout).into_owned() + &String::from_utf8_lossy(&o.stderr),
    )
}

#[tokio::test]
async fn checks_do_not_see_secret_env() {
    let dir = scratch("env");
    // A name only this test uses, so setting it disturbs nothing else.
    let name = format!("CHANTO_CHECK_PROBE_{}_API_KEY", std::process::id());
    unsafe { std::env::set_var(&name, "not-for-checks") };
    let (c, _) = check_command_with(
        &format!("echo \"[${{{name}:-withheld}}]\""),
        &dir,
        &[("EXTRA".into(), "kept".into())],
        crate::config::SandboxConfig {
            enabled: false,
            ..Default::default()
        },
        &crate::secrets::SecretsConfig::default(),
    )
    .unwrap();
    let (ok, out) = output(c).await;
    unsafe { std::env::remove_var(&name) };
    assert!(ok, "{out}");
    assert!(out.contains("[withheld]"), "{out}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn checks_run_in_the_bash_sandbox() {
    if !crate::tools::bash::sandbox_available() {
        return;
    }
    let _g = crate::tools::bash::SANDBOX_TEST_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let was = crate::tools::bash::SANDBOX_ACTIVE.swap(true, std::sync::atomic::Ordering::Relaxed);
    if !crate::tools::bash::sandbox_on() {
        // Nested inside another sandbox: Chanto's can't start here.
        crate::tools::bash::SANDBOX_ACTIVE.store(was, std::sync::atomic::Ordering::Relaxed);
        return;
    }
    let project = scratch("sbx");
    let secret = scratch("sbx-secret");
    std::fs::write(secret.join("key"), "s3cret").unwrap();
    let sandbox = crate::config::SandboxConfig {
        deny_read: vec![secret.display().to_string()],
        ..Default::default()
    };
    let secrets = crate::secrets::SecretsConfig::default();
    let (c, _) = check_command_with(
        "echo built > out.txt",
        &project,
        &[],
        sandbox.clone(),
        &secrets,
    )
    .unwrap();
    let (ok, out) = output(c).await;
    let wrote = project.join("out.txt").exists();
    let (c, _) = check_command_with(
        &format!("cat '{}'", secret.join("key").display()),
        &project,
        &[],
        sandbox,
        &secrets,
    )
    .unwrap();
    let (read_ok, read_out) = output(c).await;
    crate::tools::bash::SANDBOX_ACTIVE.store(was, std::sync::atomic::Ordering::Relaxed);
    assert!(ok && wrote, "a check writes its build output: {out}");
    assert!(
        !read_ok && !read_out.contains("s3cret"),
        "the sandbox's read denies apply to checks: {read_out}"
    );
    let _ = std::fs::remove_dir_all(&project);
    let _ = std::fs::remove_dir_all(&secret);
}

#[test]
fn pytest_probe_runs_no_project_code() {
    let Some(py) = ["python3", "python"]
        .into_iter()
        .find(|py| std::process::Command::new(py).arg("-V").output().is_ok())
    else {
        return;
    };
    // Even run from a hostile project, the probe must not import its pytest.py.
    let dir = scratch("pytest");
    let marker = dir.join("ran");
    std::fs::write(
        dir.join("pytest.py"),
        format!("open({:?}, 'w').write('x')\n", marker.display().to_string()),
    )
    .unwrap();
    has_pytest(py, &dir);
    assert!(
        !marker.exists(),
        "the probe imported the project's pytest.py"
    );
    assert!(!neutral_dir().join("pytest.py").exists());
    let _ = std::fs::remove_dir_all(&dir);
}
