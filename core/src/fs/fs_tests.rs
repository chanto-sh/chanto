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
