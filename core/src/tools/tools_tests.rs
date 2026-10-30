use super::*;
use crate::fs::MemFs;

fn workspace(fs: &MemFs) -> Workspace<'_> {
    Workspace::new(fs, "/p")
}

#[test]
fn read_shows_numbered_lines_and_says_what_is_missing() {
    let fs = MemFs::new();
    fs.insert("/p/src/main.rs", "fn main() {}\nfn other() {}\n");
    let mut w = workspace(&fs);
    let out = w.run("Read", &json!({"file_path": "src/main.rs"}));
    assert_eq!(out, ToolOutput::ok("1\tfn main() {}\n2\tfn other() {}\n"));
    let out = w.run(
        "Read",
        &json!({"file_path": "src/main.rs", "offset": 2, "limit": 1}),
    );
    assert_eq!(out.content, "2\tfn other() {}\n");
    let out = w.run("Read", &json!({"file_path": "src/mian.rs"}));
    assert!(
        out.is_error && out.content.contains("Did you mean one of: main.rs"),
        "{}",
        out.content
    );
    let out = w.run("Read", &json!({"file_path": "src"}));
    assert!(
        out.is_error && out.content.contains("is a directory"),
        "{}",
        out.content
    );
    fs.insert("/p/empty.txt", "");
    let out = w.run("Read", &json!({"file_path": "empty.txt"}));
    assert!(out.content.contains("File exists but is empty"));
}

#[test]
fn writing_needs_a_current_read() {
    let fs = MemFs::new();
    fs.insert("/p/a.txt", "one\n");
    let mut w = workspace(&fs);
    let out = w.run("Write", &json!({"file_path": "a.txt", "content": "two\n"}));
    assert!(
        out.is_error && out.content.starts_with("You must Read /p/a.txt"),
        "{}",
        out.content
    );
    w.run("Read", &json!({"file_path": "a.txt"}));
    fs.insert("/p/a.txt", "changed elsewhere\n");
    let out = w.run("Write", &json!({"file_path": "a.txt", "content": "two\n"}));
    assert!(
        out.is_error && out.content.contains("modified since you last read it"),
        "{}",
        out.content
    );
    w.run("Read", &json!({"file_path": "a.txt"}));
    let out = w.run("Write", &json!({"file_path": "a.txt", "content": "two\n"}));
    assert_eq!(out, ToolOutput::ok("File /p/a.txt overwritten (1 lines)."));
    let out = w.run(
        "Write",
        &json!({"file_path": "new/b.txt", "content": "x\ny\n"}),
    );
    assert_eq!(out, ToolOutput::ok("File created: /p/new/b.txt (2 lines)."));
    assert_eq!(fs.read(Path::new("/p/new/b.txt")).unwrap(), b"x\ny\n");
}

#[test]
fn edit_replaces_and_shows_the_result() {
    let fs = MemFs::new();
    fs.insert("/p/a.rs", "fn main() {\n    helo();\n}\n");
    let mut w = workspace(&fs);
    w.run("Read", &json!({"file_path": "a.rs"}));
    let out = w.run(
        "Edit",
        &json!({"file_path": "a.rs", "old_string": "helo", "new_string": "hello"}),
    );
    assert_eq!(
        out.content,
        "The file /p/a.rs has been updated (1 replacement). Snippet of the result:\n1\tfn main() {\n2\t    hello();\n3\t}\n"
    );
    assert_eq!(
        fs.read(Path::new("/p/a.rs")).unwrap(),
        b"fn main() {\n    hello();\n}\n"
    );
    let out = w.run(
        "Edit",
        &json!({"file_path": "a.rs", "old_string": "missing", "new_string": "x"}),
    );
    assert!(
        out.is_error && out.content.starts_with("old_string not found"),
        "{}",
        out.content
    );
    let out = w.run(
        "Edit",
        &json!({"file_path": "b.rs", "old_string": "", "new_string": "new\n"}),
    );
    assert_eq!(out, ToolOutput::ok("Created /p/b.rs"));
}

#[test]
fn search_tools_list_and_match() {
    let fs = MemFs::new();
    fs.insert("/p/.gitignore", "target/\n");
    fs.insert("/p/src/a.rs", "needle\n");
    fs.insert("/p/src/b.rs", "hay\n");
    fs.insert("/p/target/c.rs", "needle\n");
    let mut w = workspace(&fs);
    let lines = |out: ToolOutput| {
        let mut l: Vec<String> = out.content.lines().map(str::to_string).collect();
        l.sort();
        l
    };
    assert_eq!(
        lines(w.run("Glob", &json!({"pattern": "**/*.rs"}))),
        ["src/a.rs", "src/b.rs"]
    );
    assert_eq!(
        w.run("Glob", &json!({"pattern": "*.py"})).content,
        "No files found"
    );
    assert_eq!(
        w.run(
            "Grep",
            &json!({"pattern": "needle", "output_mode": "content"})
        )
        .content,
        "src/a.rs:1:needle"
    );
    assert_eq!(
        w.run("Grep", &json!({"pattern": "absent"})).content,
        "No matches found"
    );
    assert_eq!(
        w.run("Ls", &json!({})).content,
        "/p/\n  .gitignore\n  src/\n    a.rs\n    b.rs"
    );
    assert!(w.run("Nope", &json!({})).is_error);
}

#[test]
fn the_cli_and_the_core_describe_the_same_tools() {
    let names: Vec<String> = specs().into_iter().map(|s| s.name).collect();
    assert_eq!(names, ["Read", "Write", "Edit", "Glob", "Grep", "Ls"]);
}
