//! Client side of background runs: `chanto run --bg`, `chanto runs ...`,
//! `chanto attach`, `chanto daemon ...`, and the TUI's `/bg` and `/runs`.
//! Everything goes through the daemon's HTTP API (started on demand).

use super::{Run, fmt_duration};
use crate::agent::{Env, Event, Frontend, PermissionReply, PermissionRequest};
use crate::ui;
use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use std::io::Write;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio_util::sync::CancellationToken;

/// A connection to the daemon described by `~/.chanto/server.json`.
#[derive(Clone)]
pub struct Client {
    pub url: String,
    token: String,
    pub pid: Option<u32>,
    http: reqwest::Client,
}

impl Client {
    pub fn new(url: &str, token: &str) -> Client {
        Client {
            url: url.trim_end_matches('/').into(),
            token: token.into(),
            pid: None,
            http: crate::http::client(),
        }
    }

    fn from_file() -> Option<Client> {
        let v: Value =
            serde_json::from_slice(&std::fs::read(crate::server::server_file()).ok()?).ok()?;
        let mut c = Client::new(v["url"].as_str()?, v["token"].as_str()?);
        c.pid = v["pid"].as_u64().map(|p| p as u32);
        Some(c)
    }

    async fn health(&self) -> Option<Value> {
        let r = self
            .http
            .get(format!("{}/health", self.url))
            .timeout(Duration::from_secs(2))
            .send()
            .await
            .ok()?;
        r.json().await.ok()
    }

    async fn call(
        &self,
        method: reqwest::Method,
        path: &str,
        body: Option<Value>,
    ) -> Result<Value> {
        let mut req = self
            .http
            .request(method, format!("{}{path}", self.url))
            .bearer_auth(&self.token);
        if let Some(b) = body {
            req = req.json(&b);
        }
        let r = req
            .send()
            .await
            .with_context(|| format!("contacting the chanto daemon at {}", self.url))?;
        let status = r.status();
        let v: Value = r.json().await.unwrap_or_default();
        if !status.is_success() {
            match v["error"].as_str() {
                Some(e) => bail!("{e}"),
                None if status == reqwest::StatusCode::NOT_FOUND => {
                    bail!(
                        "the Chanto server at {} doesn't host background runs (an older version?); `chanto daemon stop` and try again",
                        self.url
                    )
                }
                None => bail!("daemon replied {status}"),
            }
        }
        Ok(v)
    }

    pub async fn get(&self, path: &str) -> Result<Value> {
        self.call(reqwest::Method::GET, path, None).await
    }
    pub async fn post(&self, path: &str, body: Value) -> Result<Value> {
        self.call(reqwest::Method::POST, path, Some(body)).await
    }
    pub async fn delete(&self, path: &str) -> Result<Value> {
        self.call(reqwest::Method::DELETE, path, None).await
    }

    pub async fn post_bytes(&self, path: &str, bytes: Vec<u8>) -> Result<Value> {
        let r = self
            .http
            .post(format!("{}{path}", self.url))
            .bearer_auth(&self.token)
            .header("content-type", "application/octet-stream")
            .body(bytes)
            .send()
            .await
            .with_context(|| format!("contacting the Chanto server at {}", self.url))?;
        let status = r.status();
        let v: Value = r.json().await.unwrap_or_default();
        if !status.is_success() {
            bail!(
                "{}",
                v["error"]
                    .as_str()
                    .map(str::to_string)
                    .unwrap_or_else(|| format!("server replied {status}"))
            );
        }
        Ok(v)
    }

    pub async fn get_bytes(&self, path: &str) -> Result<Vec<u8>> {
        let r = self
            .http
            .get(format!("{}{path}", self.url))
            .bearer_auth(&self.token)
            .send()
            .await?;
        let status = r.status();
        if !status.is_success() {
            let v: Value = r.json().await.unwrap_or_default();
            bail!(
                "{}",
                v["error"]
                    .as_str()
                    .map(str::to_string)
                    .unwrap_or_else(|| format!("server replied {status}"))
            );
        }
        Ok(r.bytes().await?.to_vec())
    }

    pub async fn run(&self, id: &str) -> Result<Run> {
        Ok(serde_json::from_value(
            self.get(&format!("/runs/{id}")).await?,
        )?)
    }

    /// Open the run's event stream (`since`: last seq already seen).
    pub async fn events(&self, id: &str, since: u64) -> Result<reqwest::Response> {
        let r = self
            .http
            .get(format!("{}/runs/{id}/events?since={since}", self.url))
            .bearer_auth(&self.token)
            .send()
            .await?;
        if !r.status().is_success() {
            let v: Value = r.json().await.unwrap_or_default();
            bail!(
                "{}",
                v["error"]
                    .as_str()
                    .unwrap_or("could not open the event stream")
            );
        }
        Ok(r)
    }
}

/// Connect to the running daemon, starting one when there is none.
pub async fn connect(start: bool) -> Result<Client> {
    if let Some(c) = Client::from_file()
        && let Some(h) = c.health().await
    {
        if h["runs"] == true {
            return Ok(c);
        }
        bail!(
            "the chanto server at {} (pid {}) doesn't host background runs: it listens beyond this machine. \
                 Restart it with `--runs`, or stop it so a local daemon can start.",
            c.url,
            c.pid.map(|p| p.to_string()).unwrap_or_else(|| "?".into())
        );
    }
    if !start {
        bail!("the chanto daemon is not running");
    }
    start_daemon().await
}

