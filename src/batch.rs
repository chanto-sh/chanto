//! `chanto batch`: one prompt over a list of items, as separate one-shot runs.
//!
//! Each line of `--from` is an item, and `{}` in the prompt is replaced by it.
//! Results append to a JSON Lines file as they finish, so a rerun skips the
//! items that already succeeded. Runs share the working directory unless the
//! arguments after `--` say otherwise.

use anyhow::{Context, Result, bail};
use serde_json::json;
use std::collections::HashSet;
use std::ffi::OsString;
use std::io::Write;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

pub const USAGE: &str = "usage: chanto batch <prompt> --from <file> [--jobs N] [--out <file>] [-- <chanto args>]\n\n\
Runs `chanto -p` once per line of the file, with {} in the prompt replaced by the line. --jobs is\n\
how many at once (default 4). Results go to --out (default <file>.results.jsonl), and a rerun\n\
skips lines that already succeeded. Arguments after -- go to each run, such as --mode acceptEdits;\n\
without one, a run can't make changes that need approval.";

#[derive(Debug, PartialEq)]
struct Options {
    prompt: String,
    from: PathBuf,
    jobs: usize,
    out: PathBuf,
    extra: Vec<String>,
}

fn parse(args: &[String]) -> Result<Options> {
    let (mut prompt, mut from, mut jobs, mut out, mut extra) = (None, None, 4, None, vec![]);
    let mut it = args.iter();
    while let Some(a) = it.next() {
        let mut value = || {
            it.next()
                .with_context(|| format!("{a} needs a value\n{USAGE}"))
        };
        match a.as_str() {
            "--from" => from = Some(PathBuf::from(value()?)),
            "--out" => out = Some(PathBuf::from(value()?)),
            "--jobs" | "-j" => {
                jobs = value()?
                    .parse::<usize>()
                    .ok()
                    .filter(|n| *n > 0)
                    .context("--jobs needs a positive number")?
            }
            "--" => {
                extra = it.by_ref().cloned().collect();
                break;
            }
            s if s.starts_with('-') => bail!("unknown option {s}\n{USAGE}"),
            s if prompt.is_none() => prompt = Some(s.to_string()),
            s => bail!("unexpected argument {s}\n{USAGE}"),
        }
    }
    let prompt = prompt.with_context(|| format!("no prompt\n{USAGE}"))?;
    let from: PathBuf = from.with_context(|| format!("no --from file\n{USAGE}"))?;
    if !prompt.contains("{}") {
        bail!("the prompt needs {{}} where each line goes");
    }
    let out = out.unwrap_or_else(|| {
        let mut name = from.clone().into_os_string();
        name.push(".results.jsonl");
        PathBuf::from(name)
    });
    Ok(Options {
        prompt,
        from,
        jobs,
        out,
        extra,
    })
}

/// The items that succeeded in an earlier run. A partly written last line is
/// ignored, so its item runs again.
fn already_done(results: &str) -> HashSet<String> {
    results
        .lines()
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .filter(|v| v["ok"] == true)
        .filter_map(|v| v["item"].as_str().map(str::to_string))
        .collect()
}

/// Run every item not done yet through `program`, and say how it went.
async fn run(program: OsString, o: Options) -> Result<String> {
    let list = std::fs::read_to_string(&o.from)
        .with_context(|| format!("reading {}", o.from.display()))?;
    let done = already_done(&std::fs::read_to_string(&o.out).unwrap_or_default());
    let mut seen = HashSet::new();
    let items: Vec<String> = list
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .filter(|l| seen.insert(l.to_string()))
        .map(str::to_string)
        .collect();
    let skipped = items.iter().filter(|i| done.contains(*i)).count();
    let todo: Vec<String> = items.into_iter().filter(|i| !done.contains(i)).collect();
    let results = Arc::new(Mutex::new(
        std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&o.out)
            .with_context(|| format!("opening {}", o.out.display()))?,
    ));
    let slots = Arc::new(tokio::sync::Semaphore::new(o.jobs));
    let mut runs = tokio::task::JoinSet::new();
    for item in todo {
        let permit = slots.clone().acquire_owned().await?;
        let (program, results, extra) = (program.clone(), results.clone(), o.extra.clone());
        let prompt = o.prompt.replace("{}", &item);
        runs.spawn(async move {
            let _permit = permit;
            let start = std::time::Instant::now();
            let output = tokio::process::Command::new(&program)
                .arg("-p")
                .args(&extra)
                .arg(&prompt)
                .stdin(std::process::Stdio::null())
                .kill_on_drop(true)
                .output()
                .await;
            let (ok, code, out, err) = match output {
                Ok(o) => (
                    o.status.success(),
                    o.status.code(),
                    String::from_utf8_lossy(&o.stdout).into_owned(),
                    String::from_utf8_lossy(&o.stderr).into_owned(),
                ),
                Err(e) => (false, None, String::new(), e.to_string()),
            };
            let line = json!({
                "item": item,
                "ok": ok,
                "exit": code,
                "seconds": start.elapsed().as_secs(),
                "output": out,
                "error": err,
            });
            eprintln!("{} {item}", if ok { "ok  " } else { "FAIL" });
            let mut file = results.lock().unwrap_or_else(|e| e.into_inner());
            let written = writeln!(file, "{line}").and_then(|()| file.flush());
            ok && written.is_ok()
        });
    }
    let (mut passed, mut failed) = (0, 0);
    while let Some(r) = runs.join_next().await {
        if matches!(r, Ok(true)) {
            passed += 1;
        } else {
            failed += 1;
        }
    }
    let summary = format!(
        "{passed} succeeded, {failed} failed, {skipped} already done. Results: {}",
        o.out.display()
    );
    if failed > 0 {
        bail!("{summary}\nRun it again to retry the failures.");
    }
    Ok(summary)
}

/// `chanto batch ...`.
pub async fn cli(args: &[String]) -> Result<String> {
    if args.iter().any(|a| a == "--help" || a == "-h") && !args.iter().any(|a| a == "--") {
        return Ok(USAGE.to_string());
    }
    let program = std::env::current_exe().context("finding this program")?;
    run(program.into_os_string(), parse(args)?).await
}

#[cfg(test)]
mod tests {
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
        let dir =
            std::env::temp_dir().join(format!("chanto-batch-{}", uuid::Uuid::new_v4().simple()));
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
}
