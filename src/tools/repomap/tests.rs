use super::*;

fn write(dir: &Path, rel: &str, body: &str) {
    let p = dir.join(rel);
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(p, body).unwrap();
}

/// A fresh scratch directory (the test home and repo are thrown away).
fn tmp() -> PathBuf {
    let d = std::env::temp_dir().join(format!("chanto-repomap-{}", uuid::Uuid::new_v4().simple()));
    std::fs::create_dir_all(&d).unwrap();
    d
}

#[test]
fn a_name_defined_all_over_the_tree_does_not_dominate() {
    // `common` is referenced everywhere, but it is defined in every file,
    // so it says little; `Engine` is defined once and used in five places.
    let dir = tmp();
    write(&dir, "core.rs", "pub struct Engine {}\n");
    let mut refs = String::new();
    for i in 0..30 {
        write(&dir, &format!("g{i}.rs"), "pub fn common() {}\n");
        refs.push_str("fn u() { common(); }\n");
    }
    write(&dir, "users.rs", &refs);
    let mut used = String::new();
    for _ in 0..5 {
        used.push_str("fn e() { let _ = Engine; }\n");
    }
    write(&dir, "engine_users.rs", &used);
    let (text, _) = build(&dir, 4096).unwrap();
    let core = text.find("core.rs").expect("core.rs in map");
    let generic = text.find("g0.rs").expect("g0.rs in map");
    assert!(
        core < generic,
        "a name defined in every file must not outrank one defined once:\n{text}"
    );
}

#[test]
fn a_file_repeating_one_signature_says_it_once() {
    let dir = tmp();
    let body = "pub fn dup() {}\n".repeat(20);
    write(&dir, "rep.rs", &body);
    let (text, _) = build(&dir, 4096).unwrap();
    assert_eq!(text.matches("pub fn dup()").count(), 1, "{text}");
}

#[test]
fn ranks_the_most_referenced_symbol_first() {
    let dir = tmp();
    write(
        &dir,
        "src/core.rs",
        "pub struct Engine { n: usize }\n\npub fn helper() {}\n",
    );
    write(
        &dir,
        "src/a.rs",
        "fn a() { Engine::new(); Engine::go(); let e: Engine = todo!(); }\n",
    );
    write(&dir, "src/b.rs", "fn b() { let _ = Engine::new(); }\n");
    let (text, n) = build(&dir, 1024).unwrap();
    assert!(n >= 3, "{text}");
    // `Engine` is referenced across two files; `helper` is not.
    let engine = text.find("Engine").expect("Engine in map");
    let helper = text
        .find("helper")
        .unwrap_or_else(|| panic!("helper in map:\n{text}"));
    assert!(
        engine < helper,
        "most-referenced symbol should come first:\n{text}"
    );
    assert!(text.contains("⋮..."), "elisions are marked:\n{text}");
}

#[test]
fn understands_several_languages() {
    let dir = tmp();
    write(
        &dir,
        "app.py",
        "class Server:\n    def start(self):\n        pass\n",
    );
    write(&dir, "web.ts", "export function render() {}\n");
    write(&dir, "main.go", "func main() {}\ntype Config struct{}\n");
    write(&dir, "lib.rb", "module Lib\n  def go; end\nend\n");
    let (text, n) = build(&dir, 1024).unwrap();
    for name in ["Server", "start", "render", "main", "Config", "go"] {
        assert!(text.contains(name), "{name} missing from:\n{text}");
    }
    assert!(n >= 6);
}

#[test]
fn budget_trims_the_tail() {
    let dir = tmp();
    let mut body = String::new();
    for i in 0..200 {
        body.push_str(&format!("pub fn f{i}() {{}}\n"));
    }
    write(&dir, "big.rs", &body);
    let (small, _) = build(&dir, 64).unwrap();
    let (large, _) = build(&dir, 4096).unwrap();
    assert!(small.len() < large.len(), "budget must bound the output");
    assert!(small.lines().count() < 200);
}

#[test]
fn a_repo_without_known_languages_explains_itself() {
    let dir = tmp();
    write(&dir, "notes.txt", "just prose\n");
    let (text, n) = build(&dir, 1024).unwrap();
    assert_eq!(n, 0);
    assert!(text.contains("No symbols found"));
}

#[test]
fn ignores_gitignored_files() {
    let dir = tmp();
    // `ignore` honors .gitignore only inside a git repository.
    std::fs::create_dir_all(dir.join(".git")).unwrap();
    write(&dir, ".gitignore", "generated/\n");
    write(&dir, "generated/big.rs", "pub fn hidden() {}\n");
    write(&dir, "src/real.rs", "pub fn visible() {}\n");
    let (text, _) = build(&dir, 1024).unwrap();
    assert!(text.contains("visible"));
    assert!(!text.contains("hidden"), "{text}");
}

#[test]
fn counts_only_word_boundary_references() {
    let dir = tmp();
    write(&dir, "a.rs", "pub fn run() {}\n");
    // `running` must not count as a reference to `run`.
    write(&dir, "b.rs", "fn b() { running(); }\n");
    let (text, _) = build(&dir, 1024).unwrap();
    assert!(text.contains("run"), "{text}");
    assert!(text.contains("b.rs"));
}
