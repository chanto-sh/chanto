use super::*;
use std::io::Read;

#[test]
fn memory_files_and_directories() {
    let fs = MemFs::new();
    fs.insert("/p/src/a.rs", "fn a() {}\n");
    fs.mkdir_p("/p/empty");
    let a = fs.metadata(Path::new("/p/src/a.rs")).unwrap();
    assert!(a.is_file() && a.len == 10);
    assert!(fs.metadata(Path::new("/p/src")).unwrap().is_dir());
    assert!(fs.metadata(Path::new("/")).unwrap().is_dir());
    assert_eq!(
        fs.metadata(Path::new("/p/nope")).unwrap_err().kind(),
        io::ErrorKind::NotFound
    );
    let mut s = String::new();
    fs.open(Path::new("/p/src/a.rs"))
        .unwrap()
        .read_to_string(&mut s)
        .unwrap();
    assert_eq!(s, "fn a() {}\n");
    assert!(fs.read(Path::new("/p/src")).is_err());
    assert!(fs.read_dir(Path::new("/p/src/a.rs")).is_err());
    assert!(fs.read_dir(Path::new("/p/empty")).unwrap().is_empty());
    // A write moves the modification time on.
    fs.insert("/p/src/a.rs", "fn a() { 1 }\n");
    let later = fs.metadata(Path::new("/p/src/a.rs")).unwrap();
    assert!(later.modified > a.modified);
}

/// The same tree on disk and in memory reads the same.
// The disk: not in a WebAssembly build.
#[cfg(not(target_family = "wasm"))]
#[test]
fn memory_reads_like_the_disk() {
    let dir =
        std::env::temp_dir().join(format!("chanto-core-fs-{}", uuid::Uuid::new_v4().simple()));
    let mem = MemFs::new();
    for (p, text) in [
        ("src/a.rs", "a\n"),
        ("src/b/c.rs", "c\n"),
        ("README.md", ""),
    ] {
        let on_disk = dir.join(p);
        std::fs::create_dir_all(on_disk.parent().unwrap()).unwrap();
        std::fs::write(&on_disk, text).unwrap();
        mem.insert(&on_disk, text);
    }
    let fses: [&dyn Fs; 2] = [&NativeFs, &mem];
    let listing = |fs: &dyn Fs, p: &Path| {
        let mut v = fs.read_dir(p).unwrap();
        v.sort_by(|a, b| a.name.cmp(&b.name));
        v
    };
    for p in [dir.clone(), dir.join("src"), dir.join("src/b")] {
        assert_eq!(
            listing(fses[0], &p),
            listing(fses[1], &p),
            "{}",
            p.display()
        );
    }
    for p in ["src/a.rs", "src/b/c.rs", "README.md", "src", "missing"] {
        let p = dir.join(p);
        let [disk, memory] = fses.map(|fs| {
            fs.metadata(&p)
                // A directory's size is the platform's own.
                .map(|m| (m.kind, m.is_file().then_some(m.len)))
                .map_err(|e| e.kind())
        });
        assert_eq!(disk, memory, "{}", p.display());
        let [disk, memory] = fses.map(|fs| fs.read(&p).ok());
        assert_eq!(disk, memory, "{}", p.display());
    }
    let _ = std::fs::remove_dir_all(&dir);
}

// The disk: not in a WebAssembly build.
#[cfg(not(target_family = "wasm"))]
#[test]
fn atomic_write_keeps_the_original_on_failure() {
    let dir = std::env::temp_dir().join(format!("chanto-atomic-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let f = dir.join("a.rs");
    std::fs::write(&f, "one\n").unwrap();
    write_atomic(&f, b"two\n").unwrap();
    assert_eq!(std::fs::read_to_string(&f).unwrap(), "two\n");
    // Writes create missing parents (the Write tool's create_dir_all).
    let nested = dir.join("x/y/z.rs");
    write_atomic(&nested, b"deep\n").unwrap();
    assert_eq!(std::fs::read_to_string(&nested).unwrap(), "deep\n");
    // No temp files are left behind on the happy path.
    let leftovers: Vec<_> = std::fs::read_dir(&dir)
        .unwrap()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_name().to_string_lossy().ends_with(".tmp"))
        .collect();
    assert!(leftovers.is_empty(), "temp files left: {leftovers:?}");
    // A symlink is written through: it stays a link and its target changes.
    #[cfg(unix)]
    {
        std::fs::write(dir.join("AGENTS.md"), "rule one\n").unwrap();
        std::os::unix::fs::symlink("AGENTS.md", dir.join("CLAUDE.md")).unwrap();
        write_atomic(&dir.join("CLAUDE.md"), b"rule ONE\n").unwrap();
        assert!(
            std::fs::symlink_metadata(dir.join("CLAUDE.md"))
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_eq!(
            std::fs::read_to_string(dir.join("AGENTS.md")).unwrap(),
            "rule ONE\n"
        );
    }
    // A read-only file is refused, not replaced.
    let ro = dir.join("ro.txt");
    std::fs::write(&ro, "locked\n").unwrap();
    let mut perms = std::fs::metadata(&ro).unwrap().permissions();
    perms.set_readonly(true);
    std::fs::set_permissions(&ro, perms).unwrap();
    assert_eq!(
        write_atomic(&ro, b"changed\n").unwrap_err().kind(),
        std::io::ErrorKind::PermissionDenied
    );
    assert_eq!(std::fs::read_to_string(&ro).unwrap(), "locked\n");
    let _ = std::fs::remove_dir_all(&dir);
}
