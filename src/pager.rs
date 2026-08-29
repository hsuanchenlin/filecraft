//! The full-screen read-only reader: what is on screen, where in the
//! document it is, and the search over it.
//!
//! The reader holds a parsed document ([`crate::markdown::DocLine`]) and
//! a scroll offset counted in *drawn rows*, so a wrapped line is scrolled
//! through rather than jumped over. Every computation here is pure: the
//! caller supplies the width and height the screen actually has, exactly
//! as the ladder is fitted to the width the row actually has.

use std::cell::RefCell;
use std::rc::Rc;

use crate::bearings::Glyphs;
use crate::markdown::{self, DocLine, Row};

/// Rows the reader's own frame costs inside the listing area.
pub const FRAME_ROWS: usize = 2;
/// Columns the reader's own frame costs inside the listing area: two
/// border columns plus one column of breathing room on each side, so a
/// blockquote bar is never mistaken for the frame.
pub const FRAME_COLS: usize = 4;

/// The document laid out to one geometry, kept so a drawn frame and a
/// keypress each lay the document out at most once. `Pager::doc` is
/// immutable after construction, so the geometry is the whole key.
#[derive(Debug, Clone)]
struct Laid {
    width: usize,
    glyphs: Glyphs,
    rows: Rc<Vec<Row>>,
}

/// A scrollable full-screen pane: help, the message ring, the agent
/// explanation, and the file reader all use it.
#[derive(Debug, Clone)]
pub struct Pager {
    pub title: String,
    doc: Vec<DocLine>,
    /// First drawn row on screen.
    pub scroll: usize,
    /// Live `/` input, present only while a query is being typed.
    pub find: Option<String>,
    /// The committed query; empty when no search is in force.
    pub query: String,
    laid: RefCell<Option<Laid>>,
}

/// The cache is not part of what a pane *is*: two panes over the same
/// document at the same offset are equal whether or not either has been
/// drawn yet.
impl PartialEq for Pager {
    fn eq(&self, other: &Self) -> bool {
        self.title == other.title
            && self.doc == other.doc
            && self.scroll == other.scroll
            && self.find == other.find
            && self.query == other.query
    }
}