/// Start the daemon (`chanto web --runs`) detached from this terminal.
async fn start_daemon() -> Result<Client> {
    let home = crate::config::home_dir();
    std::fs::create_dir_all(&home)?;
    let mut opts = std::fs::OpenOptions::new();
    opts.create(true).append(true);
    // Owner-only: the server prints its bearer token at startup.
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut opts, 0o600);
    let log = opts.open(home.join("daemon.log"))?;
    let exe = std::env::current_exe()?;
    let mut cmd = std::process::Command::new(exe);
    // `web`: the daemon also serves the web UI (`chanto daemon web`).
    cmd.args([
        "web",
        "--runs",
        "--port",
        "0",
        "--host",
        "127.0.0.1",
        "--no-open",
    ])
    .current_dir(&home)
    .stdin(std::process::Stdio::null())
    .stdout(log.try_clone()?)
    .stderr(log);
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        // Its own session: closing the terminal doesn't hang it up.
        unsafe {
            cmd.pre_exec(|| {
                libc::setsid();
                Ok(())
            });
        }
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const DETACHED_PROCESS: u32 = 0x0000_0008;
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
        cmd.creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP);
    }
    let child = cmd.spawn().context("starting the chanto daemon")?;
    let pid = child.id();
    for _ in 0..150 {
        tokio::time::sleep(Duration::from_millis(100)).await;
        if let Some(c) = Client::from_file().filter(|c| c.pid == Some(pid))
            && c.health().await.is_some()
        {
            return Ok(c);
        }
    }
    bail!(
        "the chanto daemon didn't start; see {}",
        home.join("daemon.log").display()
    )
}

/// A run server saved with `chanto remote add`.
pub fn remote_client(name: &str) -> Result<Client> {
    let (url, token) = crate::auth::remotes()
        .remove(name)
        .ok_or_else(|| anyhow::anyhow!("no run server named {name}; add one with `chanto remote add {name} <url> --token <token>`"))?;
    Ok(Client::new(&url, &token))
}

/// The local daemon, or the named run server.
pub async fn client_for(on: Option<&str>) -> Result<Client> {
    let Some(name) = on else {
        return connect(true).await;
    };
    let c = remote_client(name)?;
    match c.health().await {
        Some(h) if h["runs"] == true => Ok(c),
        Some(_) => bail!(
            "the Chanto server {name} ({}) doesn't host background runs; start it with `chanto serve --runs`",
            c.url
        ),
        None => bail!("can't reach the Chanto server {name} at {}", c.url),
    }
}

/// Split `--on <name>` out of command arguments.
fn take_on(args: &[String]) -> (Option<String>, Vec<String>) {
    let mut on = None;
    let mut rest = Vec::new();
    let mut it = args.iter();
    while let Some(a) = it.next() {
        if a == "--on" {
            on = it.next().cloned();
        } else if let Some(v) = a.strip_prefix("--on=") {
            on = Some(v.to_string());
        } else {
            rest.push(a.clone());
        }
    }
    (on, rest)
}

/// Send the checkout at `cwd` to a run server and start the run there: push
/// the branch to the configured shared git remote when the tree is clean,
/// otherwise upload a git bundle of HEAD plus a snapshot of uncommitted changes.
pub async fn start_remote(
    c: &Client,
    name: &str,
    cwd: &std::path::Path,
    mut body: Value,
) -> Result<Run> {
    let cwd = cwd.to_path_buf();
    let git_remote = crate::config::Config::load(&cwd, crate::config::is_trusted(&cwd))
        .ok()
        .and_then(|(cfg, _)| cfg.remote.get(name).and_then(|r| r.git_remote.clone()));
    let st = {
        let cwd = cwd.clone();
        tokio::task::spawn_blocking(move || super::git::local_state(&cwd)).await??
    };
    if let Some(o) = body.as_object_mut() {
        o.remove("cwd");
    }
    match (&git_remote, &st.snapshot, &st.head_ref) {
        (Some(remote), None, Some(branch)) => {
            let (repo, remote, branch) = (st.repo.clone(), remote.clone(), branch.clone());
            let url = tokio::task::spawn_blocking(move || -> Result<String> {
                let url = super::git::git(&repo, &["remote", "get-url", &remote])?;
                super::git::git(
                    &repo,
                    &["push", "-q", &remote, &format!("HEAD:refs/heads/{branch}")],
                )?;
                Ok(url)
            })
            .await??;
            body["source"] =
                json!({"kind": "git", "url": url, "ref": st.head_ref, "commit": st.head});
        }
        _ => {
            if git_remote.is_some() {
                eprintln!(
                    "{}",
                    ui::dim(
                        "Uncommitted changes (or a detached HEAD): sending a git bundle instead of pushing."
                    )
                );
            }
            let tip = st.snapshot.clone().unwrap_or_else(|| st.head.clone());
            let name = super::new_id();
            let (repo, n) = (st.repo.clone(), name.clone());
            let path =
                tokio::task::spawn_blocking(move || super::git::create_bundle(&repo, &tip, &n))
                    .await??;
            let bytes = tokio::fs::read(&path).await;
            let _ = std::fs::remove_file(&path);
            let v = c.post_bytes("/runs/bundles", bytes?).await?;
            body["source"] = json!({"kind": "bundle", "bundle_id": v["bundle_id"], "ref": format!("refs/chanto/bundle/{name}"),
                "head": st.head, "head_ref": st.head_ref, "snapshot": st.snapshot});
        }
    }
    let mut run: Run = serde_json::from_value(c.post("/runs", body).await?)?;
    run.target = name.to_string();
    Ok(run)
}

/// Bring a run from a run server into the checkout at `cwd`: download its
/// commits as a bundle, then apply them (or push and open a PR) locally.
pub async fn apply_remote(
    c: &Client,
    cwd: &std::path::Path,
    run: &Run,
    pr: bool,
) -> Result<String> {
    // The id names the download path below; the branch and base reach git.
    super::git::check_imported(run)?;
    let bytes = c.get_bytes(&format!("/runs/{}/bundle", run.id)).await?;
    let path = std::env::temp_dir().join(format!(
        "chanto-apply-{}-{}.bundle",
        run.id,
        super::new_id()
    ));
    tokio::fs::write(&path, bytes).await?;
    let (cwd, run) = (cwd.to_path_buf(), run.clone());
    let p = path.clone();
    let out = tokio::task::spawn_blocking(move || -> Result<String> {
        let local = super::git::import_run(&cwd, &run, &p)?;
        if pr {
            super::git::open_pr(&local, "origin")
        } else {
            super::git::apply(&cwd, &local)
        }
    })
    .await?;
    let _ = std::fs::remove_file(&path);
    out
}

