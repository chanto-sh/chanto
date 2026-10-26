//! The container lifecycle against a fake engine: a shell script standing in
//! for `docker` that logs its arguments and answers like the real one, so
//! these run without Docker installed.

use super::*;
use crate::runs::tests::{TestHome, make_repo, sh};
use serde_json::json;

/// A fake engine at `<dir>/<name>` running `body` after logging its
/// arguments, one call per line, to the returned log file.
fn fake_engine(dir: &Path, name: &str, body: &str) -> (String, PathBuf) {
    use std::os::unix::fs::PermissionsExt;
    let path = dir.join(name);
    let log = dir.join(format!("{name}.log"));
    std::fs::write(
        &path,
        format!(
            "#!/bin/sh\nprintf '%s\\n' \"$*\" >> '{}'\n{body}\nexit 0\n",
            log.display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    // A file just written can briefly be "busy" for exec while another
    // test's fork still holds it open.
    for _ in 0..50 {
        match std::process::Command::new(&path).arg("probe").status() {
            Err(e) if e.raw_os_error() == Some(libc::ETXTBSY) => {
                std::thread::sleep(Duration::from_millis(20))
            }
            _ => break,
        }
    }
    let _ = std::fs::remove_file(&log);
    (path.display().to_string(), log)
}

/// Answers like a working Docker.
const WORKING: &str = r#"case "$1 $2" in
  "info "*) echo 27.0.0 ;;
  "network inspect") echo 127.0.0.1 ;;
  "network ls") echo net1 ;;
  "container ls") echo c1; echo c2 ;;
  "run -d") case "$*" in *broken:image*) echo 'manifest unknown' >&2; exit 1 ;; esac; echo cid ;;
esac"#;

/// Installed, but its daemon isn't running.
const DOWN: &str = r#"case "$1" in
  info) echo 'Cannot connect to the Docker daemon' >&2; exit 1 ;;
esac"#;

fn calls(log: &Path) -> Vec<String> {
    std::fs::read_to_string(log)
        .unwrap_or_default()
        .lines()
        .map(str::to_string)
        .collect()
}

