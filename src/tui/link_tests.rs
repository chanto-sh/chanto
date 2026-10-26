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
fn colored_output_links_across_style_boundaries_without_changing_text() {
    let mut lines = markdown::ansi_lines(
        "\x1b[31merror:\x1b[0m `literal` https://exa\x1b[32mmple.com/report\x1b[0m now",
    );
    let original_style = lines[0].spans[0].style;
    for line in &mut lines {
        markdown::linkify_urls(line);
    }
    let text: String = lines[0].spans.iter().map(|s| s.content.as_ref()).collect();
    assert_eq!(text, "error: `literal` https://example.com/report now");
    assert_eq!(lines[0].spans[0].style, original_style);
    let targets = link_regions(&lines, 80);
    assert_eq!(targets.len(), 1);
    assert_eq!(targets[0].3, "https://example.com/report");
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
    assert_eq!(buf[(0, 0)].symbol(), "x");
}

#[test]
fn web_navigation_rejects_non_web_schemes() {
    let area = Rect::new(0, 0, 10, 1);
    for url in [
        "javascript:alert(1)",
        "file:///etc/passwd",
        "data:text/html,hello",
    ] {
        let mut buf = ratatui::buffer::Buffer::empty(area);
        buf.set_string(0, 0, "x", Style::default());
        paint_links(&mut buf, area, &[(0, 0, 1, url.into())]);
        assert_eq!(buf[(0, 0)].symbol(), "x", "{url}");
    }
}

#[tokio::test]
async fn a_click_opens_the_full_link_but_a_drag_remains_selection() {
    use super::app_tests::*;
    let mut t = test_app_with(|c| c.copy_on_select = Some(false), false);
    t.push(Kind::Assistant(
        "See [docs](https://example.com/docs).".into(),
    ));
    t.render(50, 24);
    let (row, start, _, url) = t.screen_links[0].clone();
    let event = |kind, column| crossterm::event::MouseEvent {
        kind,
        column,
        row,
        modifiers: KeyModifiers::NONE,
    };
    t.on_mouse(event(MouseEventKind::Down(MouseButton::Left), start));
    assert!(t.open_link.is_none(), "press alone never opens a link");
    t.on_mouse(event(MouseEventKind::Up(MouseButton::Left), start));
    assert_eq!(t.open_link.take().as_deref(), Some(url.as_str()));
    t.last_click = None;
    t.on_mouse(event(MouseEventKind::Down(MouseButton::Left), start));
    t.on_mouse(event(MouseEventKind::Drag(MouseButton::Left), start + 2));
    t.on_mouse(event(MouseEventKind::Up(MouseButton::Left), start + 2));
    assert!(t.open_link.is_none());
    t.help = true;
    assert!(t.web_link_at(row, start).is_none());
    t.help = false;
    t.popup = Some(Popup {
        items: vec![],
        sel: 0,
        kind: PopupKind::Slash,
    });
    assert!(t.web_link_at(row, start).is_none());
    t.popup = None;
    t.blocks.clear();
    t.render(50, 24);
    assert!(
        t.web_link_at(row, start).is_none(),
        "redraw clears stale targets"
    );
}

#[tokio::test]
async fn provider_notes_and_user_urls_are_clickable() {
    use super::app_tests::*;
    let mut t = test_app();
    t.push(Kind::Note(
        "Capacity details: https://example.com/status".into(),
        pal().error,
    ));
    t.push(Kind::User("https://example.com/report".into()));
    t.render(80, 24);
    for url in ["https://example.com/status", "https://example.com/report"] {
        assert!(t.screen_links.iter().any(|r| r.3 == url), "{url}");
    }
}

#[tokio::test]
async fn wrapped_link_cells_keep_the_full_destination() {
    use super::app_tests::*;
    let mut t = test_app();
    let url = "https://example.com/oauth/authorize?client_id=abcdefghijk&scope=read&state=long-state-value";
    t.push(Kind::Assistant(format!("Open {url} to continue.")));
    t.render(40, 30);
    assert!(t.screen_links.len() > 1);
    for (row, start, _, target) in &t.screen_links {
        assert_eq!(target, url);
        assert_eq!(t.web_link_at(*row, *start), Some(url));
    }
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
