//! `chanto agents …` and `chanto cloud run --agent`: an org's agents
//! (`chanto_proto::agents`) and the runs they start.

use crate::runs::cloud::{Cloud, resolve_org};
use anyhow::{Context, Result, bail};
use chanto_proto::agents::{Agent, AgentList, AgentSpec, AgentTrigger, RunAgent, valid_agent_name};
use chanto_proto::runs::{Placement, Run, RunList, RunStatus};
use clap::Parser;
use serde_json::Value;
use std::path::PathBuf;
use std::time::Duration;

/// How often `--wait` checks on a run.
const POLL: Duration = if cfg!(test) {
    Duration::from_millis(5)
} else {
    Duration::from_secs(2)
};

/// A path segment, escaped.
pub(crate) fn seg(s: &str) -> String {
    urlencoding::encode(s).into_owned()
}

#[derive(Parser, Debug)]
#[command(
    name = "chanto agents",
    about = "Agents in Chanto cloud: standing jobs that start runs by hand, on a schedule or on repository events"
)]
struct AgentsCli {
    #[command(subcommand)]
    cmd: Option<Cmd>,
    /// The org (default: CHANTO_CLOUD_ORG, else `chanto cloud org`, else your personal org)
    #[arg(long, global = true)]
    org: Option<String>,
    /// Print JSON
    #[arg(long, global = true)]
    json: bool,
}

#[derive(clap::Subcommand, Debug)]
enum Cmd {
    /// List the org's agents
    #[command(alias = "ls")]
    List,
    /// An agent's settings, instructions and last run
    Show { name: String },
    /// Create an agent from flags and/or --file
    Create {
        /// Its name: lowercase letters, digits and dashes (or `name` in --file)
        name: Option<String>,
        #[command(flatten)]
        spec: SpecArgs,
    },
    /// Change an agent; only the given settings change
    Edit {
        name: String,
        /// A new name
        #[arg(long)]
        rename: Option<String>,
        #[command(flatten)]
        spec: SpecArgs,
    },
    /// Delete an agent (its runs are kept)
    #[command(alias = "rm")]
    Delete {
        name: String,
        /// Don't ask
        #[arg(short, long)]
        yes: bool,
    },
    /// Start a run of the agent now
    Run {
        name: String,
        #[command(flatten)]
        run: RunArgs,
    },
    /// The agent's runs, newest first
    Runs {
        name: String,
        #[arg(long, default_value_t = 20)]
        limit: u32,
    },
}

/// An agent's settings as flags. Lists (`--on`, `--label`) replace the
/// agent's when given; `none` empties them.
#[derive(clap::Args, Debug, Default, Clone)]
pub(super) struct SpecArgs {
    /// Start from an agent file (TOML or JSON, with the fields `chanto agents show --json` prints); flags override it
    #[arg(long, value_name = "FILE")]
    file: Option<PathBuf>,
    /// Git URL its runs clone
    #[arg(long)]
    repo: Option<String>,
    /// The standing task, given to every run
    #[arg(long)]
    instructions: Option<String>,
    /// Read the instructions from a file
    #[arg(long, value_name = "FILE", conflicts_with = "instructions")]
    instructions_file: Option<PathBuf>,
    #[arg(long)]
    description: Option<String>,
    /// Five-field cron in UTC ("0 9 * * 1-5": weekdays at 09:00); `none` removes it
    #[arg(long, value_name = "CRON")]
    schedule: Option<String>,
    /// What starts it: issue_opened, issue_labeled[:LABEL], pull_request_opened,
    /// pull_request_updated or mention (repeatable; `none` for none)
    #[arg(long = "on", value_name = "EVENT")]
    on: Vec<String>,
    /// Where its runs execute: auto, hosted or self-hosted
    #[arg(long)]
    placement: Option<String>,
    /// A label runners need to take its runs (repeatable; `none` for none)
    #[arg(long = "label", value_name = "LABEL")]
    labels: Vec<String>,
    /// Branch its runs start from (default: the repository's default branch; `none` resets)
    #[arg(long)]
    branch: Option<String>,
    /// Only start by hand: no schedule or event starts
    #[arg(long)]
    disable: bool,
    /// Undo --disable
    #[arg(long, conflicts_with = "disable")]
    enable: bool,
}

