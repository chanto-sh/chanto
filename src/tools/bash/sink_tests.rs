use super::*;

fn shell() -> Arc<BgShell> {
    Arc::new(BgShell {
        id: "bash_1".into(),
        command: "true".into(),
        cwd: "/".into(),
        description: None,
        monitor: None,
        output: Mutex::new(String::new()),
        read_pos: Mutex::new(0),
        watch_pos: Mutex::new(0),
        exit: Mutex::new(None),
        pid: None,
        network: Mutex::new(None),
    })
}

/// A foreground command's output goes to the returned buffer; once it is
/// handed to the background, later chunks must land in the shell instead,
/// so nothing is duplicated or lost across the switch.
#[test]
fn chunks_follow_the_sink_across_a_hand_off() {
    let buf = Mutex::new(Vec::new());
    let sink = Mutex::new(Sink::Fg);
    push_sink(&sink, &buf, b"one\n");
    assert_eq!(String::from_utf8_lossy(&buf.lock().unwrap()), "one\n");
    let sh = shell();
    *sink.lock().unwrap() = Sink::Bg(sh.clone());
    push_sink(&sink, &buf, b"two\n");
    assert_eq!(
        String::from_utf8_lossy(&buf.lock().unwrap()),
        "one\n",
        "the foreground buffer stops growing"
    );
    assert_eq!(
        *sh.output.lock().unwrap(),
        "two\n",
        "later output lands in the shell"
    );
}
