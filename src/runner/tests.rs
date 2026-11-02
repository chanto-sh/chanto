use super::*;

fn at(s: &str) -> DateTime<Local> {
    NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M:%S")
        .unwrap()
        .and_local_timezone(Local)
        .unwrap()
}

fn fmt(t: Option<DateTime<Local>>) -> String {
    t.unwrap().format("%Y-%m-%d %H:%M").to_string()
}

#[test]
fn intervals() {
    assert_eq!(parse_schedule("90s").unwrap(), Schedule::Every(90));
    assert_eq!(parse_schedule("10m").unwrap(), Schedule::Every(600));
    assert_eq!(parse_schedule("2h").unwrap(), Schedule::Every(7200));
    assert_eq!(parse_schedule("1d").unwrap(), Schedule::Every(86400));
    assert!(parse_schedule("30s").is_err(), "under a minute");
    assert!(parse_schedule("0m").is_err());
    // A multibyte unit or a huge count is an error, not a panic.
    assert!(parse_schedule("5分").is_err());
    assert!(parse_schedule("é").is_err());
    assert!(parse_schedule("18446744073709551615d").is_err());
    assert!(parse_schedule("5x").is_err());
    assert!(parse_schedule("").is_err());
}

#[test]
fn cron_fields() {
    let c = Cron::parse("*/15 9-17 * * 1-5").unwrap();
    assert_eq!(c.minutes, vec![0, 15, 30, 45]);
    assert_eq!(c.hours, (9..=17).collect::<Vec<_>>());
    assert_eq!(c.dows, vec![1, 2, 3, 4, 5]);
    let c = Cron::parse("5/20 0,12 1 */3 *").unwrap();
    assert_eq!(c.minutes, vec![5, 25, 45]);
    assert_eq!(c.hours, vec![0, 12]);
    assert_eq!(c.months, vec![1, 4, 7, 10]);
    assert_eq!(
        Cron::parse("0 9 * * 5-7").unwrap().dows,
        vec![0, 5, 6],
        "7 is Sunday"
    );
    assert!(Cron::parse("0 0 * *").is_err());
    assert!(Cron::parse("60 0 * * *").is_err());
    assert!(Cron::parse("0 24 * * *").is_err());
    assert!(Cron::parse("*/0 0 * * *").is_err());
}

#[test]
fn cron_next_times() {
    // 2026-10-11 is a Sunday.
    assert_eq!(
        fmt(Cron::parse("0 9 * * 0")
            .unwrap()
            .next_after(at("2026-10-11 00:00:00"))),
        "2026-10-11 09:00"
    );
    assert_eq!(
        fmt(Cron::parse("0 9 * * 7")
            .unwrap()
            .next_after(at("2026-10-11 00:00:00"))),
        "2026-10-11 09:00"
    );
    let c = Cron::parse("30 3 * * *").unwrap();
    assert_eq!(
        fmt(c.next_after(at("2026-06-01 08:00:00"))),
        "2026-06-02 03:30"
    );
    assert_eq!(
        fmt(c.next_after(at("2026-06-01 03:00:00"))),
        "2026-06-01 03:30"
    );
    assert_eq!(
        fmt(c.next_after(at("2026-06-01 03:30:00"))),
        "2026-06-02 03:30",
        "strictly after"
    );
    assert_eq!(
        fmt(Cron::parse("0 0 1 * *")
            .unwrap()
            .next_after(at("2026-06-15 12:00:00"))),
        "2026-07-01 00:00"
    );
    assert_eq!(
        fmt(Cron::parse("0 0 1 1 *")
            .unwrap()
            .next_after(at("2026-06-15 12:00:00"))),
        "2027-01-01 00:00"
    );
    assert_eq!(
        fmt(Cron::parse("0 0 29 2 *")
            .unwrap()
            .next_after(at("2026-03-01 00:00:00"))),
        "2028-02-29 00:00"
    );
    // Vixie cron: "the 13th OR a Friday". 2026-10-13 is a Tuesday.
    let c = Cron::parse("0 0 13 * 5").unwrap();
    assert_eq!(
        fmt(c.next_after(at("2026-10-12 00:00:00"))),
        "2026-10-13 00:00"
    );
    assert_eq!(
        fmt(c.next_after(at("2026-10-13 01:00:00"))),
        "2026-10-16 00:00"
    );
    assert!(
        Cron::parse("0 0 30 2 *")
            .unwrap()
            .next_after(at("2026-01-01 00:00:00"))
            .is_none(),
        "Feb 30 never comes"
    );
}

#[test]
fn due_times() {
    let now = at("2026-06-01 12:00:00");
    let every = Schedule::Every(600);
    assert!(
        is_due(&every, None, now),
        "an interval job that never ran is due"
    );
    assert!(!is_due(
        &every,
        Some(now - ChronoDuration::seconds(100)),
        now
    ));
    assert!(is_due(
        &every,
        Some(now - ChronoDuration::seconds(601)),
        now
    ));
    let cron = Schedule::Cron(Cron::parse("0 3 * * *").unwrap());
    assert!(
        !is_due(&cron, None, now),
        "a new cron job waits for its slot"
    );
    assert!(!is_due(&cron, Some(at("2026-06-01 03:00:00")), now));
    assert!(
        is_due(&cron, Some(at("2026-05-30 03:00:00")), now),
        "a missed slot fires once"
    );
    assert!(
        is_due(&cron, Some(at("2026-05-31 20:00:00")), now),
        "first seen before last night's slot"
    );
}