/// Options for a new run, as sent to `POST /runs`.
pub fn new_run_body(cwd: &std::path::Path, task: &str, cli: Option<&crate::Cli>) -> Value {
    let mut body = json!({"task": task, "cwd": cwd});
    if let Some(cli) = cli {
        for (k, v) in [
            ("model", &cli.model),
            ("provider", &cli.provider),
            ("base_url", &cli.base_url),
            ("mode", &cli.mode),
        ] {
            if let Some(v) = v {
                body[k] = json!(v);
            }
        }
        if cli.no_verify {
            body["verify"] = json!(false);
        } else if cli.verify {
            body["verify"] = json!(true);
        }
        if let Some(m) = cli.max_cost {
            body["max_cost_usd"] = json!(m);
        }
        if cli.sandbox.as_deref() == Some("danger-full-access") {
            body["sandbox"] = json!(false);
        }
        if cli.dangerously_skip_permissions {
            body["mode"] = json!("bypassPermissions");
        }
    }
    body
}

/// Start a run; returns it.
pub async fn start(body: Value) -> Result<Run> {
    let cwd = std::path::PathBuf::from(body["cwd"].as_str().unwrap_or("."));
    // Refuse non-git directories before starting a daemon for nothing.
    super::git::toplevel(&cwd)?;
    let c = connect(true).await?;
    Ok(serde_json::from_value(c.post("/runs", body).await?)?)
}

fn started_message(run: &Run, on: Option<&str>) -> String {
    let at = on.map(|n| format!(" on {n}")).unwrap_or_default();
    let flag = on.map(|n| format!(" --on {n}")).unwrap_or_default();
    let mut s = format!(
        "Started background run {}{at} on branch {}",
        run.id, run.branch
    );
    if run.source.snapshot.is_some() {
        s.push_str(" (with your uncommitted changes)");
    }
    s.push_str(&format!(
        ".\n  chanto attach {id}{flag}       follow it, answer prompts\n  chanto runs show {id}{flag}    summary and diff\n  chanto runs apply {id}{flag}   bring its commits onto your branch",
        id = run.id
    ));
    s
}

/// `chanto run --bg <task>` (and `chanto --bg <task>`).
pub async fn run_bg(cli: &crate::Cli) -> Result<()> {
    let mut words = cli.prompt.clone();
    if words.first().map(String::as_str) == Some("run") {
        words.remove(0);
    }
    let task = words.join(" ");
    if task.trim().is_empty() {
        bail!("usage: chanto run --bg \"<task>\"");
    }
    let cwd = match &cli.cwd {
        Some(p) => crate::util::canonical(p)?,
        None => std::env::current_dir()?,
    };
    let run = match &cli.on {
        Some(name) => {
            super::git::toplevel(&cwd)?;
            let c = client_for(Some(name)).await?;
            start_remote(&c, name, &cwd, new_run_body(&cwd, &task, Some(cli))).await?
        }
        None => start(new_run_body(&cwd, &task, Some(cli))).await?,
    };
    println!("{}", started_message(&run, cli.on.as_deref()));
    Ok(())
}

fn verify_label(r: &Run) -> String {
    r.verify
        .as_ref()
        .map(|v| v.status.clone())
        .unwrap_or_else(|| {
            if r.settings.verify {
                "-".into()
            } else {
                "off".into()
            }
        })
}

fn cost_label(r: &Run) -> String {
    crate::util::usd(r.cost_usd)
}

pub fn table(runs: &[Run]) -> String {
    if runs.is_empty() {
        return "No background runs. Start one with `chanto run --bg \"<task>\"`.".into();
    }
    let mut s = format!(
        "{:<8}  {:<11}  {:>8}  {:>6}  {:<8}  {:<20}  {}\n",
        "ID", "STATE", "DURATION", "COST", "VERIFY", "BRANCH", "TASK"
    );
    for r in runs {
        s.push_str(&format!(
            "{:<8}  {:<11}  {:>8}  {:>6}  {:<8}  {:<20}  {}\n",
            r.id,
            r.state.label(),
            fmt_duration(r.duration_secs()),
            cost_label(r),
            verify_label(r),
            r.branch,
            {
                let task = format!(
                    "{}{}",
                    if r.kind.as_deref() == Some("goal") {
                        "goal: "
                    } else {
                        ""
                    },
                    r.task
                );
                match &r.job {
                    Some(j) => format!("[{j}] {}", crate::util::one_line(&task, 50)),
                    None => crate::util::one_line(&task, 60),
                }
            }
        ));
    }
    s.trim_end().to_string()
}

pub fn detail_text(v: &Value) -> String {
    let Ok(r) = serde_json::from_value::<Run>(v.clone()) else {
        return v.to_string();
    };
    let mut s = format!(
        "Run {}  {}\nTask:     {}\nRepo:     {}\nBranch:   {}  ({} commit(s) after {})\nModel:    {}\nDuration: {}  Cost: {} (budget ${:.2})\nVerify:   {}",
        r.id,
        r.state.label(),
        r.task,
        r.source.path.display(),
        r.branch,
        v["commits"].as_u64().unwrap_or(0),
        &r.base[..r.base.len().min(10)],
        r.model
            .clone()
            .or(r.settings.model.clone())
            .unwrap_or_else(|| "default".into()),
        fmt_duration(r.duration_secs()),
        cost_label(&r),
        r.settings.max_cost_usd,
        r.verify
            .as_ref()
            .map(|v| if v.detail.is_empty() {
                v.status.clone()
            } else {
                format!("{}: {}", v.status, v.detail)
            })
            .unwrap_or_else(|| verify_label(&r)),
    );
    s.push_str(&format!("\nIsolation: {}", r.isolation_label()));
    if let Some(job) = &r.job {
        s.push_str(&format!(
            "\nJob:      {job} ({})",
            r.trigger.as_deref().unwrap_or("schedule")
        ));
    }
    if r.kind.as_deref() == Some("goal") {
        s.push_str(&format!(
            "\nGoal:     criteria and progress in {}",
            crate::goal::goals_dir().join(&r.id).display()
        ));
    }
    if let Some(team) = &r.team {
        s.push_str(&format!("\nTeam:     {team} (`chanto team show {team}`)"));
    }
    if r.source.snapshot.is_some() {
        s.push_str(
            "\nStarted from your uncommitted changes (snapshot commit at the base of the branch).",
        );
    }
    if let Some(e) = &r.error {
        s.push_str(&format!("\nError:    {e}"));
    }
    if let Some(p) = &r.pending {
        s.push_str(&format!(
            "\n\nWaiting for input: {}\n  answer with `chanto attach {}`",
            pending_summary(p),
            r.id
        ));
    }
    if let Some(stat) = v["diff_stat"].as_str().filter(|s| !s.trim().is_empty()) {
        s.push_str(&format!("\n\nChanges:\n{stat}"));
    }
    if let Some(msgs) = v["last_messages"].as_array().filter(|m| !m.is_empty()) {
        s.push_str("\n\nLast messages:");
        for m in msgs {
            s.push_str(&format!(
                "\n---\n{}",
                crate::util::truncate(m.as_str().unwrap_or(""), 1500)
            ));
        }
    }
    s
}

