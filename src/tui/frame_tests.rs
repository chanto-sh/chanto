use super::*;

#[test]
fn the_default_frame_is_gap_free() {
    assert_eq!(frame_from(None), FrameStyle::Block);
    assert_eq!(frame_from(Some("block")), FrameStyle::Block);
    assert_eq!(frame_from(Some("nonsense")), FrameStyle::Block);
    assert_eq!(frame_from(Some("line")), FrameStyle::Line);
    assert_eq!(frame_from(Some("solid")), FrameStyle::Solid);
}

#[test]
fn frame_glyphs() {
    use ratatui::buffer::Buffer;
    use ratatui::widgets::Widget;
    let area = Rect::new(0, 0, 6, 3);
    // Block elements cover the part of the cell they draw.
    let mut buf = Buffer::empty(area);
    framed(pal().accent, FrameStyle::Block).render(area, &mut buf);
    assert_eq!(buf[(0, 0)].symbol(), "▁");
    assert_eq!(buf[(3, 0)].symbol(), "▁");
    assert_eq!(buf[(0, 1)].symbol(), "▏");
    assert_eq!(buf[(5, 1)].symbol(), "▕");
    assert_eq!(buf[(0, 2)].symbol(), "▔");
    let mut buf = Buffer::empty(area);
    framed(pal().accent, FrameStyle::Line).render(area, &mut buf);
    assert_eq!(buf[(0, 0)].symbol(), "╭");
    assert_eq!(buf[(0, 1)].symbol(), "│");
    let mut buf = Buffer::empty(area);
    framed(pal().accent, FrameStyle::Solid).render(area, &mut buf);
    assert_eq!(buf[(0, 1)].bg, pal().accent, "the whole cell is painted");
}
