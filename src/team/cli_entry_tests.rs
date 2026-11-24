//! `chanto team`'s subcommands that answer without a model or a daemon.

use super::*;
use crate::agent::tests::tmpdir;
use crate::test_support::isolated;

fn cli_with(words: &[&str], root: &std::path::Path) -> crate::Cli {
    let mut args = vec!["chanto", "-C", root.to_str().unwrap()];
    args.extend_from_slice(words);
    <crate::Cli as clap::Parser>::parse_from(args)
}

#[tokio::test]
async fn subcommands_report_or_refuse() {
    if isolated("team::cli_entry_tests::subcommands_report_or_refuse") {
        return;
    }
    let root = crate::util::canonical(tmpdir("team-cli")).unwrap();
    assert_eq!(cli(cli_with(&["team", "list"], &root)).await.unwrap(), 0);
    assert!(
        cli(cli_with(&["team", "show", "ghost"], &root))
            .await
            .is_err()
    );
    assert!(
        cli(cli_with(&["team", "apply", "ghost"], &root))
            .await
            .is_err()
    );
    let e = cli(cli_with(&["team", "--bg", "a", "task"], &root))
        .await
        .unwrap_err();
    assert!(e.to_string().contains("already runs its sub-tasks"), "{e}");
    let shown = command(&root, "").await;
    assert!(shown.starts_with("No team yet."), "{shown}");
    assert!(Team::load("ghost").is_err());
}

#[test]
fn config_merge_only_replaces_what_is_set() {
    let mut c = TeamConfig {
        agents: Some(2),
        ..Default::default()
    };
    c.merge(TeamConfig::default());
    assert_eq!(c.agents, Some(2));
    c.merge(TeamConfig {
        agents: Some(5),
        max_cost_usd: Some(1.5),
    });
    assert_eq!((c.agents, c.max_cost_usd), (Some(5), Some(1.5)));
}
