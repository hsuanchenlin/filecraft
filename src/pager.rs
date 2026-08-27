//! The full-screen read-only reader: what is on screen, where in the
//! document it is, and the search over it.
//!
//! The reader holds a parsed document ([`crate::markdown::DocLine`]) and
//! a scroll offset counted in *drawn rows*, so a wrapped line is scrolled
//! through rather than jumped over. Every computation here is pure: the
//! caller supplies the width and height the screen actually has, exactly
//! as the ladder is fitted to the width the row actually has.

use crate::bearings::Glyphs;
use crate::markdown::{self, DocLine, Row};

/// Rows the reader's own frame costs inside the listing area.
pub const FRAME_ROWS: usize = 2;
/// Columns the reader's own frame costs inside the listing area: two
/// border columns plus one column of breathing room on each side, so a
/// blockquote bar is never mistaken for the frame.
pub const FRAME_COLS: usize = 4;

/// A scrollable full-screen pane: help, the message ring, the agent
/// explanation, and the file reader all use it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pager {
    pub title: String,
    pub doc: Vec<DocLine>,
    /// First drawn row on screen.
    pub scroll: usize,
    /// Live `/` input, present only while a query is being typed.
    pub find: Option<String>,
    /// The committed query; empty when no search is in force.
    pub query: String,
}

impl Pager {
    /// A pane of plain lines, for text the app itself wrote.
    pub fn plain(title: impl Into<String>, lines: Vec<String>) -> Self {
        Pager::document(title, lines.into_iter().map(DocLine::body).collect())
    }

    /// A pane over an already-parsed document.
    pub fn document(title: impl Into<String>, doc: Vec<DocLine>) -> Self {
        Pager {
            title: title.into(),
            doc,
            scroll: 0,
            find: None,
            query: String::new(),
        }
    }

    /// Every source line as plain text.
    pub fn lines(&self) -> Vec<String> {
        self.doc.iter().map(DocLine::text).collect()
    }

    /// The whole document as plain text.
    pub fn text(&self) -> String {
        self.lines().join("\n")
    }

    /// The rows to draw in `width` columns.
    pub fn rows(&self, width: usize, glyphs: &Glyphs) -> Vec<Row> {
        markdown::layout(&self.doc, width, glyphs)
    }

    /// Largest scroll offset that still fills the view - the last page
    /// sits against the bottom instead of scrolling into empty space.
    pub fn max_scroll(total_rows: usize, view_rows: usize) -> usize {
        total_rows.saturating_sub(view_rows.max(1))
    }

    /// Clamp the offset to the geometry the screen actually has.
    pub fn clamp(&mut self, width: usize, view_rows: usize, glyphs: &Glyphs) {
        let total = self.rows(width, glyphs).len();
        self.scroll = self.scroll.min(Pager::max_scroll(total, view_rows));
    }

    /// Scroll by `delta` rows, staying inside the document.
    pub fn scroll_by(&mut self, delta: isize, width: usize, view_rows: usize, glyphs: &Glyphs) {
        let total = self.rows(width, glyphs).len();
        let max = Pager::max_scroll(total, view_rows) as isize;
        self.scroll = (self.scroll as isize + delta).clamp(0, max) as usize;
    }

    /// Jump to the top or the bottom.
    pub fn scroll_to_end(&mut self, width: usize, view_rows: usize, glyphs: &Glyphs) {
        let total = self.rows(width, glyphs).len();
        self.scroll = Pager::max_scroll(total, view_rows);
    }

    /// Put source line `line` on the first visible row.
    pub fn scroll_to_line(&mut self, line: usize, width: usize, view_rows: usize, glyphs: &Glyphs) {
        let rows = self.rows(width, glyphs);
        let target = rows
            .iter()
            .position(|row| row.line == line)
            .unwrap_or_else(|| rows.len().saturating_sub(1));
        self.scroll = target.min(Pager::max_scroll(rows.len(), view_rows));
    }

    /// The source line currently at the top of the view.
    pub fn top_line(&self, width: usize, glyphs: &Glyphs) -> usize {
        self.rows(width, glyphs)
            .get(self.scroll)
            .map(|row| row.line)
            .unwrap_or(0)
    }

    /// Move to the next (or previous) line matching the committed query.
    /// Returns false when the query matches nothing, so the caller can
    /// say so rather than leaving the screen silently still.
    pub fn step_match(
        &mut self,
        forward: bool,
        width: usize,
        view_rows: usize,
        glyphs: &Glyphs,
    ) -> bool {
        let matches = markdown::find_matches(&self.doc, &self.query);
        if matches.is_empty() {
            return false;
        }
        let here = self.top_line(width, glyphs);
        let next = if forward {
            matches
                .iter()
                .find(|&&line| line > here)
                .copied()
                .unwrap_or(matches[0])
        } else {
            matches
                .iter()
                .rev()
                .find(|&&line| line < here)
                .copied()
                .unwrap_or_else(|| *matches.last().expect("non-empty"))
        };
        self.scroll_to_line(next, width, view_rows, glyphs);
        true
    }