#[derive(clap::Args, Debug, Default, Clone)]
pub(super) struct RunArgs {
    /// Added to the agent's instructions for this run ("look at issue #42")
    #[arg(long)]
    input: Option<String>,
    /// Branch for this run instead of the agent's
    #[arg(long = "ref", value_name = "BRANCH")]
    git_ref: Option<String>,
    /// Follow the run until it finishes; exits non-zero unless it succeeded
    #[arg(long)]
    wait: bool,
}

/// `none` (or empty) clears a setting.
fn cleared(s: &str) -> bool {
    s.is_empty() || s == "none"
}

pub(super) fn parse_trigger(s: &str) -> Result<AgentTrigger> {
    let (event, label) = match s.split_once(':') {
        Some((e, l)) => (e, Some(l.trim().to_string()).filter(|l| !l.is_empty())),
        None => (s, None),
    };
    let t = match event.trim().replace('-', "_").as_str() {
        "issue_labeled" => AgentTrigger::IssueLabeled { label },
        _ if label.is_some() => bail!("only issue_labeled takes a label (`issue_labeled:bug`)"),
        "issue_opened" => AgentTrigger::IssueOpened,
        "pull_request_opened" | "pr_opened" => AgentTrigger::PullRequestOpened,
        "pull_request_updated" | "pr_updated" => AgentTrigger::PullRequestUpdated,
        "mention" => AgentTrigger::Mention,
        other => bail!(
            "unknown event {other:?}; use issue_opened, issue_labeled[:LABEL], pull_request_opened, pull_request_updated or mention"
        ),
    };
    Ok(t)
}

fn trigger_label(t: &AgentTrigger) -> String {
    match t {
        AgentTrigger::IssueOpened => "issue_opened".into(),
        AgentTrigger::IssueLabeled { label: Some(l) } => format!("issue_labeled:{l}"),
        AgentTrigger::IssueLabeled { label: None } => "issue_labeled".into(),
        AgentTrigger::PullRequestOpened => "pull_request_opened".into(),
        AgentTrigger::PullRequestUpdated => "pull_request_updated".into(),
        AgentTrigger::Mention => "mention".into(),
    }
}

fn parse_placement(s: &str) -> Result<Placement> {
    serde_json::from_value(Value::String(s.trim().replace('-', "_")))
        .map_err(|_| anyhow::anyhow!("unknown placement {s:?}; use auto, hosted or self-hosted"))
}

fn placement_label(p: Placement) -> &'static str {
    match p {
        Placement::Auto => "auto",
        Placement::Hosted => "hosted",
        Placement::SelfHosted => "self-hosted",
    }
}

/// An agent file: TOML (by extension) or JSON. `name` may come from the command line instead.
pub(super) fn parse_file(text: &str, toml_format: bool, name: Option<&str>) -> Result<AgentSpec> {
    let mut v: Value = if toml_format {
        toml::from_str(text).context("not valid TOML")?
    } else {
        serde_json::from_str(text).context("not valid JSON")?
    };
    let obj = v
        .as_object_mut()
        .context("an agent file is a table of settings")?;
    if let Some(n) = name {
        obj.insert("name".into(), Value::String(n.into()));
    }
    obj.entry("name").or_insert(Value::String(String::new()));
    obj.entry("instructions")
        .or_insert(Value::String(String::new()));
    obj.entry("repo").or_insert(Value::String(String::new()));
    // `chanto agents show --json` output works as a file: drop what the server sets.
    for k in [
        "id",
        "org",
        "next_run_at",
        "last_run",
        "last_error",
        "created_at",
        "updated_at",
    ] {
        obj.remove(k);
    }
    // `branch` reads better in a file; the wire name is `git_ref`.
    if let Some(b) = obj.remove("branch") {
        obj.entry("git_ref").or_insert(b);
    }
    // Triggers may be strings ("issue_labeled:bug") as on the command line.
    if let Some(Value::Array(ts)) = obj.get_mut("triggers") {
        for t in ts.iter_mut() {
            if let Value::String(s) = t {
                *t = serde_json::to_value(parse_trigger(s)?)?;
            }
        }
    }
    if let Some(Value::String(p)) = obj.get("placement") {
        let p = parse_placement(p)?;
        obj.insert("placement".into(), serde_json::to_value(p)?);
    }
    serde_json::from_value(v).context("not an agent: check the field names and types")
}

