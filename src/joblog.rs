//! The live log viewer: a full-screen read-only pane over a running
//! provider's output.
//!
//! The pane owns nothing the run needs. It reads a [`stream::Handle`],
//! turns the lines it holds into a document, and hands that to the same
//! [`Pager`] the file reader uses - so scrolling, searching, and the
//! position footer are one implementation, not two. Closing the pane
//! closes a view and nothing else: the run keeps going.
//!
//! Two things are the pane's own. It **follows** the newest output while
//! the reader is at the bottom, and stops the moment the reader scrolls
//! up, which is what makes a live log readable without a modifier key.
//! And it draws a two-row header naming the provider, what the run is
//! doing, and the session the provider announced, together with the
//! command that reopens that session in the provider's own CLI.

use std::time::Instant;

use crate::bearings::Glyphs;
use crate::markdown::DocLine;
use crate::pager::{self, Pager};
use crate::stream::{self, Activity, Handle};
use crate::summarize::Provider;

/// Rows the log pane's own frame costs inside the listing area: the
/// reader's border, plus the two pinned header rows.
///
/// This must match what [`crate::ui::draw_job_log`] reserves, exactly as
/// [`pager::FRAME_ROWS`] must match the reader's block: scrolling and
/// drawing have to agree about what a row is.
pub const FRAME_ROWS: usize = pager::FRAME_ROWS + HEADER_ROWS;

/// Columns it costs - the same border and breathing room the reader has.
pub const FRAME_COLS: usize = pager::FRAME_COLS;

/// Header rows drawn above the log and never scrolled.
pub const HEADER_ROWS: usize = 2;

/// The log viewer's state: the pane, plus what it last read from the run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogPane {
    pub pager: Pager,
    /// Whether new output pulls the view down with it. True while the
    /// reader is at the bottom; false the moment it scrolls up.
    pub follow: bool,
    /// The log version the document was built from.
    seen: u64,
    provider: Provider,
    activity: Activity,
    total: usize,
    session: Option<String>,
}

impl LogPane {
    /// A pane over `provider`'s run. The document is empty until
    /// [`LogPane::sync`] fills it, which the caller does immediately.
    pub fn new(provider: Provider) -> Self {
        LogPane {
            pager: Pager::plain(format!("job log: {}", provider.program()), Vec::new()),
            follow: true,
            seen: 0,
            provider,
            activity: Activity::Waiting,
            total: 0,
            session: None,
        }
    }

    /// The session the provider announced, if it announced one.
    pub fn session(&self) -> Option<&str> {
        self.session.as_deref()
    }

    /// What the run is doing, as of the last [`LogPane::sync`].
    pub fn activity(&self) -> Activity {
        self.activity
    }

    /// Read `handle` and rebuild the document if it has changed.
    ///
    /// Called once per frame. The state - what the run is doing, how much
    /// it has said, which session it is - is re-read every time, because
    /// a run goes quiet without printing anything; the document is laid
    /// out again only when the log itself has moved.
    pub fn sync(
        &mut self,
        handle: &Handle,
        now: Instant,
        width: usize,
        view_rows: usize,
        glyphs: &Glyphs,
    ) {
        let state = handle.state(now);
        self.activity = state.activity;
        self.total = state.total;
        self.session = state.session;

        if let Some(snapshot) = handle.snapshot_since(self.seen) {
            self.seen = snapshot.version;
            self.pager.replace_doc(document(&snapshot));
        }
        if self.follow {
            self.pager.scroll_to_end(width, view_rows, glyphs);
        } else {
            self.pager.clamp(width, view_rows, glyphs);
        }
    }

    /// Re-read whether the view is following after a scroll key.
    ///
    /// Following *is* being at the bottom - there is no separate mode to
    /// turn on. `G` re-arms it because `G` goes to the bottom, and `k`
    /// drops it because `k` leaves the bottom; nothing else is needed.
    pub fn refollow(&mut self, width: usize, view_rows: usize, glyphs: &Glyphs) {
        let rows = self.pager.rows(width, glyphs).len();
        self.follow = self.pager.scroll >= Pager::max_scroll(rows, view_rows);
    }

