//! Multiline input editor with history.

use unicode_width::UnicodeWidthChar;

#[derive(Default)]
pub struct Input {
    pub text: String,
    /// Byte offset of the cursor.
    pub cursor: usize,
    history: Vec<String>,
    hist_pos: Option<usize>,
    draft: String,
}

impl Input {
    pub fn with_history(history: Vec<String>) -> Input {
        Input {
            history,
            ..Default::default()
        }
    }

    pub fn is_empty(&self) -> bool {
        self.text.is_empty()
    }

    pub fn history(&self) -> &[String] {
        &self.history
    }

    pub fn set(&mut self, s: &str) {
        self.text = s.to_string();
        self.cursor = self.text.len();
    }

    pub fn take(&mut self) -> String {
        let t = std::mem::take(&mut self.text);
        self.cursor = 0;
        self.hist_pos = None;
        if !t.trim().is_empty() && self.history.last() != Some(&t) {
            self.history.push(t.clone());
        }
        t
    }

    pub fn insert(&mut self, s: &str) {
        let s = s.replace("\r\n", "\n").replace('\r', "\n");
        self.text.insert_str(self.cursor, &s);
        self.cursor += s.len();
    }

    pub fn insert_char(&mut self, c: char) {
        self.text.insert(self.cursor, c);
        self.cursor += c.len_utf8();
    }

    fn prev_boundary(&self, i: usize) -> usize {
        self.text[..i]
            .char_indices()
            .next_back()
            .map(|(j, _)| j)
            .unwrap_or(0)
    }

    fn next_boundary(&self, i: usize) -> usize {
        self.text[i..]
            .chars()
            .next()
            .map(|c| i + c.len_utf8())
            .unwrap_or(i)
    }

    pub fn backspace(&mut self) {
        if self.cursor > 0 {
            let p = self.prev_boundary(self.cursor);
            self.text.replace_range(p..self.cursor, "");
            self.cursor = p;
        }
    }

    pub fn delete(&mut self) {
        if self.cursor < self.text.len() {
            let n = self.next_boundary(self.cursor);
            self.text.replace_range(self.cursor..n, "");
        }
    }

    pub fn left(&mut self) {
        self.cursor = self.prev_boundary(self.cursor);
    }

    pub fn right(&mut self) {
        self.cursor = self.next_boundary(self.cursor);
    }

    pub fn word_left(&mut self) {
        let before = &self.text[..self.cursor];
        let trimmed = before.trim_end();
        self.cursor = trimmed
            .char_indices()
            .rev()
            .find(|(_, c)| c.is_whitespace() || "/.,:;()[]{}".contains(*c))
            .map(|(i, c)| i + c.len_utf8())
            .unwrap_or(0);
        if self.cursor == before.len() && self.cursor > 0 {
            self.left();
        }
    }

    pub fn word_right(&mut self) {
        let after = &self.text[self.cursor..];
        let skip_ws = after.len() - after.trim_start().len();
        let rest = &after[skip_ws..];
        let n = rest
            .find(|c: char| c.is_whitespace() || "/.,:;()[]{}".contains(c))
            .unwrap_or(rest.len());
        let n = if n == 0 {
            rest.chars().next().map(|c| c.len_utf8()).unwrap_or(0)
        } else {
            n
        };
        self.cursor += skip_ws + n;
    }

    pub fn delete_word_back(&mut self) {
        let end = self.cursor;
        self.word_left();
        self.text.replace_range(self.cursor..end, "");
    }

    fn line_start(&self) -> usize {
        self.text[..self.cursor]
            .rfind('\n')
            .map(|i| i + 1)
            .unwrap_or(0)
    }

    fn line_end(&self) -> usize {
        self.text[self.cursor..]
            .find('\n')
            .map(|i| self.cursor + i)
            .unwrap_or(self.text.len())
    }

    pub fn home(&mut self) {
        self.cursor = self.line_start();
    }

    pub fn end(&mut self) {
        self.cursor = self.line_end();
    }

