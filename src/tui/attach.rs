//! Attachments: images from the clipboard or a drag-and-drop, saved to the
//! session's attachments directory, and how they appear in the prompt.

/// Save an image from the system clipboard to a temp file (macOS / Wayland / X11).
pub(super) fn clipboard_image() -> Option<std::path::PathBuf> {
    // Keep clipboard images in the durable attachments dir too, so the model can
    // still read them later in the turn rather than racing the temp cleaner.
    let dir = attachments_dir();
    let stem = format!(
        "chanto-paste-{}",
        &uuid::Uuid::new_v4().simple().to_string()[..8]
    );
    let path = dir.join(format!("{stem}.png"));
    if cfg!(target_os = "macos") {
        // Screenshots usually sit on the clipboard as TIFF; other apps expose a
        // PNG flavour. Try PNG first, then TIFF converted through `sips`.
        if !macos_clipboard_flavour(&path, "PNGf") {
            let tiff = dir.join(format!("{stem}.tiff"));
            if !macos_clipboard_flavour(&tiff, "TIFF") {
                let _ = std::fs::remove_file(&path);
                return None;
            }
            let conv = std::process::Command::new("sips")
                .args([
                    "-s",
                    "format",
                    "png",
                    &tiff.to_string_lossy(),
                    "--out",
                    &path.to_string_lossy(),
                ])
                .output();
            let _ = std::fs::remove_file(&tiff);
            if conv.map(|o| !o.status.success()).unwrap_or(true) || !file_nonempty(&path) {
                let _ = std::fs::remove_file(&path);
                return None;
            }
        }
    }
    #[cfg(windows)]
    {
        // The clipboard image is whatever format Windows gave us; let
        // System.Drawing convert it to a PNG on disk.
        let script = format!(
            "Add-Type -AssemblyName System.Windows.Forms,System.Drawing; \
             $i=[System.Windows.Forms.Clipboard]::GetImage(); \
             if ($i) {{ $i.Save('{}', [System.Drawing.Imaging.ImageFormat]::Png) }}",
            path.display().to_string().replace('\'', "''")
        );
        let ok = std::process::Command::new("powershell")
            .args(["-NoProfile", "-Command", &script])
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if !ok || !file_nonempty(&path) {
            let _ = std::fs::remove_file(&path);
            return None;
        }
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        let out = std::process::Command::new("wl-paste")
            .args(["--type", "image/png"])
            .output()
            .or_else(|_| {
                std::process::Command::new("xclip")
                    .args(["-selection", "clipboard", "-t", "image/png", "-o"])
                    .output()
            })
            .ok()?;
        std::fs::write(&path, out.stdout).ok()?;
        if !file_nonempty(&path) {
            let _ = std::fs::remove_file(&path);
            return None;
        }
    }
    file_nonempty(&path).then_some(path)
}

pub(super) fn file_nonempty(p: &std::path::Path) -> bool {
    std::fs::metadata(p).map(|m| m.len() > 0).unwrap_or(false)
}

/// Where pasted images are kept between paste and submit.
pub(super) fn attachments_dir() -> std::path::PathBuf {
    let d = crate::config::home_dir().join("attachments");
    let _ = std::fs::create_dir_all(&d);
    d
}

/// The image files in a paste or drag-and-drop: one path, or several separated
/// by spaces (Unix terminals escape spaces inside a dropped path as `\ `),
/// possibly quoted. On Windows a backslash is the path separator, never an
/// escape; terminals there quote paths with spaces instead. None unless every
/// item is an existing image file.
/// The `[Pasted text #n +N lines]` tag the cursor is on or right after:
/// its byte range and `n`.
pub(super) fn paste_tag_at(text: &str, cursor: usize) -> Option<(usize, usize, usize)> {
    static RE: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
        regex::Regex::new(r"\[Pasted text #(\d+) \+\d+ lines\]").unwrap()
    });
    RE.captures_iter(text).find_map(|c| {
        let m = c.get(0)?;
        if m.start() <= cursor && cursor <= m.end() {
            Some((m.start(), m.end(), c[1].parse().ok().filter(|n| *n > 0)?))
        } else {
            None
        }
    })
}

pub(super) fn dropped_images(s: &str) -> Option<Vec<std::path::PathBuf>> {
    let s = s.trim();
    if s.is_empty() || s.contains('\n') {
        return None;
    }
    let mut items = Vec::new();
    let mut cur = String::new();
    let mut quote: Option<char> = None;
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        match (c, quote) {
            ('\\', None) if !cfg!(windows) => {
                if let Some(n) = chars.next() {
                    cur.push(n);
                }
            }
            ('\'' | '"', None) => quote = Some(c),
            (q, Some(open)) if q == open => quote = None,
            (' ', None) => {
                if !cur.is_empty() {
                    items.push(std::mem::take(&mut cur));
                }
            }
            _ => cur.push(c),
        }
    }
    if !cur.is_empty() {
        items.push(cur);
    }
    let paths: Vec<std::path::PathBuf> = items.into_iter().map(std::path::PathBuf::from).collect();
    (!paths.is_empty()
        && paths
            .iter()
            .all(|p| crate::util::is_image_path(p) && p.is_file()))
    .then_some(paths)
}

/// Copy a pasted image into our own directory before we lose it.
///
/// A screenshot dragged from the macOS capture UI lives under a `TemporaryItems`
/// directory that the system deletes as soon as the capture UI closes, so a path
/// inserted into the prompt is often already gone by the time it is submitted —
/// the image then silently vanishes. Copying the bytes now keeps the attachment
/// alive for the whole turn (and for the model to read it).
pub(super) fn persist_attachment(path: &std::path::Path) -> Option<std::path::PathBuf> {
    persist_attachment_into(&attachments_dir(), path)
}

/// Like [`persist_attachment`], but into an explicit directory (used by tests).
pub(super) fn persist_attachment_into(
    dir: &std::path::Path,
    path: &std::path::Path,
) -> Option<std::path::PathBuf> {
    if !crate::util::is_image_path(path) || !path.is_file() {
        return None;
    }
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("png")
        .to_ascii_lowercase();
    let name = format!("{}.{ext}", &uuid::Uuid::new_v4().simple().to_string()[..12]);
    std::fs::create_dir_all(dir).ok()?;
    let dest = dir.join(name);
    std::fs::copy(path, &dest).ok()?;
    Some(dest)
}

/// macOS: write the clipboard's `flavour` (`PNGf`/`TIFF`) to `dest`. Returns
/// whether a non-empty file was produced.
pub(super) fn macos_clipboard_flavour(dest: &std::path::Path, flavour: &str) -> bool {
    let _ = std::fs::remove_file(dest);
    let script = format!(
        "set d to (the clipboard as «class {flavour}»)\nset f to (open for access POSIX file \"{}\" with write permission)\nwrite d to f\nclose access f",
        dest.display()
    );
    let ok = std::process::Command::new("osascript")
        .arg("-e")
        .arg(script)
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    ok && file_nonempty(dest)
}

/// Replace `[Image #N]` placeholder tags with the `@path` the mention expander
/// understands, so the model gets the image while the transcript stays readable.
pub(super) fn expand_image_tags(text: &str, images: &[std::path::PathBuf]) -> String {
    let mut out = text.to_string();
    for (i, p) in images.iter().enumerate() {
        out = out.replace(&format!("[Image #{}]", i + 1), &format!("@{}", p.display()));
    }
    out
}
