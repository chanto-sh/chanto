use super::*;

#[test]
fn policy_parses_the_documented_values() {
    assert_eq!(Policy::parse("working"), Some(Policy::Working));
    assert_eq!(Policy::parse(" Never "), Some(Policy::Never));
    assert_eq!(Policy::parse("always"), Some(Policy::Always));
    assert_eq!(Policy::parse("off"), Some(Policy::Never));
    assert_eq!(Policy::parse("sometimes"), None);
}

#[cfg(target_os = "macos")]
#[test]
fn the_macos_assertion_holds_until_dropped() {
    let listed = || {
        std::process::Command::new("pmset")
            .args(["-g", "assertions"])
            .output()
            .map(|o| String::from_utf8_lossy(&o.stdout).contains("chanto is working"))
            .unwrap_or(false)
    };
    let i = Inhibitor::start().expect("power assertion");
    assert!(matches!(i, Inhibitor::Assertion(_)));
    assert!(listed(), "pmset lists the assertion while it's held");
    drop(i);
    assert!(!listed(), "released on drop");
}

#[test]
fn a_short_hold_never_starts_the_inhibitor() {
    // Its own state: turns in other tests take holds on the shared one.
    static CELL: Mutex<State> = Mutex::new(State {
        holders: 0,
        inhibitor: None,
        session: false,
        epoch: 0,
        started: 0,
    });
    let take = || acquire(&CELL, &mut lock(&CELL), DELAY);
    let drop_one = || release(&mut lock(&CELL));
    let started = || lock(&CELL).started;
    take();
    drop_one();
    std::thread::sleep(DELAY * 3);
    assert_eq!(started(), 0, "a hold dropped before the delay started one");
    // One that lasts past the delay does, once.
    take();
    take();
    std::thread::sleep(DELAY * 3);
    assert_eq!(started(), 1);
    drop_one();
    drop_one();
    // A later hold starts its own after the delay, not at once.
    take();
    assert_eq!(started(), 1);
    std::thread::sleep(DELAY * 3);
    assert_eq!(started(), 2);
    drop_one();
    // `keep_awake = "always"` starts at once.
    acquire(&CELL, &mut lock(&CELL), Duration::ZERO);
    assert_eq!(started(), 3);
}

#[test]
fn holds_nest_and_the_last_release_lets_go() {
    // Tests never start a real inhibitor; this checks the counting.
    let count = || STATE.lock().unwrap().holders;
    let before = count();
    let a = hold();
    let b = hold();
    assert!(count() >= before + 2);
    drop(a);
    drop(b);
    assert_eq!(count(), before);
}

#[cfg(target_os = "macos")]
#[test]
fn iokit_loads_on_demand_and_takes_a_real_assertion() {
    let api = super::iokit::api().expect("IOKit and CoreFoundation load");
    let kind = api.cfstr("PreventUserIdleSystemSleep");
    let name = api.cfstr("chanto test");
    assert!(!kind.is_null() && !name.is_null());
    let mut id = 0u32;
    // SAFETY: CFStrings just created, released below, and a local out-pointer.
    unsafe {
        assert_eq!((api.create)(kind, 255, name, &mut id), 0);
        assert_ne!(id, 0);
        assert_eq!((api.release)(id), 0);
        (api.cf_release)(kind);
        (api.cf_release)(name);
    }
}
