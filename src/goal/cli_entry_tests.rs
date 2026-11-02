//! `chanto goal`'s subcommands that answer without a model.

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
    if isolated("goal::cli_entry_tests::subcommands_report_or_refuse") {
        return;
    }
    let root = crate::util::canonical(tmpdir("goal-cli")).unwrap();
    let e = cli(cli_with(&["goal"], &root)).await.unwrap_err();
    assert!(e.to_string().starts_with("usage: chanto goal"), "{e}");
    assert_eq!(cli(cli_with(&["goal", "list"], &root)).await.unwrap(), 0);
    let e = cli(cli_with(&["goal", "status"], &root)).await.unwrap_err();
    assert!(e.to_string().contains("no goals for"), "{e}");
    assert!(cli(cli_with(&["goal", "pause"], &root)).await.is_err());
    assert!(
        cli(cli_with(&["goal", "stop", "ghost"], &root))
            .await
            .is_err()
    );
    let e = cli(cli_with(&["goal", "--bg", "resume", "x"], &root))
        .await
        .unwrap_err();
    assert!(e.to_string().contains("chanto runs resume"), "{e}");

    assert_eq!(command(&root, "list"), "No goals for this project.");
    assert_eq!(
        command(&root, "pause"),
        "No goal is running in this session."
    );
    assert_eq!(
        command(&root, "stop"),
        "No goal is running in this session."
    );
    assert!(
        command(&root, "status ghost")
            .to_lowercase()
            .contains("ghost")
    );
}