    /// Jump to the first match at or after the current top line.
    pub fn seek_match(&mut self, width: usize, view_rows: usize, glyphs: &Glyphs) -> bool {
        let matches = markdown::find_matches(&self.doc, &self.query);
        if matches.is_empty() {
            return false;
        }
        let here = self.top_line(width, glyphs);
        let target = matches
            .iter()
            .find(|&&line| line >= here)
            .copied()
            .unwrap_or(matches[0]);
        self.scroll_to_line(target, width, view_rows, glyphs);
        true
    }

    /// The position footer: the line at the top of the view, how many
    /// there are, and how far down the document that is. Words, not a
    /// scrollbar, so it can be read aloud like the status row.
    pub fn position(&self, width: usize, view_rows: usize, glyphs: &Glyphs) -> String {
        let total_lines = self.doc.len().max(1);
        let rows = self.rows(width, glyphs).len();
        let max = Pager::max_scroll(rows, view_rows);
        let percent = if max == 0 {
            100
        } else {
            (self.scroll.min(max) * 100) / max
        };
        let line = self.top_line(width, glyphs) + 1;
        format!(
            "line {line} of {total_lines} {dot} {percent}%",
            dot = glyphs.dot
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::markdown;

    const W: usize = 40;
    const H: usize = 10;

    fn numbered(lines: usize) -> Pager {
        Pager::plain("doc", (1..=lines).map(|i| format!("line {i}")).collect())
    }

    #[test]
    fn scrolling_stops_at_a_full_last_page() {
        let mut pager = numbered(30);
        pager.scroll_by(1000, W, H, &Glyphs::UNICODE);
        assert_eq!(pager.scroll, 20);
        pager.scroll_by(-1000, W, H, &Glyphs::UNICODE);
        assert_eq!(pager.scroll, 0);
    }

    #[test]
    fn a_document_shorter_than_the_view_never_scrolls() {
        let mut pager = numbered(4);
        pager.scroll_by(5, W, H, &Glyphs::UNICODE);
        assert_eq!(pager.scroll, 0);
        pager.scroll_to_end(W, H, &Glyphs::UNICODE);
        assert_eq!(pager.scroll, 0);
    }

    #[test]
    fn scrolling_counts_drawn_rows_not_source_lines() {
        // One source line that wraps into three rows.
        let doc = markdown::parse_plain("alpha beta gamma delta epsilon zeta");
        let mut pager = Pager::document("wrapped", doc);
        assert_eq!(pager.rows(12, &Glyphs::UNICODE).len(), 3);
        pager.scroll_by(1, 12, 1, &Glyphs::UNICODE);
        assert_eq!(pager.scroll, 1);
        // Still inside the same source line.
        assert_eq!(pager.top_line(12, &Glyphs::UNICODE), 0);
    }

    #[test]
    fn the_position_reads_as_a_sentence() {
        let mut pager = numbered(30);
        assert_eq!(pager.position(W, H, &Glyphs::UNICODE), "line 1 of 30 · 0%");
        pager.scroll_to_end(W, H, &Glyphs::UNICODE);
        assert_eq!(
            pager.position(W, H, &Glyphs::UNICODE),
            "line 21 of 30 · 100%"
        );
        // Everything on screen is 100% read.
        assert_eq!(
            numbered(3).position(W, H, &Glyphs::UNICODE),
            "line 1 of 3 · 100%"
        );
    }

    #[test]
    fn search_steps_forward_and_wraps() {
        let mut pager = numbered(30);
        pager.query = "line 2".to_string();
        // 2, 20..29 - and the seek starts from the top.
        assert!(pager.seek_match(W, H, &Glyphs::UNICODE));
        assert_eq!(pager.top_line(W, &Glyphs::UNICODE), 1);
        assert!(pager.step_match(true, W, H, &Glyphs::UNICODE));
        assert_eq!(pager.top_line(W, &Glyphs::UNICODE), 19);
        assert!(pager.step_match(false, W, H, &Glyphs::UNICODE));
        assert_eq!(pager.top_line(W, &Glyphs::UNICODE), 1);
        // Backwards from the first match wraps to the last.
        assert!(pager.step_match(false, W, H, &Glyphs::UNICODE));
        assert_eq!(pager.top_line(W, &Glyphs::UNICODE), 20);
    }

    #[test]
    fn a_query_that_matches_nothing_says_so_instead_of_moving() {
        let mut pager = numbered(30);
        pager.scroll = 5;
        pager.query = "nowhere".to_string();
        assert!(!pager.seek_match(W, H, &Glyphs::UNICODE));
        assert!(!pager.step_match(true, W, H, &Glyphs::UNICODE));
        assert_eq!(pager.scroll, 5);
    }

    #[test]
    fn a_narrower_screen_reclamps_the_offset() {
        let mut pager = numbered(30);
        pager.scroll_to_end(W, H, &Glyphs::UNICODE);
        assert_eq!(pager.scroll, 20);
        // A taller view shows more at once, so the last page starts higher.
        pager.clamp(W, 25, &Glyphs::UNICODE);
        assert_eq!(pager.scroll, 5);
    }
}