fn pending_summary(p: &Value) -> String {
    match p["type"].as_str() {
        Some("question") => format!("question: {}", p["question"].as_str().unwrap_or("")),
        _ => format!(
            "{} — {}",
            p["label"].as_str().unwrap_or("permission"),
            crate::util::one_line(p["detail"].as_str().unwrap_or(""), 120)
        ),
    }
}

fn arg_id(args: &[String], usage: &str) -> Result<String> {
    args.iter()
        .find(|a| !a.starts_with('-'))
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("usage: {usage}"))
}

/// `chanto runs [list|show|stop|apply|rm|resume|attach] ...`
pub async fn runs_cmd(args: &[String]) -> Result<()> {
    let (on, args) = take_on(args);
    let on = on.as_deref();
    let sub = args.first().map(String::as_str).unwrap_or("list");
    let rest = if args.is_empty() { &[][..] } else { &args[1..] };
    match sub {
        "list" | "ls" => {
            // Listing reads the saved runs when no daemon is up, rather than
            // starting one just to ask it.
            let runs: Vec<Run> = match on {
                None => match connect(false).await {
                    Ok(c) => serde_json::from_value(c.get("/runs").await?["runs"].clone())
                        .unwrap_or_default(),
                    Err(_) => super::Store::new(super::Store::default_dir()).list(),
                },
                Some(_) => serde_json::from_value(
                    client_for(on).await?.get("/runs").await?["runs"].clone(),
                )
                .unwrap_or_default(),
            };
            println!("{}", table(&runs));
        }
        "show" => {
            let id = arg_id(rest, "chanto runs show <id>")?;
            let v = client_for(on).await?.get(&format!("/runs/{id}")).await?;
            println!("{}", detail_text(&v));
        }
        "stop" => {
            let id = arg_id(rest, "chanto runs stop <id>")?;
            let r: Run = serde_json::from_value(
                client_for(on)
                    .await?
                    .post(&format!("/runs/{id}/stop"), json!({}))
                    .await?,
            )?;
            println!(
                "Stopping run {} (worktree and branch kept; `chanto runs rm {}` removes them).",
                r.id, r.id
            );
        }
        "resume" => {
            let id = arg_id(rest, "chanto runs resume <id>")?;
            let r: Run = serde_json::from_value(
                client_for(on)
                    .await?
                    .post(&format!("/runs/{id}/resume"), json!({}))
                    .await?,
            )?;
            println!("Resumed run {} ({}).", r.id, r.state.label());
        }
        "rm" | "remove" => {
            let id = arg_id(rest, "chanto runs rm <id>")?;
            let c = client_for(on).await?;
            let r = c.run(&id).await?;
            c.delete(&format!("/runs/{}", r.id)).await?;
            println!(
                "Removed run {}: worktree, branch {} and record.",
                r.id, r.branch
            );
        }
        "apply" => {
            let id = arg_id(rest, "chanto runs apply <id> [--pr]")?;
            let pr = rest.iter().any(|a| a == "--pr");
            let c = client_for(on).await?;
            let r = c.run(&id).await?;
            if r.state.is_active() {
                bail!(
                    "run {} is still {}; wait for it to finish or stop it first",
                    r.id,
                    r.state.label()
                );
            }
            let cwd = std::env::current_dir()?;
            if on.is_some() || r.source.kind != "local" {
                println!("{}", apply_remote(&c, &cwd, &r, pr).await?);
                return Ok(());
            }
            let out = tokio::task::spawn_blocking(move || {
                if pr {
                    super::git::open_pr(&r, "origin")
                } else {
                    super::git::apply(&cwd, &r)
                }
            })
            .await??;
            println!("{out}");
        }
        "attach" => {
            let mut a = rest.to_vec();
            if let Some(n) = on {
                a.extend(["--on".to_string(), n.to_string()]);
            }
            attach_cmd(&a).await?
        }
        "help" | "-h" | "--help" => println!("{RUNS_USAGE}"),
        other => bail!("unknown subcommand `{other}`\n{RUNS_USAGE}"),
    }
    Ok(())
}

const RUNS_USAGE: &str = "usage: chanto runs [list]            list background runs
       chanto runs show <id>         summary, diff stat, last messages
       chanto runs stop <id>         stop a run (keeps its worktree)
       chanto runs resume <id>       continue an interrupted run from its saved session
       chanto runs apply <id> [--pr] bring its commits onto the current branch, or push and open a PR
       chanto runs rm <id>           remove its worktree, branch and record
       chanto attach <id>            follow a run live and answer its prompts (Ctrl-C detaches)
All of them take --on <server> for runs on a server added with `chanto remote add`.
Runs on Chanto cloud: `chanto runs --cloud`, `chanto runs show|cancel <id> --cloud`, `chanto attach <id> --cloud`.";

