//! Finding link targets in rendered rows and making their cells OSC 8
//! hyperlinks, so a terminal can open them on cmd/ctrl+click.

use super::*;

/// Only explicit web navigation is opened from untrusted transcript text.
fn web_target(url: &str) -> Option<&str> {
    if url.chars().any(|c| c.is_control() || c.is_whitespace()) {
        return None;
    }
    let parsed = reqwest::Url::parse(url).ok()?;
    (matches!(parsed.scheme(), "http" | "https") && parsed.host_str().is_some()).then_some(url)
}

impl App {
    pub(super) fn paint_screen_links(
        &mut self,
        buf: &mut ratatui::buffer::Buffer,
        area: Rect,
        regions: &[(u16, u16, u16, String)],
    ) {
        if self.help {
            return;
        }
        for (row, start, end, url) in regions {
            if *row < area.height && *start < area.width && web_target(url).is_some() {
                self.screen_links.push((
                    area.y + *row,
                    area.x + *start,
                    area.x + (*end).min(area.width),
                    url.clone(),
                ));
            }
        }
        paint_links(buf, area, regions);
    }

    pub(super) fn web_link_at(&self, row: u16, col: u16) -> Option<&str> {
        if self.help || self.popup.is_some() {
            return None;
        }
        self.screen_link_at(row, col)
            .map(|(_, _, _, url)| url.as_str())
    }

    /// The link's `(row, first column, one past the last, url)` under a cell.
    pub(super) fn screen_link_at(&self, row: u16, col: u16) -> Option<&(u16, u16, u16, String)> {
        self.screen_links
            .iter()
            .rev()
            .find(|(y, start, end, _)| row == *y && (*start..*end).contains(&col))
    }
}

/// Cells covered by one link: `(row, first column, one past the last)`.
type LinkCells = Vec<(u16, u16, u16)>;

/// A link being stitched back together across wrapped rows: its URL so far
/// and the cells it covers.
type LinkChain = (String, LinkCells);

/// Link targets among rendered rows: `(row, first column, one past the last,
/// url)`. A URL too long for a line was wrapped at its separators, so its
/// fragments are stitched back together; a fragment that fills its line may
/// have been cut off below the viewport, and is left unlinked rather than
/// opening a truncated URL.
pub(super) fn link_regions(lines: &[Line<'static>], width: u16) -> Vec<(u16, u16, u16, String)> {
    struct Row {
        first: usize,
        last: usize,
        runs: Vec<markdown::LinkRun>,
    }
    let rows: Vec<Row> = lines
        .iter()
        .map(|l| {
            let (first, last) = content_bounds(l);
            Row {
                first,
                last,
                runs: markdown::link_runs(l),
            }
        })
        .collect();
    let mut out: Vec<(u16, u16, u16, String)> = Vec::new();
    let mut chain: Option<LinkChain> = None;
    let flush = |chain: &mut Option<LinkChain>, out: &mut Vec<(u16, u16, u16, String)>| {
        let Some((text, cells)) = chain.take() else {
            return;
        };
        let complete = text.starts_with("http://") || text.starts_with("https://");
        // A final fragment reaching the last column may continue on a row the
        // viewport cut off.
        let cut = cells.last().is_some_and(|c| c.2 as usize >= width as usize);
        if complete && !cut {
            out.extend(
                cells
                    .into_iter()
                    .map(|(row, start, end)| (row, start, end, text.clone())),
            );
        }
    };
    for (i, row) in rows.iter().enumerate() {
        // The row above ended in a link, and this one opens with the rest of it.
        let continues_above = i > 0
            && rows[i - 1]
                .runs
                .last()
                .is_some_and(|r| r.end == rows[i - 1].last)
            && row.runs.first().is_some_and(|r| r.start == row.first);
        for (k, run) in row.runs.iter().enumerate() {
            let continues = k == 0
                && continues_above
                && run.dest.is_none()
                && chain.is_some()
                // A fragment opening on a new scheme is a new URL that happens
                // to start the next line, not the rest of the previous one.
                && !run.text.starts_with("http://")
                && !run.text.starts_with("https://");
            if !continues {
                flush(&mut chain, &mut out);
            }
            let cell = (i as u16, run.start as u16, run.end as u16);
            if let Some(dest) = &run.dest {
                out.push((cell.0, cell.1, cell.2, dest.clone()));
            } else if continues {
                if let Some((text, cells)) = chain.as_mut() {
                    text.push_str(&run.text);
                    cells.push(cell);
                }
            } else {
                chain = Some((run.text.clone(), vec![cell]));
            }
        }
    }
    flush(&mut chain, &mut out);
    out
}

/// Make the cells of [`link_regions`] clickable by wrapping each one in an
/// OSC 8 hyperlink. Terminals without support ignore the sequence and show the
/// glyphs unchanged; the cell keeps its terminal width through the forced diff
/// width, so wrapping does not shift the row.
pub(super) fn paint_links(
    buf: &mut ratatui::buffer::Buffer,
    area: Rect,
    regions: &[(u16, u16, u16, String)],
) {
    for (row, start, end, url) in regions {
        // The URL comes from model output: control characters must not reach
        // the terminal, where they would end the escape early.
        let Some(url) = web_target(url) else {
            continue;
        };
        for x in *start..*end {
            let Some(cell) = buf.cell_mut((area.x + x, area.y + row)) else {
                continue;
            };
            let sym = cell.symbol().to_string();
            if sym.is_empty() || sym.starts_with('\x1b') {
                continue;
            }
            let w = unicode_width::UnicodeWidthStr::width(sym.as_str()).max(1) as u16;
            let linked = format!("\x1b]8;;{url}\x1b\\{sym}\x1b]8;;\x1b\\");
            cell.set_symbol(&linked);
            cell.set_diff_option(ratatui::buffer::CellDiffOption::ForcedWidth(
                std::num::NonZeroU16::new(w).unwrap(),
            ));
        }
    }
}