    pub fn kill_to_start(&mut self) {
        let s = self.line_start();
        self.text.replace_range(s..self.cursor, "");
        self.cursor = s;
    }

    pub fn kill_to_end(&mut self) {
        let e = self.line_end();
        if e == self.cursor && e < self.text.len() {
            self.text.remove(e);
        } else {
            self.text.replace_range(self.cursor..e, "");
        }
    }

    pub fn on_first_line(&self) -> bool {
        !self.text[..self.cursor].contains('\n')
    }

    pub fn on_last_line(&self) -> bool {
        !self.text[self.cursor..].contains('\n')
    }

    pub fn up(&mut self) -> bool {
        if self.on_first_line() {
            return self.history_prev();
        }
        let col = self.text[self.line_start()..self.cursor].chars().count();
        let ls = self.line_start();
        let prev_start = self.text[..ls - 1].rfind('\n').map(|i| i + 1).unwrap_or(0);
        let prev_line = &self.text[prev_start..ls - 1];
        let off: usize = prev_line.chars().take(col).map(|c| c.len_utf8()).sum();
        self.cursor = prev_start + off;
        true
    }

    pub fn down(&mut self) -> bool {
        if self.on_last_line() {
            return self.history_next();
        }
        let col = self.text[self.line_start()..self.cursor].chars().count();
        let next_start = self.line_end() + 1;
        let next_end = self.text[next_start..]
            .find('\n')
            .map(|i| next_start + i)
            .unwrap_or(self.text.len());
        let off: usize = self.text[next_start..next_end]
            .chars()
            .take(col)
            .map(|c| c.len_utf8())
            .sum();
        self.cursor = next_start + off;
        true
    }

    /// The text is a history entry recalled unchanged (up/down), not typed.
    pub fn recalled(&self) -> bool {
        self.hist_pos
            .is_some_and(|p| self.history.get(p) == Some(&self.text))
    }

    fn history_prev(&mut self) -> bool {
        if self.history.is_empty() {
            return false;
        }
        let pos = match self.hist_pos {
            None => {
                self.draft = self.text.clone();
                self.history.len() - 1
            }
            Some(0) => return false,
            Some(p) => p - 1,
        };
        self.hist_pos = Some(pos);
        self.text = self.history[pos].clone();
        self.cursor = self.text.find('\n').unwrap_or(self.text.len());
        true
    }

    fn history_next(&mut self) -> bool {
        let Some(p) = self.hist_pos else { return false };
        if p + 1 >= self.history.len() {
            self.hist_pos = None;
            self.text = std::mem::take(&mut self.draft);
        } else {
            self.hist_pos = Some(p + 1);
            self.text = self.history[p + 1].clone();
        }
        self.cursor = self.text.len();
        true
    }

    /// The word under/before the cursor (for completion), with its start offset.
    pub fn current_word(&self) -> (usize, &str) {
        let before = &self.text[..self.cursor];
        let start = before
            .char_indices()
            .rev()
            .find(|(_, c)| c.is_whitespace())
            .map(|(i, c)| i + c.len_utf8())
            .unwrap_or(0);
        (start, &self.text[start..self.cursor])
    }

    pub fn replace_current_word(&mut self, with: &str) {
        let (start, _) = self.current_word();
        self.text.replace_range(start..self.cursor, with);
        self.cursor = start + with.len();
    }

