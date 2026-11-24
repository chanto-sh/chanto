use super::*;

fn args(s: &[&str]) -> Vec<String> {
    s.iter().map(|a| a.to_string()).collect()
}

#[test]
fn parses_options_and_defaults() {
    let o = parse(&args(&[
        "fix {}", "--from", "l.txt", "-j", "2", "--", "--mode", "auto",
    ]))
    .unwrap();
    assert_eq!(o.jobs, 2);
    assert_eq!(o.out, PathBuf::from("l.txt.results.jsonl"));
    assert_eq!(o.extra, args(&["--mode", "auto"]));
    assert!(parse(&args(&["no slot", "--from", "l"])).is_err());
    assert!(parse(&args(&["{}"])).is_err());
    assert!(parse(&args(&["{}", "--from", "l", "--jobs", "0"])).is_err());
    assert!(parse(&args(&["{}", "--from", "l", "--nope"])).is_err());
}

#[test]
fn only_finished_successes_count_as_done() {
    let done = already_done(
        "{\"item\":\"a\",\"ok\":true}\n{\"item\":\"b\",\"ok\":false}\n{\"item\":\"c\",\"ok\":tr",
    );
    assert_eq!(done, HashSet::from(["a".to_string()]));
}

/// A `chanto` that fails for one item until `fixed` exists, and logs each prompt.
#[cfg(unix)]
#[tokio::test]
async fn runs_each_item_and_a_rerun_retries_only_failures() {
    use std::os::unix::fs::PermissionsExt;
    let dir = std::env::temp_dir().join(format!("chanto-batch-{}", uuid::Uuid::new_v4().simple()));
    std::fs::create_dir_all(&dir).unwrap();
    let fake = dir.join("fake");
    let (log, fixed) = (dir.join("log"), dir.join("fixed"));
    std::fs::write(
        &fake,
        format!(
            "#!/bin/sh\nfor a; do p=$a; done\necho \"$p\" >> '{}'\n\
                 case \"$p\" in *bad*) [ -e '{}' ] || exit 3 ;; esac\necho \"did $p\"\n",
            log.display(),
            fixed.display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
    let from = dir.join("list");
    std::fs::write(&from, "a\n\nbad\nc\na\n").unwrap();
    let options = || {
        parse(&args(&[
            "do {}",
            "--from",
            from.to_str().unwrap(),
            "-j",
            "2",
        ]))
        .unwrap()
    };

    let e = run(fake.clone().into_os_string(), options())
        .await
        .unwrap_err()
        .to_string();
    assert!(e.contains("2 succeeded, 1 failed, 0 already done"), "{e}");
    let mut prompts: Vec<String> = std::fs::read_to_string(&log)
        .unwrap()
        .lines()
        .map(str::to_string)
        .collect();
    prompts.sort();
    assert_eq!(
        prompts,
        ["do a", "do bad", "do c"],
        "a repeated line runs once"
    );

    std::fs::write(&fixed, "").unwrap();
    std::fs::remove_file(&log).unwrap();
    let s = run(fake.into_os_string(), options()).await.unwrap();
    assert!(s.contains("1 succeeded, 0 failed, 2 already done"), "{s}");
    assert_eq!(std::fs::read_to_string(&log).unwrap(), "do bad\n");
    let results = std::fs::read_to_string(from.with_file_name("list.results.jsonl")).unwrap();
    assert!(results.contains("did do a"), "{results}");
    let _ = std::fs::remove_dir_all(&dir);
}