impl SpecArgs {
    fn has_any(&self) -> bool {
        self.file.is_some()
            || self.repo.is_some()
            || self.instructions.is_some()
            || self.instructions_file.is_some()
            || self.description.is_some()
            || self.schedule.is_some()
            || !self.on.is_empty()
            || self.placement.is_some()
            || !self.labels.is_empty()
            || self.branch.is_some()
            || self.disable
            || self.enable
    }

    /// The file's spec, if any, then the flags on top of `base`.
    pub(super) fn apply(&self, mut spec: AgentSpec) -> Result<AgentSpec> {
        if let Some(f) = &self.file {
            let text =
                std::fs::read_to_string(f).with_context(|| format!("reading {}", f.display()))?;
            let toml_format = f.extension().is_some_and(|e| e == "toml");
            spec = parse_file(
                &text,
                toml_format,
                Some(&spec.name)
                    .filter(|n| !n.is_empty())
                    .map(String::as_str),
            )
            .with_context(|| format!("in {}", f.display()))?;
        }
        if let Some(r) = &self.repo {
            spec.repo = r.trim().to_string();
        }
        if let Some(i) = &self.instructions {
            spec.instructions = i.clone();
        }
        if let Some(f) = &self.instructions_file {
            spec.instructions =
                std::fs::read_to_string(f).with_context(|| format!("reading {}", f.display()))?;
        }
        if let Some(d) = &self.description {
            spec.description = Some(d.clone()).filter(|d| !cleared(d));
        }
        if let Some(s) = &self.schedule {
            spec.schedule = Some(s.trim().to_string()).filter(|s| !cleared(s));
        }
        if !self.on.is_empty() {
            spec.triggers = self
                .on
                .iter()
                .filter(|s| !cleared(s))
                .map(|s| parse_trigger(s))
                .collect::<Result<_>>()?;
        }
        if let Some(p) = &self.placement {
            spec.placement = Some(parse_placement(p)?);
        }
        if !self.labels.is_empty() {
            spec.labels = self
                .labels
                .iter()
                .filter(|l| !cleared(l))
                .cloned()
                .collect();
        }
        if let Some(b) = &self.branch {
            spec.git_ref = Some(b.trim().to_string()).filter(|b| !cleared(b));
        }
        if self.disable {
            spec.enabled = false;
        }
        if self.enable {
            spec.enabled = true;
        }
        Ok(spec)
    }
}

fn check(spec: &AgentSpec) -> Result<()> {
    if !valid_agent_name(&spec.name) {
        bail!(
            "agent names are lowercase letters, digits and dashes, starting with a letter: {:?}",
            spec.name
        );
    }
    if spec.repo.trim().is_empty() {
        bail!("an agent needs a repository: --repo <git url>");
    }
    if spec.instructions.trim().is_empty() {
        bail!("an agent needs instructions: --instructions <text> or --instructions-file <file>");
    }
    Ok(())
}

fn empty_spec(name: &str) -> AgentSpec {
    AgentSpec {
        name: name.into(),
        description: None,
        instructions: String::new(),
        repo: String::new(),
        git_ref: None,
        placement: None,
        labels: vec![],
        schedule: None,
        triggers: vec![],
        enabled: true,
        model_keys: None,
        gateway_model: None,
        script: None,
        model_runs: None,
        checks: None,
    }
}

fn when(t: &chrono::DateTime<chrono::Utc>) -> String {
    t.with_timezone(&chrono::Local)
        .format("%Y-%m-%d %H:%M")
        .to_string()
}

