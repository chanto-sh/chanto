use super::*;

#[test]
fn the_quit_warning_names_how_many_tasks_are_running() {
    assert_eq!(
        quit_warning(1),
        "**1 background task is still running.** Quit anyway? It stops when chanto exits."
    );
    let many = quit_warning(3);
    assert!(
        many.contains("**3 background tasks are still running.**"),
        "{many}"
    );
}