/// `chanto remote add|list|rm`: run servers for `--on <name>`.
pub async fn remote_cmd(args: &[String]) -> Result<()> {
    match args.first().map(String::as_str).unwrap_or("list") {
        "add" => {
            let (mut pos, mut token) = (Vec::new(), std::env::var("CHANTO_REMOTE_TOKEN").ok());
            let mut it = args[1..].iter();
            while let Some(a) = it.next() {
                match a.as_str() {
                    "--token" => token = it.next().cloned(),
                    t if t.starts_with("--token=") => {
                        token = Some(t["--token=".len()..].to_string())
                    }
                    _ => pos.push(a.clone()),
                }
            }
            let [name, url] = pos.as_slice() else {
                bail!("usage: chanto remote add <name> <url> --token <token>")
            };
            let token = token.filter(|t| !t.is_empty()).ok_or_else(|| anyhow::anyhow!("--token is required (the server's token, from its server.json or CHANTO_SERVER_TOKEN)"))?;
            if !(url.starts_with("http://") || url.starts_with("https://")) {
                bail!("the URL must start with http:// or https://");
            }
            crate::auth::set(
                &format!("remote:{name}"),
                crate::auth::Credential::Remote {
                    url: url.trim_end_matches('/').into(),
                    token: token.into(),
                },
            )?;
            let c = remote_client(name)?;
            match c.health().await {
                Some(h) if h["runs"] == true => match c.get("/runs").await {
                    Ok(_) => println!(
                        "Added run server {name} ({url}). Use `chanto run --bg --on {name} \"<task>\"`."
                    ),
                    Err(e) => println!("Added run server {name}, but it refused the token: {e:#}"),
                },
                Some(_) => println!(
                    "Added run server {name}, but it doesn't host runs yet: start it with `chanto serve --runs`."
                ),
                None => println!("Added run server {name}; it isn't reachable right now."),
            }
        }
        "list" | "ls" => {
            let all = crate::auth::remotes();
            if all.is_empty() {
                println!(
                    "No run servers. Add one with `chanto remote add <name> <url> --token <token>`."
                );
            }
            for (name, (url, _)) in all {
                println!("{name:<16} {url}");
            }
        }
        "rm" | "remove" => {
            let name = args
                .get(1)
                .ok_or_else(|| anyhow::anyhow!("usage: chanto remote rm <name>"))?;
            if crate::auth::remove(&format!("remote:{name}"))? {
                println!("Removed run server {name}.");
            } else {
                println!("No run server named {name}.");
            }
        }
        "help" | "-h" | "--help" => println!("{REMOTE_USAGE}"),
        other => bail!("unknown subcommand `{other}`\n{REMOTE_USAGE}"),
    }
    Ok(())
}

const REMOTE_USAGE: &str = "usage: chanto remote add <name> <url> --token <token>   run background tasks there with --on <name>
       chanto remote list                               the servers this machine can use
       chanto remote rm <name>                          forget one";

/// `chanto daemon status|stop|start`
pub async fn daemon_cmd(args: &[String]) -> Result<()> {
    match args.first().map(String::as_str).unwrap_or("status") {
        "help" | "-h" | "--help" => println!("{DAEMON_USAGE}"),
        "status" => {
            let Some(c) = Client::from_file() else {
                println!(
                    "The chanto daemon is not running (it starts on demand with `chanto run --bg`)."
                );
                return Ok(());
            };
            match c.health().await {
                None => println!(
                    "The chanto daemon is not running (stale {}).",
                    crate::server::server_file().display()
                ),
                Some(h) => {
                    let pid = c.pid.map(|p| p.to_string()).unwrap_or_else(|| "?".into());
                    println!(
                        "chanto daemon {} · pid {pid} · {} · `chanto daemon web` opens the web UI",
                        h["version"].as_str().unwrap_or("?"),
                        c.url
                    );
                    if h["runs"] != true {
                        println!(
                            "This server doesn't host background runs (start it with --runs)."
                        );
                    } else if let Ok(v) = c.get("/runs").await {
                        let runs: Vec<Run> =
                            serde_json::from_value(v["runs"].clone()).unwrap_or_default();
                        let count = |f: &dyn Fn(&Run) -> bool| runs.iter().filter(|r| f(r)).count();
                        println!(
                            "runs: {} running, {} needs input, {} queued, {} total",
                            count(&|r| r.state == super::RunState::Running),
                            count(&|r| r.state == super::RunState::NeedsInput),
                            count(&|r| r.state == super::RunState::Queued),
                            runs.len()
                        );
                    }
                    if let Ok(v) = c.get("/runs/jobs").await {
                        let n = v["jobs"].as_array().map_or(0, |j| j.len());
                        if n > 0 {
                            println!("scheduled jobs: {n} (`chanto runner list`)");
                        }
                    }
                    if h["version"].as_str() != Some(env!("CARGO_PKG_VERSION")) {
                        println!(
                            "{}",
                            ui::dim(
                                "It runs a different chanto version; `chanto daemon stop` restarts it with this one on next use."
                            )
                        );
                    }
                }
            }
        }
        "web" => {
            let c = connect(true).await?;
            let url = format!("{}/#token={}", c.url, urlencoding::encode(&c.token));
            crate::mcp_auth::open_browser(&url);
            println!("Opened the web UI ({}).", c.url);
        }
        "start" => {
            let c = connect(true).await?;
            println!(
                "chanto daemon running at {} (pid {}).",
                c.url,
                c.pid.map(|p| p.to_string()).unwrap_or_else(|| "?".into())
            );
        }
        "stop" => {
            let Some(c) = Client::from_file() else {
                println!("The chanto daemon is not running.");
                return Ok(());
            };
            let Some(pid) = c.pid else {
                bail!("server.json has no pid")
            };
            if c.health().await.is_none() {
                let _ = std::fs::remove_file(crate::server::server_file());
                println!("The chanto daemon was not running; removed the stale connection file.");
                return Ok(());
            }
            terminate(pid)?;
            for _ in 0..200 {
                tokio::time::sleep(Duration::from_millis(100)).await;
                if c.health().await.is_none() {
                    println!(
                        "Stopped the chanto daemon (pid {pid}). Active runs were saved as interrupted; `chanto runs resume <id>` continues them."
                    );
                    return Ok(());
                }
            }
            bail!("the chanto daemon (pid {pid}) didn't stop within 20s");
        }
        other => bail!("unknown subcommand `{other}`\n{DAEMON_USAGE}"),
    }
    Ok(())
}

const DAEMON_USAGE: &str =
    "usage: chanto daemon [status]   show whether the daemon runs, and its runs
       chanto daemon start    start it
       chanto daemon stop     stop it (active runs are saved as interrupted)
       chanto daemon web      open the web UI it serves";

