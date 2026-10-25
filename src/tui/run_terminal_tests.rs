use super::*;
use ratatui::backend::{Backend, ClearType, TestBackend, WindowSize};
use ratatui::buffer::Cell;
use ratatui::layout::{Position, Size};
use std::convert::Infallible;

/// Models a terminal that cannot answer cursor queries.
struct NoCursorQuery(TestBackend);

impl Backend for NoCursorQuery {
    type Error = Infallible;

    fn draw<'a, I>(&mut self, content: I) -> Result<(), Self::Error>
    where
        I: Iterator<Item = (u16, u16, &'a Cell)>,
    {
        self.0.draw(content)
    }

    fn hide_cursor(&mut self) -> Result<(), Self::Error> {
        self.0.hide_cursor()
    }

    fn show_cursor(&mut self) -> Result<(), Self::Error> {
        self.0.show_cursor()
    }

    fn get_cursor_position(&mut self) -> Result<Position, Self::Error> {
        panic!("fullscreen rendering must not query the terminal cursor")
    }

    fn set_cursor_position<P: Into<Position>>(&mut self, position: P) -> Result<(), Self::Error> {
        self.0.set_cursor_position(position)
    }

    fn clear(&mut self) -> Result<(), Self::Error> {
        self.0.clear()
    }

    fn clear_region(&mut self, clear_type: ClearType) -> Result<(), Self::Error> {
        self.0.clear_region(clear_type)
    }

    fn size(&self) -> Result<Size, Self::Error> {
        self.0.size()
    }

    fn window_size(&mut self) -> Result<WindowSize, Self::Error> {
        self.0.window_size()
    }

    fn flush(&mut self) -> Result<(), Self::Error> {
        self.0.flush()
    }
}

#[test]
fn fullscreen_startup_and_redraw_work_without_cursor_queries() {
    let backend = NoCursorQuery(TestBackend::with_lines(["stale text", "          "]));
    let mut terminal = Terminal::new(backend).unwrap();
    clear_fullscreen(&mut terminal).unwrap();
    terminal
        .backend()
        .0
        .assert_buffer_lines(["          ", "          "]);

    let draw = |f: &mut ratatui::Frame| {
        f.render_widget(Paragraph::new("hello"), f.area());
    };
    terminal.draw(draw).unwrap();
    terminal
        .backend()
        .0
        .assert_buffer_lines(["hello     ", "          "]);

    // Focus restoration and editor return must repaint even unchanged cells.
    clear_fullscreen(&mut terminal).unwrap();
    terminal
        .backend()
        .0
        .assert_buffer_lines(["          ", "          "]);
    terminal.draw(draw).unwrap();
    terminal
        .backend()
        .0
        .assert_buffer_lines(["hello     ", "          "]);

    terminal.backend_mut().0.resize(12, 3);
    clear_fullscreen(&mut terminal).unwrap();
    terminal.draw(draw).unwrap();
    terminal
        .backend()
        .0
        .assert_buffer_lines(["hello       ", "            ", "            "]);
}
