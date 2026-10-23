use super::*;

#[test]
fn dropped_image_paths_become_tags() {
    let dir = std::env::temp_dir().join(format!("chanto-drop-{}", uuid::Uuid::new_v4().simple()));
    std::fs::create_dir_all(dir.join("My Shots")).unwrap();
    let a = dir.join("My Shots/Screen Shot 1.png");
    let b = dir.join("b.jpg");
    std::fs::write(&a, b"x").unwrap();
    std::fs::write(&b, b"x").unwrap();
    // How a terminal drops the path: `\ `-escaped on Unix, quoted on Windows.
    let esc = |p: &std::path::Path| {
        if cfg!(windows) {
            format!("\"{}\"", p.display())
        } else {
            p.display().to_string().replace(' ', "\\ ")
        }
    };
    assert_eq!(dropped_images(&esc(&a)), Some(vec![a.clone()]));
    assert_eq!(
        dropped_images(&format!("{} {}", esc(&a), esc(&b))),
        Some(vec![a.clone(), b.clone()])
    );
    assert_eq!(
        dropped_images(&format!("'{}'", a.display())),
        Some(vec![a.clone()])
    );
    // Text, non-image files and missing files are left alone.
    assert_eq!(dropped_images("fix the failing test"), None);
    std::fs::write(dir.join("notes.txt"), b"x").unwrap();
    assert_eq!(
        dropped_images(&dir.join("notes.txt").display().to_string()),
        None
    );
    assert_eq!(
        dropped_images(&format!("{} {}", esc(&a), dir.join("gone.png").display())),
        None
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn image_tags_become_paths_the_mention_expander_understands() {
    let a = std::path::PathBuf::from("/tmp/chanto-paste-1.png");
    let b = std::path::PathBuf::from("/tmp/chanto-paste-2.png");
    let out = expand_image_tags(
        "look at [Image #1] and [Image #2] please",
        &[a.clone(), b.clone()],
    );
    assert_eq!(
        out,
        format!("look at @{} and @{} please", a.display(), b.display())
    );
}

#[test]
fn an_image_without_a_tag_is_left_alone() {
    let a = std::path::PathBuf::from("/tmp/chanto-paste-1.png");
    assert_eq!(expand_image_tags("just text", &[a]), "just text");
}

#[test]
fn a_pasted_screenshot_is_copied_somewhere_durable() {
    // Mimic a dragged screenshot whose source file is about to be reaped.
    let src = std::env::temp_dir().join(format!("{}-shot.png", std::process::id()));
    std::fs::write(&src, b"\x89PNG\r\n\x1a\nfake").unwrap();
    let dest_dir = std::env::temp_dir().join(format!("chanto-attach-{}", std::process::id()));
    let kept = persist_attachment_into(&dest_dir, &src).expect("image should be persisted");
    assert!(kept.is_file(), "{kept:?} should exist");
    assert_ne!(kept, src, "the durable copy is a new path");
    // The bytes survive even after the original is gone.
    std::fs::remove_file(&src).unwrap();
    assert_eq!(std::fs::read(&kept).unwrap(), b"\x89PNG\r\n\x1a\nfake");
    let _ = std::fs::remove_dir_all(&dest_dir);
}

#[test]
fn non_images_and_missing_files_are_not_persisted() {
    let txt = std::env::temp_dir().join(format!("{}-note.txt", std::process::id()));
    std::fs::write(&txt, b"hello").unwrap();
    assert!(
        persist_attachment(&txt).is_none(),
        "text files stay in place"
    );
    let _ = std::fs::remove_file(&txt);
    assert!(persist_attachment(std::path::Path::new("/tmp/definitely-absent.png")).is_none());
}