fn terminate(pid: u32) -> Result<()> {
    #[cfg(unix)]
    {
        if unsafe { libc::kill(pid as i32, libc::SIGTERM) } != 0 {
            bail!(
                "could not signal pid {pid}: {}",
                std::io::Error::last_os_error()
            );
        }
    }
    #[cfg(windows)]
    {
        let ok = std::process::Command::new("taskkill")
            .args(["/PID", &pid.to_string(), "/F"])
            .status()?
            .success();
        if !ok {
            bail!("taskkill failed for pid {pid}");
        }
    }
    Ok(())
}

/// Parse SSE `data:` payloads out of a byte stream, block by block.
struct SseParser {
    buf: String,
}

impl SseParser {
    fn push(&mut self, chunk: &[u8]) -> Vec<Value> {
        self.buf.push_str(&String::from_utf8_lossy(chunk));
        let mut out = Vec::new();
        while let Some(i) = self.buf.find("\n\n") {
            let block: String = self.buf.drain(..i + 2).collect();
            let data: String = block
                .lines()
                .filter_map(|l| l.strip_prefix("data:"))
                .map(|l| l.strip_prefix(' ').unwrap_or(l))
                .collect::<Vec<_>>()
                .join("\n");
            if let Ok(v) = serde_json::from_str(&data) {
                out.push(v);
            }
        }
        out
    }
}

/// Read a line from the terminal without blocking the runtime.
async fn read_line() -> Option<String> {
    tokio::task::spawn_blocking(|| {
        let mut s = String::new();
        std::io::stdin()
            .read_line(&mut s)
            .ok()
            .filter(|n| *n > 0)
            .map(|_| s.trim().to_string())
    })
    .await
    .ok()
    .flatten()
}

/// Ask the terminal user about a pending request; None leaves it open.
async fn answer_prompt(ev: &Value) -> Option<Value> {
    use std::io::IsTerminal;
    let rid = ev["request_id"].clone();
    if ev["type"] == "question" {
        println!(
            "\n\x1b[33m? {}\x1b[0m",
            ev["question"].as_str().unwrap_or("")
        );
        let opts: Vec<String> = ev["options"]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|o| o.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default();
        for (i, o) in opts.iter().enumerate() {
            println!("  {}. {o}", i + 1);
        }
        if !std::io::stdin().is_terminal() {
            return None;
        }
        print!("Answer (number or text; Enter leaves it for later): ");
        let _ = std::io::stdout().flush();
        let line = read_line().await?;
        if line.is_empty() {
            return None;
        }
        let answer = line
            .parse::<usize>()
            .ok()
            .and_then(|n| opts.get(n.wrapping_sub(1)).cloned())
            .unwrap_or(line);
        return Some(json!({"request_id": rid, "answer": answer}));
    }
    println!(
        "\n\x1b[33m⚠ Permission needed: {}\x1b[0m",
        ev["label"].as_str().unwrap_or("")
    );
    if let Some(d) = ev["detail"].as_str().filter(|d| !d.is_empty()) {
        println!("{}", crate::util::truncate(d, 2000));
    }
    if let Some(r) = ev["reason"].as_str() {
        println!("{}", ui::dim(r));
    }
    if !std::io::stdin().is_terminal() {
        println!(
            "{}",
            ui::dim("(answer from a terminal with `chanto attach`)")
        );
        return None;
    }
    print!("Allow? [y]es / [a]lways / [n]o / Enter leaves it for later: ");
    let _ = std::io::stdout().flush();
    let line = read_line().await?;
    let decision = match line.to_lowercase().as_str() {
        "y" | "yes" => "allow",
        "a" | "always" => "allow_always",
        "n" | "no" => "deny",
        _ => return None,
    };
    let mut body = json!({"request_id": rid, "decision": decision});
    if decision == "deny" {
        print!("Tell it what to do instead (optional): ");
        let _ = std::io::stdout().flush();
        if let Some(f) = read_line().await.filter(|f| !f.is_empty()) {
            body["feedback"] = json!(f);
        }
    }
    Some(body)
}

/// Prints a run event that needs no answer: the model's text, its top-level
/// tool calls, failed tools and messages. `in_text`: the last thing printed
/// was text without a newline. Returns false for the other events (prompts,
/// state changes), which the caller handles.
pub(crate) fn print_event(ev: &Value, in_text: &mut bool) -> bool {
    let ty = ev["type"].as_str().unwrap_or("");
    if *in_text && !matches!(ty, "text" | "assistant_end") {
        println!();
        *in_text = false;
    }
    match ty {
        "text" => {
            print!("{}", ev["text"].as_str().unwrap_or(""));
            let _ = std::io::stdout().flush();
            *in_text = true;
        }
        "assistant_end" => {
            if *in_text {
                println!();
                *in_text = false;
            }
        }
        "tool_start" => {
            if ev["parent"].is_null() {
                println!(
                    "{}",
                    ui::dim(&format!("▸ {}", ev["label"].as_str().unwrap_or("")))
                );
            }
        }
        "tool_end" => {
            if ev["is_error"] == true && ev["parent"].is_null() {
                println!(
                    "{}",
                    ui::dim(&format!(
                        "  ↳ {}",
                        crate::util::one_line(ev["output"].as_str().unwrap_or(""), 160)
                    ))
                );
            }
        }
        "warning" => println!("\x1b[33m{}\x1b[0m", ev["message"].as_str().unwrap_or("")),
        "error" => println!("\x1b[31m{}\x1b[0m", ev["message"].as_str().unwrap_or("")),
        "info" => println!("{}", ev["message"].as_str().unwrap_or("")),
        _ => return false,
    }
    true
}