#[test]
fn hmac_matches_rfc_4231() {
    assert_eq!(
        hex(&hmac_sha256(b"Jefe", b"what do ya want for nothing?")),
        "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843"
    );
    // A key longer than the block size is hashed first (test case 6).
    assert_eq!(
        hex(&hmac_sha256(
            &[0xaa; 131],
            b"Test Using Larger Than Block-Size Key - Hash Key First"
        )),
        "60e431591ee0b67f0d8a26aacbf5b77f8e0bc6213728c5140546040f0ee37f54"
    );
}

#[test]
fn webhook_tokens_in_any_of_their_forms() {
    let t = "hjt_secret";
    let body = br#"{"action":"opened"}"#;
    let h = |pairs: &[(&'static str, String)]| {
        let mut m = HeaderMap::new();
        for (k, v) in pairs {
            m.insert(*k, v.parse().unwrap());
        }
        m
    };
    let none = HashMap::new();
    assert!(carries_token(
        &h(&[("authorization", format!("Bearer {t}"))]),
        &none,
        body,
        t
    ));
    assert!(carries_token(
        &h(&[("x-chanto-token", t.into())]),
        &none,
        body,
        t
    ));
    assert!(carries_token(
        &HeaderMap::new(),
        &HashMap::from([("token".to_string(), t.to_string())]),
        body,
        t
    ));
    let sig = format!("sha256={}", hex(&hmac_sha256(t.as_bytes(), body)));
    assert!(carries_token(
        &h(&[("x-hub-signature-256", sig.clone())]),
        &none,
        body,
        t
    ));
    assert!(
        !carries_token(&h(&[("x-hub-signature-256", sig)]), &none, b"tampered", t),
        "the signature covers the body"
    );
    assert!(!carries_token(
        &h(&[("authorization", "Bearer nope".into())]),
        &none,
        body,
        t
    ));
    assert!(!carries_token(&HeaderMap::new(), &none, body, t));
}

#[test]
fn job_files_parse_strictly() {
    let dir = std::env::temp_dir().join(format!("chanto-jobs-{}", uuid::Uuid::new_v4().simple()));
    std::fs::create_dir_all(&dir).unwrap();
    let f = dir.join("runner.toml");
    let w = |s: &str| std::fs::write(&f, s).unwrap();
    w(
        "[[job]]\nname = \"nightly\"\nschedule = \"0 3 * * *\"\nprompt = \"Review\"\nverify = true\nmax_minutes = 20\n\n[[job]]\nname = \"triage\"\nprompt = \"Triage\"\ncwd = \"sub\"\nenabled = false\n",
    );
    let jobs = read_jobs(&f, Some(&dir)).unwrap();
    assert_eq!(jobs.len(), 2);
    assert_eq!(
        (
            jobs[0].repo.clone(),
            jobs[0].verify,
            jobs[0].max_minutes,
            jobs[0].enabled
        ),
        (dir.clone(), Some(true), Some(20), true)
    );
    assert_eq!(
        (jobs[1].repo.clone(), jobs[1].enabled),
        (dir.join("sub"), false)
    );
    // Global jobs must say where to run.
    assert!(format!("{:#}", read_jobs(&f, None).unwrap_err()).contains("needs `cwd`"));
    w("[[job]]\nname = \"x\"\nprompt = \"p\"\ntimeout = 5\n");
    assert!(
        format!("{:#}", read_jobs(&f, Some(&dir)).unwrap_err()).contains("timeout"),
        "unknown keys are errors, not ignored"
    );
    w("[[job]]\nname = \"a b\"\nprompt = \"p\"\n");
    assert!(read_jobs(&f, Some(&dir)).is_err());
    w("[[job]]\nname = \"x\"\nprompt = \"p\"\nschedule = \"every day\"\n");
    assert!(read_jobs(&f, Some(&dir)).is_err());
    w("[[job]]\nname = \"x\"\nprompt = \"p\"\nmode = \"reckless\"\n");
    assert!(read_jobs(&f, Some(&dir)).is_err());
    // A job can run its prompt as a goal; other kinds are refused.
    w("[[job]]\nname = \"g\"\nprompt = \"p\"\nkind = \"goal\"\n");
    assert_eq!(
        read_jobs(&f, Some(&dir)).unwrap()[0].kind.as_deref(),
        Some("goal")
    );
    w("[[job]]\nname = \"g\"\nprompt = \"p\"\nkind = \"epic\"\n");
    assert!(
        format!("{:#}", read_jobs(&f, Some(&dir)).unwrap_err()).contains("unknown kind `epic`")
    );
    assert!(
        read_jobs(&dir.join("missing.toml"), None)
            .unwrap()
            .is_empty()
    );
    let _ = std::fs::remove_dir_all(&dir);
}
