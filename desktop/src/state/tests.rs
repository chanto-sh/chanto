use super::*;

fn temp(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("hd-{name}-{}", std::process::id()));
    std::fs::create_dir_all(&d).unwrap();
    d
}

#[test]
fn window_state_round_trips_and_is_clamped() {
    let dir = temp("state");
    let file = dir.join("desktop.json");
    assert_eq!(WindowState::load(&file), WindowState::default());
    let s = WindowState {
        width: 900,
        height: 700,
        x: Some(10),
        y: Some(-20),
        maximized: true,
        ..WindowState::default()
    };
    s.save(&file).unwrap();
    assert_eq!(WindowState::load(&file), s);
    std::fs::write(
        &file,
        r#"{"width": 5, "height": 99999, "x": null, "y": null}"#,
    )
    .unwrap();
    let clamped = WindowState::load(&file);
    assert_eq!(
        (clamped.width, clamped.height, clamped.maximized),
        (480, 10_000, false)
    );
    std::fs::write(&file, "garbage").unwrap();
    assert_eq!(WindowState::load(&file), WindowState::default());
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn the_project_and_zoom_persist_and_the_zoom_is_clamped() {
    let dir = temp("state-zoom");
    let file = dir.join("desktop.json");
    let s = WindowState {
        project: Some("/Users/me/api".into()),
        zoom: 1.5,
        ..WindowState::default()
    };
    s.save(&file).unwrap();
    let loaded = WindowState::load(&file);
    assert_eq!(loaded.project.as_deref(), Some("/Users/me/api"));
    assert_eq!(loaded.zoom, 1.5);
    // A zoom left over from another build can't make the page unreadable.
    std::fs::write(
        &file,
        r#"{"width": 900, "height": 700, "x": null, "y": null, "zoom": 9}"#,
    )
    .unwrap();
    assert_eq!(WindowState::load(&file).zoom, 3.0);
    // An older desktop.json has no zoom, which means actual size.
    std::fs::write(
        &file,
        r#"{"width": 900, "height": 700, "x": null, "y": null}"#,
    )
    .unwrap();
    assert_eq!(WindowState::load(&file).zoom, 1.0);
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn long_homes_get_a_short_socket_path() {
    let long = PathBuf::from(format!("/tmp/{}", "x".repeat(120)));
    let a = Instance::new(&long);
    assert!(a.path.as_os_str().len() <= 104 || cfg!(windows));
    assert_eq!(a.path, Instance::new(&long).path);
    assert_ne!(a.path, Instance::new(&long.join("y")).path);
}

#[test]
fn requests_encode_and_decode() {
    for r in [
        Request::Focus,
        Request::Open(PathBuf::from("/Users/me/my project")),
    ] {
        assert_eq!(Request::decode(&r.encode()), Some(r));
    }
    assert_eq!(Request::decode("open\t"), None);
    assert_eq!(Request::decode("rm -rf"), None);
}

#[test]
fn a_second_launch_reaches_the_first() {
    let dir = temp("instance");
    let inst = Instance::new(&dir);
    assert!(!inst.forward(&Request::Focus), "nothing is listening yet");
    let (tx, rx) = std::sync::mpsc::channel();
    inst.listen(move |r| tx.send(r).unwrap()).unwrap();
    #[cfg(unix)]
    assert_eq!(
        std::os::unix::fs::PermissionsExt::mode(
            &std::fs::metadata(&inst.path).unwrap().permissions()
        ) & 0o777,
        0o600
    );
    assert!(Instance::new(&dir).forward(&Request::Open(PathBuf::from("/tmp/p"))));
    assert_eq!(
        rx.recv_timeout(std::time::Duration::from_secs(5)).unwrap(),
        Request::Open(PathBuf::from("/tmp/p"))
    );
    inst.cleanup();
    std::fs::remove_dir_all(dir).unwrap();
}