    /// The two pinned header rows.
    ///
    /// The first says what the run is: the provider, what it is doing,
    /// and how much it has printed. The second says which session it is
    /// and how to reopen it, or says plainly that the provider never
    /// announced one - never a command that would not work.
    pub fn header(&self, glyphs: &Glyphs) -> [String; HEADER_ROWS] {
        let dot = glyphs.dot;
        let unit = if self.total == 1 { "line" } else { "lines" };
        let top = format!(
            "{} {dot} {} {dot} {} {unit}",
            self.provider.program(),
            self.activity.label(),
            self.total
        );
        let bottom = match self.session.as_deref() {
            Some(id) => format!(
                "session {id} {dot} resume: {}",
                self.provider.resume_command(id)
            ),
            None => format!("session: not reported by {}", self.provider.program()),
        };
        [top, bottom]
    }
}

/// The log as a document: one body line per captured line, with a note
/// at the top when the buffer has forgotten the beginning of the run.
///
/// The note is a line of its own rather than a change to the numbering,
/// so line 4001 is still called line 4001 and the gap is stated instead
/// of implied.
fn document(snapshot: &stream::Snapshot) -> Vec<DocLine> {
    let mut doc: Vec<DocLine> = Vec::with_capacity(snapshot.lines.len() + 1);
    if snapshot.dropped > 0 {
        doc.push(DocLine::meta(format!(
            "({} earlier {} dropped - the log keeps the last {})",
            snapshot.dropped,
            if snapshot.dropped == 1 {
                "line"
            } else {
                "lines"
            },
            stream::MAX_LINES
        )));
    }
    if snapshot.lines.is_empty() {
        doc.push(DocLine::meta("(no output yet)"));
        return doc;
    }
    doc.extend(
        snapshot
            .lines
            .iter()
            .map(|line| DocLine::gutter(line.gutter(), &line.text)),
    );
    doc
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::stream::Origin;

    const W: usize = 60;
    const H: usize = 10;

    /// The rows the pane would draw, gutters and all.
    fn drawn(pane: &LogPane) -> Vec<String> {
        pane.pager
            .rows(W, &Glyphs::UNICODE)
            .iter()
            .map(|row| row.text().trim_end().to_string())
            .collect()
    }

    fn pane_over(handle: &Handle) -> LogPane {
        let mut pane = LogPane::new(Provider::Co);
        pane.sync(handle, Instant::now(), W, H, &Glyphs::UNICODE);
        pane
    }

    #[test]
    fn an_empty_run_says_so_rather_than_drawing_nothing() {
        let handle = Handle::new();
        let pane = pane_over(&handle);
        assert_eq!(pane.pager.lines(), vec!["(no output yet)".to_string()]);
        assert_eq!(pane.activity(), Activity::Waiting);
    }

    #[test]
    fn new_output_pulls_the_view_down_while_the_reader_is_at_the_bottom() {
        let handle = Handle::new();
        let mut pane = pane_over(&handle);
        for i in 1..=40 {
            handle.append(Origin::Out, &format!("line {i}\n"));
        }
        pane.sync(&handle, Instant::now(), W, H, &Glyphs::UNICODE);
        assert!(pane.follow);
        assert_eq!(pane.pager.scroll, Pager::max_scroll(40, H));

        // Scrolling up stops the follow, and later output leaves the
        // reader exactly where it was reading.
        pane.pager.scroll_by(-20, W, H, &Glyphs::UNICODE);
        pane.refollow(W, H, &Glyphs::UNICODE);
        assert!(!pane.follow);
        let held = pane.pager.scroll;
        for i in 41..=60 {
            handle.append(Origin::Out, &format!("line {i}\n"));
        }
        pane.sync(&handle, Instant::now(), W, H, &Glyphs::UNICODE);
        assert_eq!(pane.pager.scroll, held);

        // Going back to the bottom re-arms it.
        pane.pager.scroll_to_end(W, H, &Glyphs::UNICODE);
        pane.refollow(W, H, &Glyphs::UNICODE);
        assert!(pane.follow);
    }

    #[test]
    fn both_streams_are_shown_and_told_apart_by_a_character() {
        let handle = Handle::new();
        handle.append(Origin::Out, "reading the files\n");
        handle.append(Origin::Err, "warning: slow\n");
        let pane = pane_over(&handle);
        assert_eq!(
            drawn(&pane),
            vec![
                "    1 | reading the files".to_string(),
                "    2 ! warning: slow".to_string(),
            ]
        );
    }

    /// A log line too wide for the pane hangs from its own gutter, so a
    /// continuation is never read as a line that lost its number.
    #[test]
    fn a_wrapped_line_is_indented_under_its_own_text() {
        let handle = Handle::new();
        handle.append(Origin::Out, &format!("{}\n", "word ".repeat(40)));
        let pane = pane_over(&handle);
        let rows = drawn(&pane);
        assert!(rows.len() > 1, "{rows:?}");
        assert!(rows[0].starts_with("    1 | word"), "{rows:?}");
        for row in &rows[1..] {
            assert!(row.starts_with("        word"), "{row:?}");
        }
    }

    #[test]
    fn the_header_names_the_run_and_the_session_it_opened() {
        let handle = Handle::new();
        handle.append(Origin::Err, "session id: 01a04eef-d4a6-7232\n");
        let pane = pane_over(&handle);
        assert_eq!(pane.session(), Some("01a04eef-d4a6-7232"));
        let [top, bottom] = pane.header(&Glyphs::UNICODE);
        assert_eq!(top, "codex · streaming · 1 line");
        assert_eq!(
            bottom,
            "session 01a04eef-d4a6-7232 · resume: codex resume 01a04eef-d4a6-7232"
        );
    }

    #[test]
    fn a_run_that_never_announced_a_session_says_so() {
        let handle = Handle::new();
        handle.append(Origin::Out, "just some output\n");
        handle.end();
        let pane = pane_over(&handle);
        let [top, bottom] = pane.header(&Glyphs::UNICODE);
        assert_eq!(top, "codex · finished · 1 line");
        assert_eq!(bottom, "session: not reported by codex");
    }

    /// The header is drawn in whichever character set is in force, like
    /// every other row: nothing is baked in when the pane is built.
    #[test]
    fn the_header_follows_the_character_set() {
        let handle = Handle::new();
        handle.append(Origin::Out, "a\n");
        let pane = pane_over(&handle);
        assert!(pane.header(&Glyphs::ASCII)[0].contains(Glyphs::ASCII.dot));
        assert!(!pane.header(&Glyphs::ASCII)[0].contains(Glyphs::UNICODE.dot));
    }

    #[test]
    fn a_forgotten_beginning_is_stated_rather_than_implied() {
        let handle = Handle::new();
        for i in 1..=stream::MAX_LINES + 3 {
            handle.append(Origin::Out, &format!("line {i}\n"));
        }
        let pane = pane_over(&handle);
        let lines = drawn(&pane);
        assert!(
            lines[0].starts_with("(3 earlier lines dropped"),
            "{:?}",
            lines[0]
        );
        assert!(lines[1].starts_with("    4 | line 4"), "{:?}", lines[1]);
    }

    /// The pane lays the log out again only when the log has moved, but
    /// it re-reads what the run is doing every time - a provider goes
    /// quiet without printing anything.
    #[test]
    fn an_unchanged_log_is_not_laid_out_again_but_is_still_re_read() {
        let handle = Handle::new();
        handle.append(Origin::Out, "one line\n");
        let mut pane = pane_over(&handle);
        assert_eq!(pane.activity(), Activity::Streaming);
        let before = pane.pager.rows(W, &Glyphs::UNICODE);

        pane.sync(&handle, Instant::now(), W, H, &Glyphs::UNICODE);
        assert!(std::rc::Rc::ptr_eq(
            &before,
            &pane.pager.rows(W, &Glyphs::UNICODE)
        ));

        handle.end();
        pane.sync(&handle, Instant::now(), W, H, &Glyphs::UNICODE);
        assert_eq!(pane.activity(), Activity::Ended);
    }

    /// The frame the pane scrolls against is the reader's frame plus its
    /// own header. If these ever disagree the log scrolls past rows the
    /// screen never drew.
    #[test]
    fn the_frame_is_the_readers_frame_plus_the_header() {
        assert_eq!(FRAME_ROWS, pager::FRAME_ROWS + HEADER_ROWS);
        assert_eq!(FRAME_COLS, pager::FRAME_COLS);
    }
}
