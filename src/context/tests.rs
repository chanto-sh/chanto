use super::*;

#[test]
fn git_runs_only_where_a_repository_can_be_found() {
    let d = std::env::temp_dir().join(format!("chanto-git-{}", uuid::Uuid::new_v4().simple()));
    let sub = d.join("a/b");
    std::fs::create_dir_all(&sub).unwrap();
    let env_points_elsewhere =
        std::env::var_os("GIT_DIR").is_some() || std::env::var_os("GIT_WORK_TREE").is_some();
    let outside = std::env::temp_dir()
        .ancestors()
        .any(|a| a.join(".git").exists());
    if !env_points_elsewhere && !outside {
        assert!(!may_be_in_git_repo(&sub));
    }
    // A work tree (`.git` is a directory, or a file in a linked worktree)
    // found from any directory below it.
    std::fs::write(d.join(".git"), "gitdir: /elsewhere\n").unwrap();
    assert!(may_be_in_git_repo(&sub));
    std::fs::remove_file(d.join(".git")).unwrap();
    // A bare repository.
    std::fs::create_dir_all(d.join("objects")).unwrap();
    std::fs::create_dir_all(d.join("refs")).unwrap();
    std::fs::write(d.join("HEAD"), "ref: refs/heads/main\n").unwrap();
    assert!(may_be_in_git_repo(&d));
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn instruction_files_have_a_total_budget() {
    let d = std::env::temp_dir().join(format!(
        "chanto-ctx-budget-{}",
        uuid::Uuid::new_v4().simple()
    ));
    std::fs::create_dir_all(d.join("docs")).unwrap();
    let paths: Vec<PathBuf> = (0..10)
        .map(|i| {
            let p = d.join(format!("docs/d{i}.md"));
            std::fs::write(&p, format!("doc {i} {}", "x".repeat(39_000))).unwrap();
            p
        })
        .collect();
    let out = load_unique(paths, &d);
    let total: usize = out.iter().map(|(_, t)| t.len()).sum();
    assert!(
        out.len() == 4 && total < MAX_INSTRUCTION_TOTAL + 2000,
        "{} files, {total} bytes",
        out.len()
    );
    let note = &out.last().unwrap().1;
    assert!(
        note.contains("6 more instruction files weren't loaded") && note.contains("d9.md"),
        "{}",
        &note[note.len() - 300..]
    );
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn instruction_discovery() {
    let d = std::env::temp_dir().join(format!("chanto-ctx-{}", uuid::Uuid::new_v4().simple()));
    std::fs::create_dir_all(&d).unwrap();
    let d = d.canonical().unwrap();
    let _home_lock = crate::compat::TEST_HOME_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    unsafe { std::env::set_var("CHANTO_COMPAT_HOME", d.join("home")) };
    let w = |p: &str, t: &str| {
        let p = d.join(p);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, t).unwrap();
    };
    std::fs::create_dir_all(d.join(".git")).unwrap();
    w("AGENTS.md", "base agents");
    w("AGENTS.override.md", "override agents");
    w(
        "CLAUDE.md",
        "claude root\nSee @docs/style.md and `@not/imported.md`.",
    );
    w("docs/style.md", "use tabs");
    w("home/secret.txt", "TOPSECRET");
    w("AGENTS.local.md", "local @home/secret.txt @../../etc/hosts");
    w(".claude/rules/always.md", "always rule");
    w(
        ".claude/rules/ts.md",
        "---\npaths:\n  - \"web/**/*.ts\"\n---\nts rule",
    );
    w("pkg/AGENTS.md", "pkg rules");
    w("web/app.ts", "x");
    let files = instruction_files(&d);
    let all: String = files
        .iter()
        .map(|(_, t)| t.as_str())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(all.contains("override agents"));
    assert!(!all.contains("base agents"), "override replaces AGENTS.md");
    assert!(all.contains("use tabs"), "@import inlined");
    assert!(
        all.contains("TOPSECRET"),
        "imports inside the project are fine"
    );
    assert!(
        !all.contains("localhost"),
        "untrusted project files can't import outside the project"
    );
    #[cfg(unix)]
    {
        // Nor be a symlink to a file outside it.
        let out = d
            .parent()
            .unwrap()
            .join(format!("chanto-ctx-out-{}", uuid::Uuid::new_v4().simple()));
        std::fs::write(&out, "OUTSIDE-SECRET").unwrap();
        std::os::unix::fs::symlink(&out, d.join("CLAUDE.local.md")).unwrap();
        let files = instruction_files(&d);
        std::fs::remove_file(d.join("CLAUDE.local.md")).unwrap();
        std::fs::remove_file(&out).unwrap();
        assert!(
            !files.iter().any(|(_, t)| t.contains("OUTSIDE-SECRET")),
            "a symlink out of an untrusted project isn't read"
        );
    }
    assert!(all.contains("always rule"));
    assert!(!all.contains("ts rule"), "path-scoped rule loads lazily");
    assert!(!all.contains("pkg rules"), "nested files load lazily");
    let mut loaded = std::collections::HashSet::new();
    let n = nested_instructions(&d, &d.join("pkg/lib.rs"), &mut loaded);
    assert_eq!(n.len(), 1);
    assert!(n[0].1.contains("pkg rules"));
    assert!(
        nested_instructions(&d, &d.join("pkg/other.rs"), &mut loaded).is_empty(),
        "only once"
    );
    let n = nested_instructions(&d, &d.join("web/app.ts"), &mut loaded);
    assert!(n.iter().any(|(_, t)| t.contains("ts rule")));
}

#[cfg(unix)]
#[test]
fn a_rule_added_after_a_directory_was_seen_empty_is_found() {
    let d = std::env::temp_dir().join(format!(
        "chanto-ctx-norules-{}",
        uuid::Uuid::new_v4().simple()
    ));
    std::fs::create_dir_all(d.join(".git")).unwrap();
    let d = d.canonical().unwrap();
    // Untouched for an hour: old enough for "no rules here" to be remembered.
    let hour_ago = std::time::SystemTime::now() - std::time::Duration::from_secs(3600);
    let age = |p: &Path| {
        std::fs::File::open(p)
            .unwrap()
            .set_modified(hour_ago)
            .unwrap()
    };
    age(&d);
    let mut loaded = std::collections::HashSet::new();
    assert!(nested_instructions(&d, &d.join("a.ts"), &mut loaded).is_empty());
    assert!(!may_hold_rules(&d), "remembered as holding no rules");
    std::fs::create_dir_all(d.join(".claude/rules")).unwrap();
    std::fs::write(
        d.join(".claude/rules/ts.md"),
        "---\npaths:\n  - \"**/*.ts\"\n---\nts rule",
    )
    .unwrap();
    let n = nested_instructions(&d, &d.join("b.ts"), &mut loaded);
    assert!(n.iter().any(|(_, t)| t.contains("ts rule")), "{n:?}");
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn other_tools_instructions_need_trust() {
    let d = std::env::temp_dir().join(format!(
        "chanto-ctx-foreign-{}",
        uuid::Uuid::new_v4().simple()
    ));
    std::fs::create_dir_all(d.join(".git")).unwrap();
    let d = d.canonical().unwrap();
    let _home_lock = crate::compat::TEST_HOME_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    unsafe { std::env::set_var("CHANTO_COMPAT_HOME", d.join("home")) };
    let w = |p: &str, t: &str| {
        let p = d.join(p);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, t).unwrap();
    };
    w("home/.gemini/GEMINI.md", "user gemini");
    w("AGENTS.md", "shared text");
    w("GEMINI.md", "shared text");
    w("sub/GEMINI.md", "sub gemini");
    w(".cursorrules", "legacy cursor");
    w(
        ".cursor/rules/api.mdc",
        "---\ndescription: API conventions\n---\napi",
    );
    w(
        ".github/instructions/rs.instructions.md",
        "---\napplyTo: \"**/*.rs\"\n---\nrust rule",
    );
    let text = |f: &[(PathBuf, String)]| {
        f.iter()
            .map(|(_, t)| t.as_str())
            .collect::<Vec<_>>()
            .join("\n")
    };
    let all = text(&instruction_files(&d));
    assert!(all.contains("user gemini"), "user files load anywhere");
    assert!(
        !all.contains("legacy cursor"),
        "project files of other tools need trust"
    );
    crate::config::trust(&d).unwrap();
    let files = instruction_files(&d);
    let all = text(&files);
    assert!(all.contains("legacy cursor"));
    assert_eq!(
        all.matches("shared text").count(),
        1,
        "identical content is loaded once"
    );
    assert!(all.contains("api.mdc (Cursor): API conventions"));
    assert!(!all.contains("rust rule"));
    let mut loaded = std::collections::HashSet::new();
    let n = text(&nested_instructions(&d, &d.join("sub/lib.rs"), &mut loaded));
    assert!(n.contains("sub gemini") && n.contains("rust rule"));
    assert_eq!(scoped_foreign_rules(&d).len(), 1);
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn frontmatter_yaml_subset() {
    let fm = frontmatter(
        "---\n# comment\nname: \"quoted: name\"\ndescription: >-\n  Folded\n  text\n\n  next\nnotes: |\n  line 1\n    indented\ntools:\n  - Read\n  - 'Grep'   # trailing\nflow: [Bash(git:*), \"a, b\"]\nplain: Read, Grep # c\nlong: starts here\n  and continues\nbad: [Read, Grep\nhooks:\n  Stop:\n    - command: x\nempty:\n---\nbody",
    );
    let get = |k: &str| fm.iter().find(|(a, _)| a == k).map(|(_, v)| v.clone());
    assert_eq!(get("name"), Some(FmValue::Str("quoted: name".into())));
    assert_eq!(
        get("description"),
        Some(FmValue::Str("Folded text\nnext".into()))
    );
    assert_eq!(
        get("notes"),
        Some(FmValue::Str("line 1\n  indented\n".into()))
    );
    assert_eq!(
        get("tools"),
        Some(FmValue::List(vec!["Read".into(), "Grep".into()]))
    );
    assert_eq!(
        get("flow"),
        Some(FmValue::List(vec!["Bash(git:*)".into(), "a, b".into()]))
    );
    assert_eq!(
        get("long"),
        Some(FmValue::Str("starts here and continues".into()))
    );
    assert_eq!(get("bad"), Some(FmValue::Other));
    assert_eq!(get("hooks"), Some(FmValue::Other));
    assert_eq!(
        fm_list(&fm, "plain"),
        Some(Ok(vec!["Read".to_string(), "Grep".to_string()]))
    );
    assert_eq!(fm_list(&fm, "bad"), Some(Err(())));
    assert_eq!(fm_list(&fm, "empty"), None);
    assert_eq!(fm_get(&fm, "tools").as_deref(), Some("Read, Grep"));
    assert_eq!(
        split_commas("src/*.{ts,tsx}, lib/*.rs"),
        vec!["src/*.{ts,tsx}", "lib/*.rs"]
    );
    assert!(frontmatter("---\nbroken").is_empty());
}

#[test]
fn nested_frontmatter_reads_as_yaml() {
    let text = "---\nname: db-reader\n# c\nhooks:\n  PreToolUse:\n    - matcher: \"Bash\"\n      hooks:\n        - type: command\n          command: \"./validate.sh # not a comment\"\n          timeout: 30\n          async: true\n  Stop:\n  - hooks:\n    - type: prompt\n      prompt: |\n        Line one\n          indented\n      continueOnBlock: yes\nmcpServers:\n  - playwright:\n      type: stdio\n      command: npx\n      args: [\"-y\", '@playwright/mcp@latest']\n  - github\nflow: {a: 1, b: [x, y]}\n---\nbody";
    let y = frontmatter_yaml(text);
    assert_eq!(y["name"], "db-reader");
    let pre = &y["hooks"]["PreToolUse"][0];
    assert_eq!(pre["matcher"], "Bash");
    assert_eq!(pre["hooks"][0]["command"], "./validate.sh # not a comment");
    assert_eq!(pre["hooks"][0]["timeout"], 30);
    assert_eq!(pre["hooks"][0]["async"], true);
    assert_eq!(
        y["hooks"]["Stop"][0]["hooks"][0]["prompt"],
        "Line one\n  indented"
    );
    assert_eq!(y["hooks"]["Stop"][0]["hooks"][0]["continueOnBlock"], "yes");
    assert_eq!(
        y["mcpServers"][0]["playwright"]["args"],
        serde_json::json!(["-y", "@playwright/mcp@latest"])
    );
    assert_eq!(y["mcpServers"][1], "github");
    assert_eq!(y["flow"], serde_json::json!({"a": 1, "b": ["x", "y"]}));
    let h = fm_hooks(&y).unwrap();
    assert_eq!(h["PreToolUse"][0].command, "./validate.sh # not a comment");
    assert!(h["PreToolUse"][0].run_async && h["PreToolUse"][0].matcher == "Bash");
    assert!(
        !h.contains_key("Stop"),
        "a hook whose fields don't parse is dropped, the rest kept"
    );
    let m = fm_mcp(&y);
    assert!(
        matches!(&m[0], AgentMcp::Inline(n, c) if n == "playwright" && c.command == "npx" && c.args.len() == 2)
    );
    assert!(matches!(&m[1], AgentMcp::Named(n) if n == "github"));
    assert!(frontmatter_yaml("no frontmatter").is_empty());
}

#[test]
fn skill_and_agent_frontmatter_fields() {
    let d = std::env::temp_dir().join(format!("chanto-fm-{}", uuid::Uuid::new_v4().simple()));
    std::fs::create_dir_all(d.join(".git")).unwrap();
    let d = d.canonical().unwrap();
    let _home_lock = crate::compat::TEST_HOME_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    unsafe { std::env::set_var("CHANTO_COMPAT_HOME", d.join("home")) };
    let w = |p: &str, t: &str| {
        let p = d.join(p);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, t).unwrap();
    };
    w(
        ".claude/skills/commit/SKILL.md",
        "---\ndescription: Commit\nargument-hint: \"[message]\"\nallowed-tools: Bash(git add *) Bash(git commit *), Read\nmodel: inherit\neffort: low\nshell: PowerShell\ncontext: fork\nbackground: No\nhooks:\n  PostToolUse:\n    - matcher: Bash\n      hooks:\n        - type: command\n          command: echo hi\n          once: true\n---\nCommit.",
    );
    w(
        ".claude/commands/fix.md",
        "---\nargument-hint: [issue]\nallowed-tools:\n  - Edit\nmodel: sonnet\n---\nFix $1",
    );
    w(
        ".claude/agents/full.md",
        "---\nname: full\ndescription: All fields\npermissionMode: acceptEdits\nmaxTurns: 12\nskills: [commit, other]\nmcpServers:\n  - github\nhooks:\n  Stop:\n    - hooks:\n        - type: command\n          command: ./stop.sh\nmemory: Local\nbackground: true\neffort: max\nisolation: worktree\ninitialPrompt: /review\ncolor: Purple\nomitClaudeMd: true\nunknownKey: ignored\n---\nPrompt.",
    );
    let skills = load_skills(&d);
    let s = &skills.iter().find(|s| s.name == "commit").unwrap().meta;
    assert_eq!(s.argument_hint.as_deref(), Some("[message]"));
    assert_eq!(
        s.allowed_tools,
        vec!["Bash(git add *)", "Bash(git commit *)", "Read"]
    );
    assert_eq!(
        (
            s.model.as_deref(),
            s.effort.as_deref(),
            s.shell.as_deref(),
            s.background
        ),
        (None, Some("low"), Some("powershell"), Some(false))
    );
    assert!(s.hooks.as_ref().unwrap()["PostToolUse"][0].once);
    let c = &load_commands(&d)
        .into_iter()
        .find(|c| c.name == "fix")
        .unwrap()
        .meta;
    assert_eq!(
        (c.argument_hint.as_deref(), c.model.as_deref()),
        (Some("[issue]"), Some("sonnet"))
    );
    assert_eq!(c.allowed_tools, vec!["Edit"]);
    let a = load_agents(&d)
        .into_iter()
        .find(|a| a.name == "full")
        .unwrap();
    assert_eq!(
        (
            a.permission_mode.as_deref(),
            a.max_turns,
            a.memory.as_deref(),
            a.effort.as_deref()
        ),
        (Some("acceptEdits"), Some(12), Some("local"), Some("max"))
    );
    assert_eq!(a.skills, vec!["commit", "other"]);
    assert!(matches!(&a.mcp_servers[..], [AgentMcp::Named(n)] if n == "github"));
    assert_eq!(a.hooks.as_ref().unwrap()["Stop"][0].command, "./stop.sh");
    assert!(a.background && a.omit_claude_md && a.project && !a.plugin);
    assert_eq!(
        (
            a.isolation.as_deref(),
            a.initial_prompt.as_deref(),
            a.color.as_deref()
        ),
        (Some("worktree"), Some("/review"), Some("purple"))
    );
    assert_eq!(a.prompt.trim(), "Prompt.");
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn components_from_root_down_with_subfolders() {
    let d = std::env::temp_dir().join(format!("chanto-comp-{}", uuid::Uuid::new_v4().simple()));
    std::fs::create_dir_all(d.join(".git")).unwrap();
    let d = d.canonical().unwrap();
    let _home_lock = crate::compat::TEST_HOME_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    unsafe { std::env::set_var("CHANTO_COMPAT_HOME", d.join("home")) };
    let w = |p: &str, t: &str| {
        let p = d.join(p);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, t).unwrap();
    };
    w(
        ".claude/commands/frontend/test.md",
        "run the frontend tests",
    );
    w(
        ".claude/skills/s1/SKILL.md",
        "---\ndescription: far\n---\nfar",
    );
    w(
        "sub/.claude/skills/s1/SKILL.md",
        "---\ndescription: near\n---\nnear",
    );
    w(
        ".claude/skills/manual/SKILL.md",
        "---\ndescription: manual\ndisable-model-invocation: true\nuser-invocable: false\ncontext: fork\nagent: Explore\n---\nx",
    );
    w(
        ".claude/agents/review/reader.md",
        "---\nname: reader\ndescription: >\n  Reads\n  code\ntools:\n  - Read\n  - Grep\ndisallowedTools: [Bash]\nmodel: sonnet\n---\nRead only.",
    );
    w(
        ".claude/agents/broken.md",
        "---\nname: broken\ntools: [Read, Grep\n---\nx",
    );
    w(".claude/rules/backend/deep.md", "deep rule");
    let cwd = d.join("sub");
    let cmds = load_commands(&cwd);
    let c = cmds
        .iter()
        .find(|c| c.name == "frontend:test")
        .expect("subfolder command is namespaced");
    assert!(c.project);
    let skills = load_skills(&cwd);
    assert_eq!(
        skills.iter().find(|s| s.name == "s1").unwrap().description,
        "near",
        "nearer dir wins"
    );
    let m = skills.iter().find(|s| s.name == "manual").unwrap();
    assert!(!m.model_invocable && !m.user_invocable && m.project);
    assert_eq!(m.fork.as_deref(), Some("Explore"));
    let agents = load_agents(&cwd);
    let r = agents
        .iter()
        .find(|a| a.name == "reader")
        .expect("agents in subfolders load");
    assert_eq!(r.tools, Some(vec!["Read".to_string(), "Grep".to_string()]));
    assert_eq!(r.disallowed_tools, vec!["Bash".to_string()]);
    assert_eq!(r.description, "Reads code");
    assert_eq!(r.model.as_deref(), Some("sonnet"));
    let b = agents.iter().find(|a| a.name == "broken").unwrap();
    assert_eq!(b.tools, Some(vec![]), "unreadable tools fail closed");
    let all: String = instruction_files(&cwd)
        .into_iter()
        .map(|(_, t)| t)
        .collect();
    assert!(
        all.contains("deep rule"),
        "rules load recursively from the git root"
    );
    let _ = std::fs::remove_dir_all(&d);
}
