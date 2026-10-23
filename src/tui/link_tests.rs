use super::*;

fn lines_for(md: &str, width: u16) -> Vec<Line<'static>> {
    markdown::render_markdown(md)
        .into_iter()
        .flat_map(|l| markdown::wrap(&l.line, width as usize, l.indent))
        .collect()
}

#[test]
fn a_wrapped_url_is_one_link() {
    let url = "https://dash.cloudflare.com/oauth2/auth?response_type=code&client_id=abc&redirect_uri=http%3A%2F%2Flocalhost%3A8762%2Foauth%2Fcallback&scope=read";
    let lines = lines_for(url, 40);
    assert!(lines.len() > 2);
    let regions = link_regions(&lines, 40);
    // Every visible fragment maps to the full URL: clicking any part opens
    // the same destination.
    let covered: u16 = regions.iter().map(|(_, s, e, _)| e - s).sum();
    let total: u16 = lines.iter().map(|l| l.width() as u16).sum();
    assert_eq!(covered, total);
    assert!(regions.iter().all(|r| r.3 == url), "{regions:?}");
}

#[test]
fn a_fragment_cut_at_the_viewport_edge_is_not_linked() {
    let url = "https://example.com/aaaa?bbbb=cccc&dddd=eeee&ffff=gggg";
    let lines = lines_for(url, 20);
    // Only the last row is hidden: the first one fills the width and must
    // continue below, so opening it would use a truncated URL.
    let shown = &lines[..1];
    assert_eq!(shown[0].width(), 20);
    assert!(link_regions(shown, 20).is_empty());
}

#[test]
fn a_markdown_link_links_its_label_and_the_url_shown() {
    let lines = lines_for("see [docs](https://example.com/x) now", 80);
    let regions = link_regions(&lines, 80);
    assert_eq!(regions.len(), 2, "{regions:?}");
    assert!(regions.iter().all(|r| r.3 == "https://example.com/x"));
}

#[test]
fn two_urls_on_consecutive_lines_stay_separate() {
    let mut lines = lines_for("https://a.example/x\n\nhttps://b.example/y", 80);
    lines.retain(|l| l.width() > 0);
    assert_eq!(lines.len(), 2);
    let regions = link_regions(&lines, 80);
    assert_eq!(regions.len(), 2, "{regions:?}");
    assert_eq!(regions[0].3, "https://a.example/x");
    assert_eq!(regions[1].3, "https://b.example/y");
}

#[test]
fn painting_a_link_wraps_each_cell_in_osc8() {
    let mut buf = ratatui::buffer::Buffer::empty(Rect::new(0, 0, 20, 1));
    buf.set_string(0, 0, "hello", Style::default());
    paint_links(
        &mut buf,
        Rect::new(0, 0, 20, 1),
        &[(0, 0, 2, "https://x.test".to_string())],
    );
    assert_eq!(
        buf[(0, 0)].symbol(),
        "\x1b]8;;https://x.test\x1b\\h\x1b]8;;\x1b\\"
    );
    assert_eq!(
        buf[(1, 0)].symbol(),
        "\x1b]8;;https://x.test\x1b\\e\x1b]8;;\x1b\\"
    );
    assert_eq!(buf[(2, 0)].symbol(), "l");
}

#[test]
fn a_link_url_cannot_carry_control_characters() {
    let area = Rect::new(0, 0, 10, 1);
    let mut buf = ratatui::buffer::Buffer::empty(area);
    buf.set_string(0, 0, "x", Style::default());
    paint_links(
        &mut buf,
        area,
        &[(0, 0, 1, "https://x\x1b]0;evil\x07".to_string())],
    );
    assert_eq!(
        buf[(0, 0)].symbol(),
        "\x1b]8;;https://x]0;evil\x1b\\x\x1b]8;;\x1b\\"
    );
}

#[test]
fn the_backend_writes_the_hyperlink_escape() {
    #[derive(Clone)]
    struct Shared(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);
    impl std::io::Write for Shared {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let written = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let mut backend = CrosstermBackend::new(Shared(written.clone()));
    let area = Rect::new(0, 0, 40, 1);
    let mut buf = ratatui::buffer::Buffer::empty(area);
    buf.set_string(0, 0, "https://example.com/x", Style::default());
    paint_links(
        &mut buf,
        area,
        &[(0, 0, 21, "https://example.com/x".to_string())],
    );
    let prev = ratatui::buffer::Buffer::empty(area);
    let diff = prev.diff(&buf);
    ratatui::backend::Backend::draw(&mut backend, diff.into_iter()).unwrap();
    ratatui::backend::Backend::flush(&mut backend).unwrap();
    let out = String::from_utf8(written.lock().unwrap().clone()).unwrap();
    assert!(
        out.contains("\x1b]8;;https://example.com/x\x1b\\h"),
        "{out:?}"
    );
    assert_eq!(out.matches("\x1b]8;;\x1b\\").count(), 21);
}
