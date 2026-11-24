use super::*;

fn iso() -> Isolation {
    Isolation {
        engine: "docker".into(),
        image: "rust:1-bookworm".into(),
        network: NetMode::None,
        cpus: Some(2.0),
        memory: Some("4g".into()),
    }
}

#[test]
fn config_parsing_and_validation() {
    let dir = std::env::temp_dir().join(format!("chanto-iso-{}", uuid::Uuid::new_v4().simple()));
    std::fs::create_dir_all(&dir).unwrap();
    let mut b = super::super::BackgroundConfig::default();
    assert_eq!(Isolation::from_config(&b, &dir).unwrap(), None);
    b.isolate = Some("none".into());
    assert_eq!(Isolation::from_config(&b, &dir).unwrap(), None);
    b.isolate = Some("lxc".into());
    assert!(
        Isolation::from_config(&b, &dir)
            .unwrap_err()
            .to_string()
            .contains("not supported")
    );
    b.isolate = Some("docker".into());
    let i = Isolation::from_config(&b, &dir).unwrap().unwrap();
    assert_eq!(
        (i.image.as_str(), i.network),
        ("buildpack-deps:bookworm", NetMode::None)
    );
    std::fs::write(dir.join("package.json"), "{}").unwrap();
    assert_eq!(
        Isolation::from_config(&b, &dir).unwrap().unwrap().image,
        "node:22-bookworm"
    );
    b.image = Some("alpine:3".into());
    b.network = Some("allowlist".into());
    b.memory = Some("512m".into());
    b.cpus = Some(1.5);
    let i = Isolation::from_config(&b, &dir).unwrap().unwrap();
    assert_eq!(
        i.label(),
        "docker · alpine:3 · network allowlist · 1.5 cpus · 512m memory"
    );
    b.network = Some("open".into());
    assert!(Isolation::from_config(&b, &dir).is_err());
    b.network = None;
    for bad in ["lots", "4gb", "-1g", ""] {
        b.memory = Some(bad.into());
        assert!(Isolation::from_config(&b, &dir).is_err(), "{bad}");
    }
    b.memory = None;
    b.cpus = Some(0.0);
    assert!(Isolation::from_config(&b, &dir).is_err());
    b.cpus = None;
    b.image = Some("--privileged".into());
    assert!(Isolation::from_config(&b, &dir).is_err());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn default_images_by_project_type() {
    let dir = std::env::temp_dir().join(format!("chanto-img-{}", uuid::Uuid::new_v4().simple()));
    std::fs::create_dir_all(&dir).unwrap();
    for (file, image) in [
        ("Gemfile", "ruby:3-bookworm"),
        ("pyproject.toml", "python:3.12-bookworm"),
        ("package.json", "node:22-bookworm"),
        ("go.mod", "golang:1-bookworm"),
        ("Cargo.toml", "rust:1-bookworm"),
    ] {
        std::fs::write(dir.join(file), "").unwrap();
        assert_eq!(default_image(&dir), image, "{file}");
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn run_command_has_limits_mounts_and_labels() {
    let spec = Spec {
        run_id: "ab12cd34".into(),
        store: "0011aabbccdd".into(),
        iso: iso(),
        mounts: vec![
            Mount {
                path: "/w/t".into(),
                read_only: false,
            },
            Mount {
                path: "/repo/.git".into(),
                read_only: true,
            },
        ],
        workdir: "/w/t".into(),
        network: Some("none".into()),
        user: None,
    };
    let a = run_args(&spec).join(" ");
    assert_eq!(
        a,
        "run -d --name chanto-run-ab12cd34 --label chanto.run=ab12cd34 --label chanto.store=0011aabbccdd --network none --cpus 2 --memory 4g \
             --pids-limit 4096 --security-opt no-new-privileges -v /w/t:/w/t -v /repo/.git:/repo/.git:ro -w /w/t --entrypoint tail rust:1-bookworm -f /dev/null"
    );
    // Full networking is the engine's default bridge; Linux maps the user.
    let a = run_args(&Spec {
        network: None,
        user: Some("1000:1000".into()),
        iso: Isolation {
            cpus: None,
            memory: None,
            ..iso()
        },
        ..spec
    });
    assert!(
        !a.contains(&"--network".to_string())
            && !a.contains(&"--cpus".to_string())
            && !a.contains(&"--memory".to_string())
    );
    assert!(a.windows(2).any(|w| w == ["--user", "1000:1000"]));
}

#[test]
fn exec_command_carries_only_the_given_environment() {
    let env = vec![
        ("NO_COLOR".to_string(), "1".to_string()),
        (
            "HTTP_PROXY".to_string(),
            "http://k:@chanto-proxy:3128".to_string(),
        ),
    ];
    let a = exec_args(
        "chanto-run-x",
        Path::new("/w/t/src"),
        &env,
        &[],
        "tok",
        "echo hi > f && cat f",
    );
    assert_eq!(&a[..5], ["exec", "-w", "/w/t/src", "-e", "CHANTO_EXEC=tok"]);
    assert!(
        a.windows(2)
            .any(|w| w == ["-e", "HTTP_PROXY=http://k:@chanto-proxy:3128"])
    );
    assert_eq!(
        a[a.len() - 6..a.len() - 2],
        ["chanto-run-x", "sh", "-c", EXEC_WRAP]
    );
    assert_eq!(
        a.last().unwrap(),
        "echo hi > f && cat f",
        "the script is one argument, never re-split by the host shell"
    );
    // Nothing from the host environment rides along.
    // SAFETY: test-only; the variable name is unique to this test.
    unsafe { std::env::set_var("CHANTO_TEST_PROVIDER_KEY", "sk-secret") };
    let c = Container {
        engine: "docker".into(),
        name: "chanto-run-x".into(),
        worktree: "/w/t".into(),
        scratch: "/s".into(),
        network: NetMode::None,
        proxy: None,
        env: vec![],
        auto_allow: true,
    };
    let (cmd, token) = c.command("env", Path::new("/elsewhere"), &[]);
    let args: Vec<String> = cmd
        .as_std()
        .get_args()
        .map(|s| s.to_string_lossy().to_string())
        .collect();
    assert!(!args.iter().any(|a| a.contains("sk-secret")));
    assert!(
        cmd.as_std()
            .get_envs()
            .all(|(k, _)| k != "CHANTO_TEST_PROVIDER_KEY")
    );
    assert!(
        args.windows(2).any(|w| w == ["-w", "/w/t"]),
        "outside the worktree falls back to its root: {args:?}"
    );
    assert!(args.contains(&format!("CHANTO_EXEC={token}")));
    unsafe { std::env::remove_var("CHANTO_TEST_PROVIDER_KEY") };
}

#[test]
fn relay_and_kill_commands() {
    let a = relay_args("r1", "s", "host.docker.internal", 40123, false).join(" ");
    assert!(
        a.starts_with(
            "run -d --name chanto-run-r1-proxy --label chanto.run=r1 --label chanto.store=s"
        ),
        "{a}"
    );
    assert!(
        a.ends_with("alpine:latest nc -lk -p 3128 -e nc host.docker.internal 40123"),
        "{a}"
    );
    assert!(!a.contains("--add-host"));
    assert!(
        relay_args("r1", "s", "host.docker.internal", 1, true)
            .join(" ")
            .contains("--add-host host.docker.internal:host-gateway")
    );
    let k = kill_args("chanto-run-r1", "abc");
    assert_eq!(&k[..4], ["exec", "chanto-run-r1", "sh", "-c"]);
    assert!(k[4].contains("grep -qx 'CHANTO_EXEC=abc'"));
}

#[test]
fn engine_errors_say_what_to_do() {
    let e = engine_error("docker", false, "");
    assert!(
        e.contains("isn't installed") && e.contains("isolate = \"none\""),
        "{e}"
    );
    let e = engine_error(
        "docker",
        true,
        "\nCannot connect to the Docker daemon at unix:///var/run/docker.sock. Is the docker daemon running?\n",
    );
    assert!(
        e.contains("isn't running (Cannot connect to the Docker daemon")
            && e.contains("start Docker Desktop"),
        "{e}"
    );
    assert!(engine_error("podman", true, "").contains("podman machine start"));
    assert!(
        check_engine("chanto-no-such-engine")
            .unwrap_err()
            .to_string()
            .contains("isn't installed")
    );
}

#[test]
fn registry_routes_by_worktree() {
    let dir = std::env::temp_dir().join(format!("chanto-reg-{}", uuid::Uuid::new_v4().simple()));
    std::fs::create_dir_all(dir.join("src")).unwrap();
    let c = Arc::new(Container {
        engine: "docker".into(),
        name: format!("reg-{}", uuid::Uuid::new_v4().simple()),
        worktree: dir.clone(),
        scratch: dir.clone(),
        network: NetMode::None,
        proxy: None,
        env: vec![],
        auto_allow: false,
    });
    assert!(for_path(&dir.join("src")).is_none_or(|x| x.name != c.name));
    register(c.clone());
    assert_eq!(for_path(&dir.join("src")).unwrap().name, c.name);
    assert!(for_path(&std::env::temp_dir()).is_none_or(|x| x.name != c.name));
    unregister(&c.name);
    assert!(for_path(&dir).is_none_or(|x| x.name != c.name));
    let _ = std::fs::remove_dir_all(&dir);
}