fn status_label(s: RunStatus) -> &'static str {
    match s {
        RunStatus::Queued => "queued",
        RunStatus::Running => "running",
        RunStatus::Succeeded => "succeeded",
        RunStatus::Failed => "failed",
        RunStatus::Cancelled => "cancelled",
    }
}

/// What starts it, in a few words.
fn starts(spec: &AgentSpec) -> String {
    let mut parts: Vec<String> = spec.schedule.iter().map(|s| format!("cron {s}")).collect();
    parts.extend(spec.triggers.iter().map(trigger_label));
    if !spec.enabled {
        return "by hand (disabled)".into();
    }
    if parts.is_empty() {
        "by hand".into()
    } else {
        parts.join(", ")
    }
}

pub(crate) fn list_table(agents: &[Agent]) -> String {
    if agents.is_empty() {
        return "No agents yet. Create one: chanto agents create <name> --repo <git url> --instructions <text> [--schedule <cron>] [--on <event>]".into();
    }
    let mut s = format!(
        "{:<24} {:<40} {:<28} {}\n",
        "NAME", "STARTS", "LAST RUN", "NEXT"
    );
    for a in agents {
        let last = a
            .last_run
            .as_ref()
            .map(|r| format!("{} {}", when(&r.created_at), status_label(r.status)))
            .unwrap_or_else(|| "-".into());
        let next = a
            .next_run_at
            .as_ref()
            .map(when)
            .unwrap_or_else(|| "-".into());
        s.push_str(&format!(
            "{:<24} {:<40} {:<28} {}\n",
            a.spec.name,
            crate::util::truncate(&starts(&a.spec), 40),
            last,
            next
        ));
        if let Some(e) = &a.last_error {
            s.push_str(&format!("  ! {}\n", crate::util::one_line(e, 160)));
        }
    }
    s.trim_end().to_string()
}

fn show_text(a: &Agent) -> String {
    let sp = &a.spec;
    let mut s = format!(
        "{} ({}, org {})\n",
        sp.name,
        if sp.enabled { "enabled" } else { "disabled" },
        a.org
    );
    if let Some(d) = &sp.description {
        s.push_str(&format!("  {d}\n"));
    }
    s.push_str(&format!(
        "  repo        {}{}\n",
        sp.repo,
        sp.git_ref
            .as_ref()
            .map(|r| format!(" @ {r}"))
            .unwrap_or_default()
    ));
    s.push_str(&format!("  starts      {}\n", starts(sp)));
    if let Some(n) = &a.next_run_at {
        s.push_str(&format!("  next run    {}\n", when(n)));
    }
    s.push_str(&format!(
        "  placement   {}\n",
        sp.placement.map(placement_label).unwrap_or("org default")
    ));
    if !sp.labels.is_empty() {
        s.push_str(&format!("  labels      {}\n", sp.labels.join(", ")));
    }
    if let Some(r) = &a.last_run {
        s.push_str(&format!(
            "  last run    {} {} ({})\n",
            r.id,
            status_label(r.status),
            when(&r.created_at)
        ));
    }
    if let Some(e) = &a.last_error {
        s.push_str(&format!("  last error  {e}\n"));
    }
    s.push_str("\nInstructions\n");
    for l in sp.instructions.trim_end().lines() {
        s.push_str(&format!("  {l}\n"));
    }
    s.trim_end().to_string()
}

fn runs_table(runs: &[Run]) -> String {
    if runs.is_empty() {
        return "No runs yet.".into();
    }
    let mut s = format!(
        "{:<38} {:<10} {:<17} {}\n",
        "RUN", "STATUS", "CREATED", "RESULT"
    );
    for r in runs {
        let result = r
            .result
            .as_ref()
            .map(|x| {
                x.pr_url
                    .clone()
                    .unwrap_or_else(|| crate::util::one_line(&x.summary, 80))
            })
            .unwrap_or_default();
        s.push_str(&format!(
            "{:<38} {:<10} {:<17} {}\n",
            r.id,
            status_label(r.status),
            when(&r.created_at),
            result
        ));
    }
    s.trim_end().to_string()
}