/// `chanto attach <id>`: stream a run's events; Ctrl-C detaches.
pub async fn attach_cmd(args: &[String]) -> Result<()> {
    use futures::StreamExt;
    if args
        .iter()
        .any(|a| matches!(a.as_str(), "help" | "-h" | "--help"))
    {
        println!(
            "usage: chanto attach <id> [--on <server>]\n\n\
             Follow a background run live and answer its prompts; Ctrl-C detaches and the run keeps going."
        );
        return Ok(());
    }
    let (on, args) = take_on(args);
    let id = arg_id(&args, "chanto attach <id> [--on <server>]")?;
    let c = client_for(on.as_deref()).await?;
    let run = c.run(&id).await?;
    let id = run.id.clone();
    println!(
        "{}",
        ui::dim(&format!(
            "Attached to run {id} ({}) · {} · Ctrl-C detaches, the run keeps going",
            run.state.label(),
            run.task
        ))
    );
    let mut stream = c.events(&id, 0).await?.bytes_stream();
    let mut parser = SseParser { buf: String::new() };
    let mut in_text = false;
    let mut answered: Vec<String> = Vec::new();
    loop {
        let chunk = tokio::select! {
            c = stream.next() => c,
            _ = tokio::signal::ctrl_c() => {
                println!("\n{}", ui::dim(&format!("Detached; run {id} keeps going. `chanto attach {id}` to come back.")));
                return Ok(());
            }
        };
        let Some(Ok(chunk)) = chunk else { break };
        for ev in parser.push(&chunk) {
            if print_event(&ev, &mut in_text) {
                continue;
            }
            match ev["type"].as_str().unwrap_or("") {
                "permission_request" | "question" => {
                    let rid = ev["request_id"].as_str().unwrap_or("").to_string();
                    if answered.contains(&rid) {
                        continue;
                    }
                    // Still open? (Replayed history includes answered ones.)
                    let open = c
                        .run(&id)
                        .await
                        .ok()
                        .and_then(|r| r.pending)
                        .is_some_and(|p| p["request_id"] == ev["request_id"]);
                    if !open {
                        continue;
                    }
                    let reply = tokio::select! {
                        r = answer_prompt(&ev) => r,
                        _ = tokio::signal::ctrl_c() => {
                            println!("\n{}", ui::dim(&format!("Detached; run {id} is waiting for input. `chanto attach {id}` to answer.")));
                            return Ok(());
                        }
                    };
                    if let Some(body) = reply {
                        answered.push(rid);
                        if let Err(e) = c.post(&format!("/runs/{id}/respond"), body).await {
                            ui::warn(&format!("{e:#}"));
                        }
                    } else {
                        println!(
                            "{}",
                            ui::dim(
                                "Left open; the run waits (answer later with `chanto attach`)."
                            )
                        );
                    }
                }
                "run_state" => {
                    let st = ev["state"].as_str().unwrap_or("");
                    if !matches!(st, "queued" | "running" | "needs-input") {
                        let r: Option<Run> = serde_json::from_value(ev["run"].clone()).ok();
                        println!("\n● Run {id} {st}");
                        if let Some(r) = r {
                            if let Some(v) = &r.verify {
                                println!(
                                    "  verify: {}{}",
                                    v.status,
                                    if v.detail.is_empty() {
                                        String::new()
                                    } else {
                                        format!(": {}", v.detail)
                                    }
                                );
                            }
                            if let Some(e) = &r.error {
                                println!("  {e}");
                            }
                            println!(
                                "  cost {} · {} · `chanto runs show {id}` · `chanto runs apply {id}`",
                                crate::util::usd(r.cost_usd),
                                fmt_duration(r.duration_secs())
                            );
                        }
                    } else if st == "needs-input" {
                        // The prompt itself follows (or was replayed).
                    } else {
                        println!("{}", ui::dim(&format!("● {st}")));
                    }
                }
                _ => {}
            }
        }
    }
    Ok(())
}

// ------------------------------------------------------------------ TUI / REPL

/// `/bg <task>` (and `&<task>` in the TUI).
pub async fn slash_bg(env: &Env, task: &str) -> String {
    slash_bg_kind(env, task, None).await
}

/// `/bg <task>`, or `/goal --bg <objective>` (kind "goal").
pub async fn slash_bg_kind(env: &Env, task: &str, kind: Option<&str>) -> String {
    if task.trim().is_empty() {
        return "Usage: `/bg <task>` (or start a prompt with `&`): runs the task in the background in its own git worktree.".into();
    }
    let mut body = new_run_body(&env.root, task.trim(), None);
    body["model"] = json!(env.model());
    if let Some(k) = kind {
        body["kind"] = json!(k);
    }
    match start(body).await {
        Ok(run) => format!(
            "Started background {} `{}` on branch `{}`{}. `/runs` lists runs; `/runs attach {}` follows it here.",
            if kind == Some("goal") { "goal" } else { "run" },
            run.id,
            run.branch,
            if run.source.snapshot.is_some() {
                " (with your uncommitted changes)"
            } else {
                ""
            },
            run.id
        ),
        Err(e) => format!("Couldn't start a background run: {e:#}"),
    }
}

static ATTACHED: Mutex<Option<(String, CancellationToken)>> = Mutex::new(None);

/// `/runs`, `/runs <id>`, `/runs attach <id>`, `/runs detach`, `/runs stop <id>`.
pub async fn slash_runs(env: &Env, arg: &str) -> String {
    let words: Vec<&str> = arg.split_whitespace().collect();
    let res: Result<String> = async {
        match words.as_slice() {
            [] | ["list"] => {
                let v = connect(true).await?.get("/runs").await?;
                let runs: Vec<Run> = serde_json::from_value(v["runs"].clone()).unwrap_or_default();
                Ok(format!("```\n{}\n```\n`/runs <id>` details · `/runs attach <id>` follow here · `/runs stop <id>`", table(&runs)))
            }
            ["detach"] => Ok(match ATTACHED.lock().unwrap().take() {
                Some((id, t)) => {
                    t.cancel();
                    format!("Detached from run `{id}`; it keeps going.")
                }
                None => "Not attached to a run.".into(),
            }),
            ["attach", id] => {
                let c = connect(true).await?;
                let run = c.run(id).await?;
                let token = CancellationToken::new();
                if let Some((_, old)) = ATTACHED.lock().unwrap().replace((run.id.clone(), token.clone())) {
                    old.cancel();
                }
                tokio::spawn(follow(env.frontend.clone(), c, run.id.clone(), token));
                Ok(format!("Following run `{}` ({}); its messages and prompts appear here. `/runs detach` stops following.", run.id, run.state.label()))
            }
            ["stop", id] => {
                let r: Run = serde_json::from_value(connect(true).await?.post(&format!("/runs/{id}/stop"), json!({})).await?)?;
                Ok(format!("Stopping run `{}`.", r.id))
            }
            ["resume", id] => {
                let r: Run = serde_json::from_value(connect(true).await?.post(&format!("/runs/{id}/resume"), json!({})).await?)?;
                Ok(format!("Resumed run `{}`.", r.id))
            }
            [id] | ["show", id] => {
                let v = connect(true).await?.get(&format!("/runs/{id}")).await?;
                Ok(format!("```\n{}\n```", detail_text(&v)))
            }
            _ => Ok("Usage: `/runs [<id> | attach <id> | detach | stop <id> | resume <id>]`".into()),
        }
    }
    .await;
    res.unwrap_or_else(|e| format!("{e:#}"))
}

