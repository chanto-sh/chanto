use super::*;

fn lang(ext: &str) -> Language {
    language_for(Path::new(&format!("x.{ext}"))).unwrap()
}

fn skel(src: &str, ext: &str) -> String {
    skeleton("f", src.as_bytes(), lang(ext)).unwrap()
}

fn ctx_in(dir: &Path) -> Ctx {
    let env = crate::agent::tests::make_env(
        "http://localhost:1",
        dir,
        crate::permissions::Mode::Default,
        32768,
    );
    Ctx {
        env,
        cancel: tokio_util::sync::CancellationToken::new(),
        depth: 0,
        call_id: None,
        cwd_override: None,
        reads: None,
    }
}

#[test]
fn rust_skeleton_keeps_structure() {
    let s = skel(
        "use std::fmt;\npub struct Foo { x: u8 }\npub enum E { A }\n\
         pub trait T { fn m(&self); }\nimpl Foo {\n    pub fn new() -> Self { Foo { x: 0 } }\n}\n\
         pub fn free(x: u8) -> u8 { x }\nconst K: u8 = 1;\n",
        "rs",
    );
    assert!(s.contains("use std::fmt"), "{s}");
    assert!(s.contains("struct Foo"), "{s}");
    assert!(s.contains("enum E"), "{s}");
    assert!(s.contains("trait T"), "{s}");
    assert!(s.contains("impl Foo"), "{s}");
    assert!(s.contains("fn new()"), "{s}");
    assert!(s.contains("fn free(x: u8) -> u8"), "{s}");
    assert!(s.contains("const K: u8 = 1"), "{s}");
}

#[test]
fn python_skeleton_keeps_structure() {
    let s = skel(
        "import os\nclass C(Base):\n    def m(self, x):\n        return x\n\
         @decorator\ndef f(a, b=1):\n    return a\nX = 1\n",
        "py",
    );
    assert!(s.contains("import os"), "{s}");
    assert!(s.contains("class C"), "{s}");
    assert!(s.contains("fn m(self, x)"), "{s}");
    assert!(s.contains("fn f(a, b=1)"), "{s}");
    assert!(s.contains("X = 1"), "{s}");
}

#[test]
fn javascript_skeleton_keeps_structure() {
    let s = skel(
        "import x from 'y';\nfunction f(a) { return a; }\n\
         class K { m() {} }\nexport const z = 1;\n",
        "js",
    );
    assert!(s.contains("import x"), "{s}");
    assert!(s.contains("fn f(a)"), "{s}");
    assert!(s.contains("class K"), "{s}");
    assert!(s.contains("export const z"), "{s}");
}

#[test]
fn typescript_skeleton_keeps_structure() {
    let s = skel(
        "interface I { m(): void }\ntype A = string;\nenum E { X }\n\
         function f(c: I): void {}\n",
        "ts",
    );
    assert!(s.contains("interface I"), "{s}");
    assert!(s.contains("fn m() -> void"), "{s}");
    assert!(s.contains("type A"), "{s}");
    assert!(s.contains("enum E"), "{s}");
    assert!(s.contains("fn f(c: I) -> void"), "{s}");
}

#[test]
fn go_skeleton_keeps_structure() {
    let s = skel(
        "package main\nimport \"fmt\"\ntype S struct { X int }\n\
         func (s *S) M(a int) error { return nil }\nfunc F() {}\n",
        "go",
    );
    assert!(s.contains("import \"fmt\""), "{s}");
    assert!(s.contains("type S"), "{s}");
    assert!(s.contains("fn M(a int)"), "{s}");
    assert!(s.contains("fn F()"), "{s}");
}

#[test]
fn regex_fallback_covers_other_languages() {
    let s = regex_skeleton(
        "L.java",
        b"public class L {\n  private int c;\n}\ninterface I {}\nenum E { A }\n",
    );
    assert!(s.contains("# L.java"), "{s}");
    assert!(s.contains("class L"), "{s}");
    assert!(s.contains("interface I"), "{s}");
    assert!(s.contains("enum E"), "{s}");
}

#[test]
fn language_mapping_and_support() {
    for ext in ["rs", "py", "js", "jsx", "mjs", "ts", "tsx", "mts"] {
        let p = Path::new("x").with_extension(ext);
        assert!(language_for(&p).is_some(), "{ext} should parse");
        assert!(supported(&p), "{ext} should be supported");
    }
    for ext in ["java", "kt", "cpp", "rb"] {
        let p = Path::new("x").with_extension(ext);
        assert!(language_for(&p).is_none(), "{ext} uses the fallback");
        assert!(supported(&p), "{ext} should still be supported");
    }
    assert!(!supported(Path::new("x.txt")));
}

#[test]
fn truncate_short_and_long() {
    assert_eq!(truncate("abc", 5), "abc");
    assert_eq!(truncate("abcdef", 5), "ab...");
}

#[tokio::test]
async fn run_indexes_a_directory_and_respects_the_budget() {
    let dir = std::env::temp_dir().join(format!("chanto-index-{}", uuid::Uuid::new_v4().simple()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("a.rs"), "pub fn a() {}\n").unwrap();
    std::fs::write(dir.join("b.py"), "def b():\n    pass\n").unwrap();
    std::fs::write(dir.join("c.java"), "public class C {}\n").unwrap();
    std::fs::write(dir.join("d.txt"), "ignored\n").unwrap();
    let ctx = ctx_in(&dir);

    let out = Index.run(json!({"path": "."}), &ctx).await.unwrap();
    assert!(out.content.contains("fn a()"), "{}", out.content);
    assert!(out.content.contains("fn b()"), "{}", out.content);
    assert!(out.content.contains("class C"), "{}", out.content);
    assert!(!out.content.contains("d.txt"), "{}", out.content);

    let tiny = Index
        .run(json!({"path": ".", "tokens": 1}), &ctx)
        .await
        .unwrap();
    assert!(tiny.content.contains("token budget"), "{}", tiny.content);

    let none = Index.run(json!({"path": "d.txt"}), &ctx).await.unwrap();
    assert!(
        none.content.contains("Could not parse")
            || none.content.contains("No supported source files"),
        "{}",
        none.content
    );

    let _ = std::fs::remove_dir_all(&dir);
}
