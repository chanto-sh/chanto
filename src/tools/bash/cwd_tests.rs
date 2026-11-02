use super::settle_cwd;

#[test]
fn the_shell_stays_inside_the_project_or_added_dirs() {
    let base = crate::permissions::normalize(&std::env::temp_dir())
        .join(format!("chanto-settle-{}", uuid::Uuid::new_v4().simple()));
    let (root, sub, extra) = (base.join("proj"), base.join("proj/sub"), base.join("extra"));
    for d in [&sub, &extra] {
        std::fs::create_dir_all(d).unwrap();
    }
    let extras = vec![extra.clone()];
    assert_eq!(
        settle_cwd(&sub, &root, &extras, false),
        (sub.clone(), false)
    );
    assert_eq!(
        settle_cwd(&extra, &root, &extras, false),
        (extra.clone(), false)
    );
    assert_eq!(
        settle_cwd(&base, &root, &extras, false),
        (root.clone(), true)
    );
    // A sibling whose name starts with the root's isn't inside it.
    let sibling = base.join("proj-other");
    std::fs::create_dir_all(&sibling).unwrap();
    assert_eq!(
        settle_cwd(&sibling, &root, &extras, false),
        (root.clone(), true)
    );
    // CLAUDE_BASH_MAINTAIN_PROJECT_WORKING_DIR: always back to the root,
    // with the note only when the command had left the project.
    assert_eq!(
        settle_cwd(&sub, &root, &extras, true),
        (root.clone(), false)
    );
    assert_eq!(
        settle_cwd(&base, &root, &extras, true),
        (root.clone(), true)
    );
    let _ = std::fs::remove_dir_all(base);
}