impl Eq for Pager {}

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
            laid: RefCell::new(None),
        }
    }

    /// The parsed document behind this pane.
    pub fn doc(&self) -> &[DocLine] {
        &self.doc
    }

    /// Swap the document, keeping where the reader is and what it is
    /// searching for.
    ///
    /// A file never does this - it is read once and does not change
    /// underneath. A running provider's log does: it grows while it is
    /// being read, and the offset, the live `/` input, and the committed
    /// query all have to survive that. The layout cache does not: it is
    /// keyed on the geometry, not on the document, so it is dropped here
    /// rather than left describing lines that are gone.
    pub fn replace_doc(&mut self, doc: Vec<DocLine>) {
        self.doc = doc;
        *self.laid.borrow_mut() = None;
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
    ///
    /// Memoized on the geometry it was laid out for: a frame asks for the
    /// row count, the top line, and the rows themselves, and every scroll
    /// key asks again, so laying the document out once per geometry is
    /// what keeps a held `j` responsive on a file at the reader's cap.
    pub fn rows(&self, width: usize, glyphs: &Glyphs) -> Rc<Vec<Row>> {
        let width = width.max(1);
        if let Some(laid) = self.laid.borrow().as_ref() {
            if laid.width == width && laid.glyphs == *glyphs {
                return Rc::clone(&laid.rows);
            }
        }
        let rows = Rc::new(markdown::layout(&self.doc, width, glyphs));
        *self.laid.borrow_mut() = Some(Laid {
            width,
            glyphs: *glyphs,
            rows: Rc::clone(&rows),
        });
        rows
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
    ///
    /// An offset past the end reads as the last line rather than the
    /// first: a stale offset must never claim the reader is at the top of
    /// a document whose bottom is on screen.
    pub fn top_line(&self, width: usize, glyphs: &Glyphs) -> usize {
        let rows = self.rows(width, glyphs);
        rows.get(self.scroll)
            .or_else(|| rows.last())
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
        let percent = (self.scroll.min(max) * 100).checked_div(max).unwrap_or(100);
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
    fn the_layout_is_computed_once_per_geometry() {
        // A frame asks for the row count, the top line, and the rows;
        // every one of those must reuse the same laid-out document.
        let pager = numbered(200);
        let first = pager.rows(W, &Glyphs::UNICODE);
        assert!(Rc::ptr_eq(&first, &pager.rows(W, &Glyphs::UNICODE)));
        pager.position(W, H, &Glyphs::UNICODE);
        pager.top_line(W, &Glyphs::UNICODE);
        assert!(Rc::ptr_eq(&first, &pager.rows(W, &Glyphs::UNICODE)));
        // A new width or character set is a new geometry, so it re-lays.
        assert!(!Rc::ptr_eq(&first, &pager.rows(W + 1, &Glyphs::UNICODE)));
        assert!(!Rc::ptr_eq(&first, &pager.rows(W, &Glyphs::ASCII)));
    }

    #[test]
    fn a_plain_pane_is_cleaned_like_a_parsed_one() {
        // Text the app itself wrote goes through the same cleaning, so
        // the width the wrap budgets is the width the screen spends.
        let pager = Pager::plain("built-in", vec!["\t\t\tdeeply indented".to_string()]);
        let line = &pager.lines()[0];
        assert!(!line.contains('\t'));
        assert!(line.starts_with("            deeply"));
        for row in pager.rows(20, &Glyphs::UNICODE).iter() {
            assert!(crate::bearings::display_width(&row.text()) <= 20);
        }
    }

    #[test]
    fn a_stale_offset_reads_as_the_last_line_not_the_first() {
        // Nothing should be able to set this, but if it ever is, the
        // footer and `n`/`N` must not claim the reader is at the top.
        let mut pager = numbered(30);
        pager.scroll = 9_999;
        assert_eq!(pager.top_line(W, &Glyphs::UNICODE), 29);
    }

    /// A growing document keeps the reader where it was and keeps what
    /// it was searching for, and the memoized layout is dropped so the
    /// new lines are actually drawn.
    #[test]
    fn replacing_the_document_keeps_the_reader_where_it_was() {
        let mut pager = numbered(30);
        pager.scroll_by(5, W, H, &Glyphs::UNICODE);
        pager.query = "line 2".to_string();
        pager.find = Some("part".to_string());
        let before = pager.rows(W, &Glyphs::UNICODE);

        pager.replace_doc(markdown::parse_plain(&"grew\n".repeat(60)));
        assert_eq!(pager.scroll, 5);
        assert_eq!(pager.query, "line 2");
        assert_eq!(pager.find.as_deref(), Some("part"));
        let after = pager.rows(W, &Glyphs::UNICODE);
        assert!(!Rc::ptr_eq(&before, &after));
        assert_eq!(after.len(), 60);
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

    #[test]
    fn a_wider_screen_unwraps_the_document_and_reclamps_the_offset() {
        // Lines that wrap 3:1 at 12 columns fit on one row at 60, so the
        // bottom of the document moves a long way up.
        let doc = markdown::parse_plain(&"alpha beta gamma delta\n".repeat(30));
        let mut pager = Pager::document("wrapped", doc);
        pager.scroll_to_end(12, H, &Glyphs::UNICODE);
        assert!(pager.scroll > 20);
        pager.clamp(60, H, &Glyphs::UNICODE);
        assert_eq!(pager.scroll, Pager::max_scroll(30, H));
        // The position is honest against the wider screen, not stuck at
        // the top of the file.
        assert_eq!(
            pager.position(60, H, &Glyphs::UNICODE),
            "line 21 of 30 · 100%"
        );
    }
}