fn wait_for_call(log: &Path, f: impl Fn(&str) -> bool) -> String {
    for _ in 0..200 {
        if let Some(c) = calls(log).into_iter().find(|c| f(c)) {
            return c;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    panic!("no such call in {:?}", calls(log));
}

fn iso(engine: &str, network: NetMode) -> Isolation {
    Isolation {
        engine: engine.into(),
        image: "rust:1-bookworm".into(),
        network,
        cpus: Some(2.0),
        memory: Some("4g".into()),
    }
}

/// A run whose worktree is a linked git worktree of a fresh repository.
fn run_in_worktree(home: &TestHome) -> (crate::runs::Run, PathBuf) {
    let repo = make_repo(home);
    let id = format!("c{}", &uuid::Uuid::new_v4().simple().to_string()[..7]);
    let wt = home.dir.join("wt").join(&id);
    sh(
        &repo,
        &[
            "worktree",
            "add",
            "-q",
            "-b",
            &id,
            &wt.display().to_string(),
        ],
    );
    let wt = wt.canonicalize().unwrap();
    let run: crate::runs::Run = serde_json::from_value(json!({
        "id": id, "task": "t", "state": "running", "created": "", "worktree": wt, "branch": id,
        "base": "x", "settings": {}, "source": {"kind": "local", "path": repo, "head": "x"}
    }))
    .unwrap();
    (run, repo)
}

#[test]
fn engines_that_are_missing_or_down_are_explained() {
    let home = TestHome::new("engine-check");
    let e = check_engine(&home.dir.join("nope").join("docker").display().to_string())
        .unwrap_err()
        .to_string();
    assert!(e.contains("isn't installed or isn't on PATH"), "{e}");
    let (down, _) = fake_engine(&home.dir, "docker-down", DOWN);
    let e = check_engine(&down).unwrap_err().to_string();
    assert!(
        e.contains("engine isn't running (Cannot connect to the Docker daemon)"),
        "{e}"
    );
    let (ok, log) = fake_engine(&home.dir, "docker", WORKING);
    check_engine(&ok).unwrap();
    assert_eq!(calls(&log), ["info --format {{.ServerVersion}}"]);
    // A command that doesn't answer in time is killed.
    let e = output_timeout(
        std::process::Command::new("sleep").arg("5"),
        Duration::from_millis(100),
    )
    .unwrap_err()
    .to_string();
    assert_eq!(e, "no answer within 0s");
}

#[test]
fn cleanup_removes_only_labeled_resources() {
    let home = TestHome::new("engine-cleanup");
    let (eng, log) = fake_engine(&home.dir, "docker", WORKING);
    remove_run(&eng, "r1");
    assert_eq!(
        calls(&log),
        [
            "container ls -q -a --filter label=chanto.run=r1",
            "rm -f c1 c2",
            "network ls -q --filter label=chanto.run=r1",
            "network rm net1",
        ]
    );
    std::fs::remove_file(&log).unwrap();
    let store = home.store().dir;
    assert_eq!(sweep(&eng, &store), 2);
    assert_eq!(
        calls(&log)[0],
        format!(
            "container ls -q -a --filter label=chanto.store={}",
            store_key(&store)
        )
    );
    // Nothing to do without an engine.
    let missing = home.dir.join("none").display().to_string();
    assert_eq!(sweep(&missing, &store), 0);
    remove_run(&missing, "r1");
    // Nothing labeled: no rm calls.
    let (empty, log) = fake_engine(&home.dir, "docker-empty", "");
    assert_eq!(sweep(&empty, &store), 0);
    assert!(
        calls(&log).iter().all(|c| c.contains(" ls ")),
        "{:?}",
        calls(&log)
    );
}

#[tokio::test]
async fn a_run_container_mounts_its_worktree_and_runs_commands_there() {
    let home = TestHome::new("engine-start");
    let (eng, log) = fake_engine(&home.dir, "docker", WORKING);
    let (run, repo) = run_in_worktree(&home);
    let notes = Mutex::new(Vec::<String>::new());
    let c = start(&run, &iso(&eng, NetMode::None), true, &|n| {
        notes.lock().unwrap().push(n)
    })
    .await
    .unwrap();
    assert_eq!(c.name, format!("chanto-run-{}", run.id));
    assert_eq!(c.worktree, run.worktree);
    assert!(c.proxy.is_none() && c.auto_allow);
    assert_eq!(c.network, NetMode::None);
    let scratch = home.store().dir.join(&run.id).join("container");
    assert!(scratch.is_dir());
    assert_eq!(c.scratch, scratch);
    assert!(notes.lock().unwrap()[0].starts_with("Starting the run's "));

    let all = calls(&log);
    // Leftovers of an earlier worker are removed first.
    assert!(all.iter().any(|c| c.starts_with("rm -f c1 c2")), "{all:?}");
    let run_call = all.iter().find(|c| c.starts_with("run -d")).unwrap();
    let wt = run.worktree.display().to_string();
    let common = repo.join(".git").canonicalize().unwrap();
    for want in [
        format!("--name chanto-run-{}", run.id),
        "--network none".into(),
        "--cpus 2 --memory 4g".into(),
        format!("-v {wt}:{wt} "),
        format!("-v {0}:{0} ", scratch.display()),
        format!("-v {0}:{0}:ro", common.display()),
        format!("-w {wt} --entrypoint tail rust:1-bookworm -f /dev/null"),
    ] {
        assert!(run_call.contains(&want), "{want:?} not in {run_call}");
    }
    // The worktree's own git dir (index, HEAD) is writable.
    let own = common.join("worktrees").join(&run.id);
    assert!(
        run_call.contains(&format!("-v {0}:{0} ", own.display())),
        "{run_call}"
    );

    // Commands from inside the worktree go to this container.
    assert_eq!(for_path(&run.worktree.join("src")).unwrap().name, c.name);
    let (cmd, token) = c.command(
        "cargo test",
        &home.dir,
        &[("EXTRA".to_string(), "1".to_string())],
    );
    let std = cmd.as_std();
    assert_eq!(std.get_program(), eng.as_str());
    let a: Vec<String> = std
        .get_args()
        .map(|a| a.to_string_lossy().to_string())
        .collect();
    // A cwd outside the worktree starts in the worktree.
    assert_eq!(&a[..3], ["exec", "-w", wt.as_str()]);
    assert!(a.contains(&format!("CHANTO_EXEC={token}")));
    assert!(a.contains(&"EXTRA=1".to_string()) && a.contains(&"CHANTO=1".to_string()));
    assert_eq!(a.last().unwrap(), "cargo test");
    let (cmd, _) = c.command("ls", &run.worktree.join("sub"), &[]);
    assert_eq!(
        cmd.as_std().get_args().nth(2).unwrap().to_string_lossy(),
        run.worktree.join("sub").display().to_string()
    );
    // No relay, no proxy settings.
    let lease =
        crate::network::Lease::new(&crate::config::SandboxConfig::default(), &home.dir).unwrap();
    assert!(c.proxy_env(&lease).is_empty());

    // Killing a tracked `docker exec` client kills its command in the container.
    track(Some(4_000_001), &c, "tok-a");
    untrack(Some(4_000_001));
    kill_tracked(4_000_001);
    track(Some(4_000_002), &c, "tok-b");
    kill_tracked(4_000_002);
    let k = wait_for_call(&log, |l| l.starts_with(&format!("exec {} sh -c", c.name)));
    assert!(k.contains("CHANTO_EXEC=tok-b"), "{k}");
    assert!(!calls(&log).iter().any(|l| l.contains("CHANTO_EXEC=tok-a")));
    track(None, &c, "x");
    untrack(None);

    unregister(&c.name);
    assert!(for_path(&run.worktree).is_none_or(|x| x.name != c.name));
}

#[tokio::test]
async fn allowlist_networking_adds_a_relay_on_an_internal_network() {
    let home = TestHome::new("engine-allow");
    let (eng, log) = fake_engine(&home.dir, "docker", WORKING);
    let (run, _) = run_in_worktree(&home);
    let c = start(&run, &iso(&eng, NetMode::Allowlist), false, &|_| {})
        .await
        .unwrap();
    assert_eq!(c.proxy.as_deref(), Some("chanto-proxy:3128"));
    let all = calls(&log);
    let net = format!("chanto-run-{}-net", run.id);
    assert!(
        all.iter()
            .any(|c| c.starts_with("network create --internal --label") && c.ends_with(&net)),
        "{all:?}"
    );
    let relay = all
        .iter()
        .find(|c| c.contains(&format!("--name chanto-run-{}-proxy", run.id)))
        .unwrap();
    assert!(
        relay.contains("alpine:latest nc -lk -p 3128 -e nc host.docker.internal"),
        "{relay}"
    );
    assert!(all.contains(&format!(
        "network connect --alias chanto-proxy {net} chanto-run-{}-proxy",
        run.id
    )));
    let main = all
        .iter()
        .find(|c| c.contains(&format!("--name chanto-run-{} ", run.id)))
        .unwrap();
    assert!(main.contains(&format!("--network {net}")), "{main}");
    // Each command gets the proxy, with its lease's credentials.
    let lease =
        crate::network::Lease::new(&crate::config::SandboxConfig::default(), &home.dir).unwrap();
    let env: HashMap<String, String> = c.proxy_env(&lease).into_iter().collect();
    assert!(
        env["HTTPS_PROXY"].starts_with("http://")
            && env["HTTPS_PROXY"].ends_with("@chanto-proxy:3128")
    );
    assert_eq!(env["https_proxy"], env["HTTPS_PROXY"]);
    assert_eq!(env["NO_PROXY"], "localhost");
    unregister(&c.name);
}

#[tokio::test]
async fn a_container_that_cannot_start_is_cleaned_up() {
    let home = TestHome::new("engine-fail");
    let (eng, log) = fake_engine(&home.dir, "docker", WORKING);
    let (run, _) = run_in_worktree(&home);
    let mut i = iso(&eng, NetMode::Full);
    i.image = "broken:image".into();
    let e = start(&run, &i, false, &|_| {}).await.unwrap_err();
    let msg = format!("{e:#}");
    assert!(
        msg.starts_with(&format!(
            "could not start the {eng} container for this run: `{eng} run -d` failed: manifest unknown"
        )),
        "{msg}"
    );
    let all = calls(&log);
    let run_at = all.iter().position(|c| c.starts_with("run -d")).unwrap();
    assert!(
        !all[run_at].contains("--network"),
        "full: the default bridge"
    );
    assert!(
        all[run_at..].iter().any(|c| c.starts_with("rm -f")),
        "removed after the failure: {all:?}"
    );
    assert!(for_path(&run.worktree).is_none_or(|x| !x.name.contains(&run.id)));

    // An engine that's down fails before anything is created.
    let (down, log) = fake_engine(&home.dir, "docker-down", DOWN);
    let e = start(&run, &iso(&down, NetMode::None), false, &|_| {})
        .await
        .unwrap_err();
    assert!(e.to_string().contains("isn't running"), "{e}");
    assert_eq!(calls(&log), ["info --format {{.ServerVersion}}"]);
}

#[test]
fn mounts_of_a_plain_checkout_are_just_the_worktree_and_scratch() {
    let home = TestHome::new("engine-mounts");
    let repo = make_repo(&home);
    let scratch = home.dir.join("scratch");
    let m = mounts(&repo, &scratch);
    assert_eq!(
        m,
        vec![
            Mount {
                path: repo.clone(),
                read_only: false
            },
            Mount {
                path: scratch,
                read_only: false
            }
        ]
    );
    // Not a git checkout at all: the same two.
    let plain = home.dir.join("plain");
    std::fs::create_dir_all(&plain).unwrap();
    assert_eq!(mounts(&plain, &home.dir).len(), 2);
}

#[test]
fn a_worktrees_records_are_mounted_read_only() {
    let home = TestHome::new("engine-wt-mounts");
    let repo = make_repo(&home);
    let wt = home.dir.join("wt");
    sh(
        &repo,
        &[
            "worktree",
            "add",
            "-q",
            "-b",
            "w",
            &wt.display().to_string(),
        ],
    );
    let m = mounts(&wt, &home.dir.join("scratch"));
    let gd = crate::hostgit::worktree_git_dir(&repo, &wt).unwrap();
    let mode = |p: &Path| {
        let p = std::fs::canonicalize(p).unwrap();
        m.iter()
            .find(|x| std::fs::canonicalize(&x.path).ok().as_deref() == Some(p.as_path()))
            .map(|x| x.read_only)
    };
    assert_eq!(mode(&gd), Some(false), "{m:?}");
    assert_eq!(mode(&gd.join("commondir")), Some(true), "{m:?}");
    assert_eq!(mode(&gd.join("gitdir")), Some(true), "{m:?}");
}

#[test]
fn isolation_labels_and_net_modes() {
    let mut i = iso("docker", NetMode::Allowlist);
    assert_eq!(
        i.label(),
        "docker · rust:1-bookworm · network allowlist · 2 cpus · 4g memory"
    );
    i.cpus = None;
    i.memory = None;
    i.network = NetMode::Full;
    assert_eq!(i.label(), "docker · rust:1-bookworm · network full");
    for n in [NetMode::None, NetMode::Allowlist, NetMode::Full] {
        assert_eq!(NetMode::parse(n.label()), Some(n));
    }
    assert_eq!(NetMode::parse("bridge"), None);
    assert!(valid_memory("512m") && valid_memory("1.5G") && valid_memory("100"));
    assert!(
        !valid_memory("m") && !valid_memory("4gb") && !valid_memory("-1g") && !valid_memory("0")
    );
}