fn print_json(v: &impl serde::Serialize) -> Result<()> {
    println!("{}", serde_json::to_string_pretty(v)?);
    Ok(())
}

fn confirm(question: &str) -> bool {
    eprint!("{question} [y/N] ");
    let mut line = String::new();
    let _ = std::io::stdin().read_line(&mut line);
    matches!(line.trim(), "y" | "Y" | "yes")
}

fn interactive() -> bool {
    use std::io::IsTerminal;
    std::io::stdin().is_terminal()
        && std::io::stderr().is_terminal()
        && std::env::var_os("CI").is_none()
}

/// `chanto agents …`; the exit code.
pub async fn cli(args: &[String]) -> Result<i32> {
    let a = match AgentsCli::try_parse_from(
        std::iter::once("chanto agents".to_string()).chain(args.iter().cloned()),
    ) {
        Ok(a) => a,
        Err(e)
            if matches!(
                e.kind(),
                clap::error::ErrorKind::DisplayHelp | clap::error::ErrorKind::DisplayVersion
            ) =>
        {
            print!("{e}");
            return Ok(0);
        }
        Err(e) => bail!("{e}"),
    };
    let client = Cloud::signed_in()?;
    let org = resolve_org(&client, a.org.as_deref()).await?;
    run_cmd(&client, &org, a.cmd.unwrap_or(Cmd::List), a.json).await
}

async fn run_cmd(client: &Cloud, org: &str, cmd: Cmd, json: bool) -> Result<i32> {
    let base = format!("/v1/orgs/{}/agents", seg(org));
    match cmd {
        Cmd::List => {
            let list: AgentList = client
                .get(&base)
                .await
                .with_context(|| format!("agents in org {org}"))?;
            if json {
                print_json(&list)?
            } else {
                println!("{}", list_table(&list.agents))
            }
        }
        Cmd::Show { name } => {
            let agent: Agent = client
                .get(&format!("{base}/{}", seg(&name)))
                .await
                .with_context(|| format!("agent {name} in org {org}"))?;
            if json {
                print_json(&agent)?
            } else {
                println!("{}", show_text(&agent))
            }
        }
        Cmd::Create { name, spec } => {
            let s = spec.apply(empty_spec(name.as_deref().unwrap_or("")))?;
            check(&s)?;
            let agent: Agent = client
                .post(&base, &s)
                .await
                .with_context(|| format!("creating agent {} in org {org}", s.name))?;
            if json {
                print_json(&agent)?
            } else {
                println!(
                    "Created agent {} in {}. Start it now with `chanto agents run {}`.",
                    agent.spec.name, agent.org, agent.spec.name
                )
            }
        }
        Cmd::Edit { name, rename, spec } => {
            if !spec.has_any() && rename.is_none() {
                bail!("nothing to change; see `chanto agents edit --help`");
            }
            let current: Agent = client
                .get(&format!("{base}/{}", seg(&name)))
                .await
                .with_context(|| format!("agent {name} in org {org}"))?;
            let mut s = spec.apply(current.spec)?;
            if let Some(r) = rename {
                s.name = r;
            }
            check(&s)?;
            let agent: Agent = client
                .put(&format!("{base}/{}", seg(&name)), &s)
                .await
                .with_context(|| format!("updating agent {name} in org {org}"))?;
            if json {
                print_json(&agent)?
            } else {
                println!("Updated agent {}.", agent.spec.name)
            }
        }
        Cmd::Delete { name, yes } => {
            if !yes {
                if !interactive() {
                    bail!("deleting needs --yes when there's no terminal to ask");
                }
                if !confirm(&format!("Delete agent {name} in {org}? Its runs are kept.")) {
                    return Ok(1);
                }
            }
            client
                .delete(&format!("{base}/{}", seg(&name)))
                .await
                .with_context(|| format!("deleting agent {name} in org {org}"))?;
            println!("Deleted agent {name}.");
        }
        Cmd::Run { name, run } => return start(client, org, &name, run, json).await,
        Cmd::Runs { name, limit } => {
            let list: RunList = client
                .get(&format!(
                    "/v1/orgs/{}/runs?agent={}&limit={limit}",
                    seg(org),
                    seg(&name)
                ))
                .await
                .with_context(|| format!("runs of agent {name} in org {org}"))?;
            if json {
                print_json(&list)?
            } else {
                println!("{}", runs_table(&list.runs))
            }
        }
    }
    Ok(0)
}