/// Mirror a run into this session's frontend: messages and tool calls as
/// notes, prompts as the usual permission dialog.
async fn follow(fe: Arc<dyn Frontend>, c: Client, id: String, cancel: CancellationToken) {
    use futures::StreamExt;
    let tag = format!("[run {id}]");
    // Start from the latest message, then follow live.
    let Ok(detail) = c.get(&format!("/runs/{id}")).await else {
        return;
    };
    if let Some(last) = detail["last_messages"]
        .as_array()
        .and_then(|m| m.last())
        .and_then(|m| m.as_str())
    {
        fe.emit(Event::Info(format!(
            "{tag} {}",
            crate::util::truncate(last, 2000)
        )));
    }
    let since = detail["last_seq"].as_u64().unwrap_or(0);
    let Ok(resp) = c.events(&id, since).await else {
        return;
    };
    let mut stream = resp.bytes_stream();
    let mut parser = SseParser { buf: String::new() };
    let mut text = String::new();
    // (The events endpoint replays an open request even when it predates `since`.)
    loop {
        let chunk = tokio::select! {
            c = stream.next() => c,
            _ = cancel.cancelled() => return,
        };
        let Some(Ok(chunk)) = chunk else { break };
        for ev in parser.push(&chunk) {
            match ev["type"].as_str().unwrap_or("") {
                "assistant_start" => text.clear(),
                "text" => text.push_str(ev["text"].as_str().unwrap_or("")),
                "assistant_end" if !text.trim().is_empty() => fe.emit(Event::Info(format!(
                    "{tag} {}",
                    crate::util::truncate(text.trim(), 2000)
                ))),
                "tool_start" if ev["parent"].is_null() => fe.emit(Event::Info(format!(
                    "{tag} ▸ {}",
                    ev["label"].as_str().unwrap_or("")
                ))),
                "warning" => fe.emit(Event::Warn(format!(
                    "{tag} {}",
                    ev["message"].as_str().unwrap_or("")
                ))),
                "permission_request" | "question" => {
                    let open = c
                        .run(&id)
                        .await
                        .ok()
                        .and_then(|r| r.pending)
                        .is_some_and(|p| p["request_id"] == ev["request_id"]);
                    if !open {
                        continue;
                    }
                    let body = if ev["type"] == "question" {
                        let opts: Vec<String> = ev["options"]
                            .as_array()
                            .map(|a| {
                                a.iter()
                                    .filter_map(|o| o.as_str().map(str::to_string))
                                    .collect()
                            })
                            .unwrap_or_default();
                        let q = format!("{tag} {}", ev["question"].as_str().unwrap_or(""));
                        let a = tokio::select! { a = fe.question(&q, &opts) => a, _ = cancel.cancelled() => return };
                        a.map(|a| json!({"request_id": ev["request_id"], "answer": a}))
                    } else {
                        let req = PermissionRequest {
                            tool: ev["tool"].as_str().unwrap_or("").into(),
                            label: format!("{tag} {}", ev["label"].as_str().unwrap_or("")),
                            detail: ev["detail"].as_str().unwrap_or("").into(),
                            always_label: ev["always_label"].as_str().unwrap_or("").into(),
                            reason: ev["reason"].as_str().map(str::to_string),
                            // No proposed content: the run applies its own version.
                            proposed: None,
                            auto_deny: None,
                        };
                        let reply = tokio::select! { r = fe.permission(req) => r, _ = cancel.cancelled() => return };
                        Some(match reply {
                            PermissionReply::Allow | PermissionReply::AllowEdited(_) => {
                                json!({"request_id": ev["request_id"], "decision": "allow"})
                            }
                            PermissionReply::AllowAlways => {
                                json!({"request_id": ev["request_id"], "decision": "allow_always"})
                            }
                            PermissionReply::Deny(f) => {
                                json!({"request_id": ev["request_id"], "decision": "deny", "feedback": f})
                            }
                            PermissionReply::AutoDenied(f) => {
                                json!({"request_id": ev["request_id"], "decision": "deny", "feedback": f})
                            }
                        })
                    };
                    if let Some(b) = body {
                        let _ = c.post(&format!("/runs/{id}/respond"), b).await;
                    }
                }
                "run_state" => {
                    let st = ev["state"].as_str().unwrap_or("");
                    if !matches!(st, "queued" | "running" | "needs-input") {
                        let r: Option<Run> = serde_json::from_value(ev["run"].clone()).ok();
                        let extra = r
                            .map(|r| {
                                let v = r
                                    .verify
                                    .map(|v| format!(" · verify {}", v.status))
                                    .unwrap_or_default();
                                format!(
                                    " · {}{v}{}",
                                    crate::util::usd(r.cost_usd),
                                    r.error.map(|e| format!(" · {e}")).unwrap_or_default()
                                )
                            })
                            .unwrap_or_default();
                        fe.emit(Event::Info(format!("{tag} {st}{extra}. `chanto runs apply {id}` brings its commits onto your branch.")));
                        let mut a = ATTACHED.lock().unwrap();
                        if a.as_ref().is_some_and(|(i, _)| *i == id) {
                            *a = None;
                        }
                        return;
                    }
                }
                _ => {}
            }
        }
    }
}

#[cfg(test)]
mod cmd_tests;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sse_blocks_split_across_chunks() {
        let mut p = SseParser { buf: String::new() };
        assert!(p.push(b"data: {\"type\":\"te").is_empty());
        let v = p.push(b"xt\",\"text\":\"hi\"}\n\n: keep-alive\n\ndata: {\"seq\":2}\n\n");
        assert_eq!(v.len(), 2);
        assert_eq!(v[0]["text"], "hi");
        assert_eq!(v[1]["seq"], 2);
    }
}