    /// Lay out the text into rows of `width` columns (prefix excluded). Long
    /// lines wrap at a word boundary when one is available: a word that would
    /// overflow starts the next row, and the spaces before it are dropped. A
    /// single word wider than the row still breaks on the character, so nothing
    /// is ever lost. Returns rows and the (row, col) of the cursor.
    pub fn layout(&self, width: usize) -> (Vec<String>, (usize, usize)) {
        let width = width.max(4);
        let text = &self.text;
        let chars: Vec<(usize, char)> = text.char_indices().collect();
        let mut rows: Vec<String> = vec![String::new()];
        let mut col = 0usize;
        let mut cursor_pos: Option<(usize, usize)> = None;
        let mut k = 0usize;
        while k < chars.len() {
            let (i, c) = chars[k];
            if c == '\n' {
                if i == self.cursor {
                    cursor_pos = Some((rows.len() - 1, col));
                }
                rows.push(String::new());
                col = 0;
                k += 1;
                continue;
            }
            if c.is_whitespace() {
                if i == self.cursor {
                    cursor_pos = Some((rows.len() - 1, col));
                }
                rows.last_mut().unwrap().push(c);
                col += c.width().unwrap_or(0);
                k += 1;
                continue;
            }
            // A word: a run of non-space characters. Measure it first so a wrap
            // can happen before it, at the boundary, rather than inside it.
            let start = k;
            let mut end = k;
            let mut word = 0usize;
            while end < chars.len() && !chars[end].1.is_whitespace() {
                word += chars[end].1.width().unwrap_or(0);
                end += 1;
            }
            if col > 0 && col + word > width {
                // Drop the spaces that would trail the row, then break.
                let row = rows.last_mut().unwrap();
                while row.ends_with(' ') {
                    row.pop();
                }
                rows.push(String::new());
                col = 0;
            }
            for &(ci, wc) in &chars[start..end] {
                let w = wc.width().unwrap_or(0);
                if col > 0 && col + w > width {
                    rows.push(String::new());
                    col = 0;
                }
                if ci == self.cursor {
                    cursor_pos = Some((rows.len() - 1, col));
                }
                rows.last_mut().unwrap().push(wc);
                col += w;
            }
            k = end;
        }
        if self.cursor >= text.len() {
            if col >= width {
                rows.push(String::new());
                col = 0;
            }
            cursor_pos = Some((rows.len() - 1, col));
        }
        let cur = cursor_pos.unwrap_or((rows.len() - 1, col));
        (rows, cur)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn editing_and_history() {
        let mut i = Input::default();
        i.insert("hello world");
        i.word_left();
        assert_eq!(&i.text[i.cursor..], "world");
        i.delete_word_back();
        assert_eq!(i.text, "world");
        i.set("first");
        i.take();
        i.insert("draft");
        assert!(i.up());
        assert_eq!(i.text, "first");
        assert!(i.down());
        assert_eq!(i.text, "draft");
    }

    #[test]
    fn unicode_whitespace_is_safe() {
        let mut i = Input::default();
        i.insert("a\u{00A0}bé");
        let _ = i.current_word();
        i.word_left();
        i.word_right();
        i.delete_word_back();
        i.insert("\u{00A0}x");
        let (_, w) = i.current_word();
        assert_eq!(w, "x");
    }

    #[test]
    fn multiline_navigation() {
        let mut i = Input::default();
        i.insert("abc\ndefgh");
        assert!(i.up());
        assert_eq!(i.cursor, 3);
        let (rows, cur) = i.layout(80);
        assert_eq!(rows.len(), 2);
        assert_eq!(cur, (0, 3));
    }

    #[test]
    fn long_lines_wrap_at_a_word_boundary() {
        let mut i = Input::default();
        i.insert("hello world");
        // The space is dropped and "world" starts the next row.
        let (rows, _) = i.layout(8);
        assert_eq!(rows, vec!["hello".to_string(), "world".to_string()]);
        // The cursor follows the last row.
        let (_, cur) = i.layout(8);
        assert_eq!(cur, (1, 5));
        // A word too long for the row still breaks on the character.
        let mut j = Input::default();
        j.insert("abcdefgh");
        let (rows, _) = j.layout(5);
        assert_eq!(rows, vec!["abcde".to_string(), "fgh".to_string()]);
        // An explicit newline still wins over wrapping.
        let mut k = Input::default();
        k.insert("a\nb");
        let (rows, _) = k.layout(80);
        assert_eq!(rows, vec!["a".to_string(), "b".to_string()]);
    }
}