/// `chanto cloud run --agent <name> …`: for CI, where `CHANTO_CLOUD_TOKEN`
/// (and `CHANTO_CLOUD_URL`, `CHANTO_CLOUD_ORG`) stand in for a login.
pub async fn cloud_run(args: &[String]) -> Result<i32> {
    #[derive(Parser)]
    #[command(
        name = "chanto cloud run",
        about = "Start an agent's run in Chanto cloud (for CI: set CHANTO_CLOUD_TOKEN)"
    )]
    struct Args {
        /// The agent to run
        #[arg(long)]
        agent: String,
        #[arg(long)]
        org: Option<String>,
        #[arg(long)]
        json: bool,
        #[command(flatten)]
        run: RunArgs,
    }
    let a = match Args::try_parse_from(
        std::iter::once("chanto cloud run".to_string()).chain(args.iter().cloned()),
    ) {
        Ok(a) => a,
        Err(e) if e.kind() == clap::error::ErrorKind::DisplayHelp => {
            print!("{e}");
            println!(
                "\nEnvironment: CHANTO_CLOUD_TOKEN (an account token, such as the one `chanto cloud login` saves), CHANTO_CLOUD_URL, CHANTO_CLOUD_ORG"
            );
            return Ok(0);
        }
        Err(e) => bail!("{e}"),
    };
    let client = Cloud::signed_in()?;
    let org = resolve_org(&client, a.org.as_deref()).await?;
    start(&client, &org, &a.agent, a.run, a.json).await
}

/// Starts a run; with `--wait`, follows it to the end. The exit code: 0 unless
/// a followed run didn't succeed (130: stopped following).
pub(super) async fn start(
    client: &Cloud,
    org: &str,
    name: &str,
    args: RunArgs,
    json: bool,
) -> Result<i32> {
    let body = RunAgent {
        input: args.input.filter(|s| !s.trim().is_empty()),
        git_ref: args.git_ref.filter(|s| !s.trim().is_empty()),
    };
    let run: Run = client
        .post(
            &format!("/v1/orgs/{}/agents/{}/runs", seg(org), seg(name)),
            &body,
        )
        .await
        .with_context(|| format!("starting agent {name} in org {org}"))?;
    if !args.wait {
        if json {
            print_json(&run)?;
        } else {
            println!(
                "Started run {} of {name} ({}). Follow it with `chanto attach {} --cloud`.",
                run.id,
                status_label(run.status),
                run.id
            );
        }
        return Ok(0);
    }
    let end = if json {
        // Only the run on stdout: no events, and Ctrl-C just ends the wait.
        let end = tokio::select! {
            r = settle(client, &run.id) => Some(r?),
            _ = tokio::signal::ctrl_c() => None,
        };
        print_json(end.as_ref().unwrap_or(&run))?;
        end
    } else {
        println!(
            "{}",
            crate::ui::dim(&format!(
                "Started run {} of {name} · Ctrl-C cancels it or stops following",
                run.id
            ))
        );
        let ask = |q: &str| if interactive() { confirm(q) } else { true };
        crate::runs::cloud::follow_or_cancel(client, &run.id, POLL, Some(&ask)).await?
    };
    Ok(exit_code(end.as_ref()))
}

/// Waits quietly until the run finishes.
async fn settle(client: &Cloud, id: &str) -> Result<Run> {
    loop {
        let run = client.run(id).await?;
        if run.status.is_terminal() {
            return Ok(run);
        }
        tokio::time::sleep(POLL).await;
    }
}

/// `None`: stopped following before the run finished.
pub(super) fn exit_code(run: Option<&Run>) -> i32 {
    match run.map(|r| r.status) {
        Some(RunStatus::Succeeded) => 0,
        Some(_) => 1,
        None => 130,
    }
}
