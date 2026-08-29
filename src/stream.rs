//! The live output of a running provider: what it has said so far, on
//! which stream, and what that says about whether it is still thinking.
//!
//! [`StreamLog`] is a pure buffer. Bytes arrive in whatever chunks a pipe
//! hands over - half a line, three lines, a spinner rewriting itself -
//! and come out as whole, numbered, terminal-safe lines. Nothing here
//! reads a pipe, spawns anything, or touches a terminal: the one moving
//! part is [`Handle`], a `StreamLog` behind a lock so the reader threads
//! in [`crate::summarize::ProcessRunner`] can fill it while the UI thread
//! draws it.
//!
//! Two things a provider's output does that a file never does are dealt
//! with here rather than downstream. It carries ANSI escape sequences,
//! which the reader would otherwise draw as `\u{FFFD}[32m`; and it
//! rewrites its own last line with a carriage return, which would
//! otherwise stack a spinner's every frame into the log. Both are
//! resolved as the line is completed, so what the buffer holds is what a
//! terminal would have shown.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use crate::session;

/// Lines kept before the oldest are forgotten. A long run can print more
/// than a screen can ever hold; what matters is the recent end of it, and
/// the numbering keeps counting so a dropped prefix is visible rather
/// than silent.
pub const MAX_LINES: usize = 4000;

/// Longest run of bytes accepted without a newline before it is committed
/// as a line anyway. A provider streaming a single enormous line must not
/// be able to grow the buffer without bound.
const MAX_PARTIAL: usize = 8 * 1024;

/// How quiet a running provider has to be before it is called thinking
/// rather than streaming. Long enough that a normal gap between tokens
/// does not flicker the header.
pub const THINKING_AFTER: Duration = Duration::from_secs(3);

/// Which of the child's two streams a line came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Origin {
    Out,
    Err,
}

impl Origin {
    /// The one character that separates a line's number from its text.
    /// A textual dual, not a color: `NO_COLOR` loses nothing.
    pub fn mark(self) -> char {
        match self {
            Origin::Out => '|',
            Origin::Err => '!',
        }
    }
}

/// One completed line of provider output.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StreamLine {
    pub origin: Origin,
    /// Its position in everything the run has printed, counting from one
    /// and counting lines already dropped.
    pub number: usize,
    pub text: String,
}

impl StreamLine {
    /// The fixed gutter the line hangs from: its number, then the
    /// character naming the stream it came from. A wrapped line is
    /// indented to exactly this width, so a continuation is never read
    /// as a line that lost its number.
    pub fn gutter(&self) -> String {
        format!("{:>5} {}", self.number, self.origin.mark())
    }

    /// The line as the log viewer draws it, gutter and all - one string,
    /// for anything that wants the whole row rather than its two halves.
    pub fn numbered(&self) -> String {
        format!("{} {}", self.gutter(), self.text)
    }
}

/// Everything a run has printed, as whole lines.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct StreamLog {
    lines: VecDeque<StreamLine>,
    partial_out: String,
    partial_err: String,
    total: usize,
    dropped: usize,
    session: Option<String>,
}

impl StreamLog {
    pub fn new() -> Self {
        StreamLog::default()
    }

    /// Take another chunk of bytes from one of the streams.
    ///
    /// Only whole lines are committed; a trailing fragment waits for the
    /// rest of itself, unless it grows past [`MAX_PARTIAL`].
    pub fn push(&mut self, origin: Origin, chunk: &str) {
        let mut ready: Vec<String> = Vec::new();
        {
            let buffer = match origin {
                Origin::Out => &mut self.partial_out,
                Origin::Err => &mut self.partial_err,
            };
            buffer.push_str(chunk);
            while let Some(at) = buffer.find('\n') {
                ready.push(buffer[..at].to_string());
                buffer.drain(..=at);
            }
            if buffer.len() >= MAX_PARTIAL {
                ready.push(std::mem::take(buffer));
            }
        }
        for raw in ready {
            self.commit(origin, &raw);
        }
    }

    /// Commit whatever is left in both buffers. Called once when the run
    /// is over, so a provider that never ended its last line still has it
    /// in the log.
    pub fn flush(&mut self) {
        for origin in [Origin::Out, Origin::Err] {
            let buffer = match origin {
                Origin::Out => &mut self.partial_out,
                Origin::Err => &mut self.partial_err,
            };
            if buffer.is_empty() {
                continue;
            }
            let raw = std::mem::take(buffer);
            self.commit(origin, &raw);
        }
    }

    fn commit(&mut self, origin: Origin, raw: &str) {
        let text = settle(raw);
        self.total += 1;
        if self.session.is_none() {
            self.session = session::scan(&text);
        }
        self.lines.push_back(StreamLine {
            origin,
            number: self.total,
            text,
        });
        while self.lines.len() > MAX_LINES {
            self.lines.pop_front();
            self.dropped += 1;
        }
    }

    /// The lines still held, oldest first.
    pub fn lines(&self) -> impl Iterator<Item = &StreamLine> {
        self.lines.iter()
    }

    /// How many lines the run has printed in total, including any the
    /// buffer has since forgotten.
    pub fn total(&self) -> usize {
        self.total
    }

    /// How many of those are no longer held.
    pub fn dropped(&self) -> usize {
        self.dropped
    }

    /// The session identifier the provider announced, if it announced
    /// one. The first one seen wins: a run has one session, and a later
    /// line echoing another must not rename it.
    pub fn session(&self) -> Option<&str> {
        self.session.as_deref()
    }
}

/// Take everything decodable out of `tail`, leaving behind only bytes the
/// pipe has not finished handing over.
///
/// Two things look alike at a chunk boundary and are not. A multi-byte
/// character split by the boundary has to wait for the rest of itself,
/// or a spinner glyph cut in half becomes a replacement character. Bytes
/// that are simply not UTF-8 will never become valid, so waiting for them
/// would wait forever - they are consumed here as the one replacement
/// character a lossy read would have produced for them, which is what
/// keeps the decoded stream identical to reading the whole pipe lossily.
pub fn decode(tail: &mut Vec<u8>) -> String {
    let mut text = String::new();
    loop {
        match std::str::from_utf8(tail) {
            Ok(whole) => {
                text.push_str(whole);
                tail.clear();
                return text;
            }
            Err(error) => {
                let good = error.valid_up_to();
                text.push_str(std::str::from_utf8(&tail[..good]).unwrap_or_default());
                match error.error_len() {
                    // Invalid: consumed, so the loop always moves on.
                    Some(bad) => {
                        text.push(char::REPLACEMENT_CHARACTER);
                        tail.drain(..good + bad);
                    }
                    // Incomplete: the rest of it is still in the pipe.
                    None => {
                        tail.drain(..good);
                        return text;
                    }
                }
            }
        }
    }
}

/// One raw line as a terminal would have left it: the last rewrite wins,
/// and no escape sequence survives.
fn settle(raw: &str) -> String {
    // A CRLF ending is not a rewrite - strip it before the rest is read.
    let raw = raw.strip_suffix('\r').unwrap_or(raw);
    let last = raw.rsplit('\r').next().unwrap_or(raw);
    strip_ansi(last)
}

/// Remove ANSI escape sequences, leaving the text they were styling.
///
/// Four shapes, which between them cover what a CLI actually emits:
///
/// - CSI (`ESC [ ... final`) - color, cursor movement, line clearing.
/// - The string sequences (`ESC ]`, `P`, `X`, `^`, `_`) - window titles
///   and hyperlinks - which run until BEL or ESC `\`.
/// - `ESC` then intermediate characters then a final one - `ESC ( B`,
///   the charset selection a lot of tools print on their way out.
/// - Anything else, which is two characters.
///
/// What is left is text, which the reader then cleans like any other
/// line. A sequence cut short by the end of the line takes nothing with
/// it but itself.
pub fn strip_ansi(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '\u{1b}' {
            out.push(c);
            continue;
        }
        match chars.peek().copied() {
            // A second escape belongs to the sequence after this one, so
            // it is left where it is rather than eaten as this one's
            // second character.
            Some('\u{1b}') => {}
            // Control Sequence Introducer: parameters, then a final byte.
            Some('[') => {
                chars.next();
                for next in chars.by_ref() {
                    if ('\u{40}'..='\u{7e}').contains(&next) {
                        break;
                    }
                }
            }
            // A string sequence: text, then BEL or ESC \.
            Some(']' | 'P' | 'X' | '^' | '_') => {
                chars.next();
                while let Some(next) = chars.next() {
                    if next == '\u{7}' {
                        break;
                    }
                    if next == '\u{1b}' {
                        chars.next();
                        break;
                    }
                }
            }
            // Intermediates, then the final character.
            Some(c) if ('\u{20}'..='\u{2f}').contains(&c) => {
                chars.next();
                for next in chars.by_ref() {
                    if !('\u{20}'..='\u{2f}').contains(&next) {
                        break;
                    }
                }
            }
            // Any other escape is two characters; both go.
            Some(_) => {
                chars.next();
            }
            None => break,
        }
    }
    out
}

/// What a run is doing right now, in one word.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Activity {
    /// Running, and it has not said anything at all yet.
    Waiting,
    /// Running, and quiet for longer than [`THINKING_AFTER`].
    Thinking,
    /// Running and printing.
    Streaming,
    /// Over.
    Ended,
}

impl Activity {
    /// The word the header shows. Never a spinner: the header is read the
    /// same way the status row is.
    pub fn label(self) -> &'static str {
        match self {
            Activity::Waiting => "waiting for output",
            Activity::Thinking => "thinking",
            Activity::Streaming => "streaming",
            Activity::Ended => "finished",
        }
    }
}

/// The rule, as a pure function of what is known: a finished run has
/// ended, a silent one is waiting, a quiet one is thinking, and one that
/// just printed is streaming.
pub fn activity(running: bool, lines: usize, quiet: Option<Duration>) -> Activity {
    if !running {
        return Activity::Ended;
    }
    if lines == 0 {
        return Activity::Waiting;
    }
    match quiet {
        Some(quiet) if quiet < THINKING_AFTER => Activity::Streaming,
        _ => Activity::Thinking,
    }
}

/// The log, its version, and whether the run is still going.
#[derive(Debug)]
struct Shared {
    log: StreamLog,
    /// Bumped on every change, so the viewer can lay the document out
    /// once per change rather than once per frame.
    version: u64,
    /// When the last line arrived. `None` until the first one does.
    updated: Option<Instant>,
    ended: bool,
}

/// Versions count from one, so a viewer that has seen nothing (`0`) is
/// always handed the log the first time it asks - including the empty
/// log of a run that has not printed yet.
impl Default for Shared {
    fn default() -> Self {
        Shared {
            log: StreamLog::new(),
            version: 1,
            updated: None,
            ended: false,
        }
    }
}

/// A [`StreamLog`] shared between the threads draining a child's pipes
/// and the thread drawing the screen.
///
/// Cloning a handle shares the same log. A lock poisoned by a panicking
/// writer still hands the log back: the viewer's job is to show what was
/// captured, and refusing to draw would be a worse answer than drawing a
/// log that stopped growing.
#[derive(Clone, Default)]
pub struct Handle {
    inner: Arc<Mutex<Shared>>,
}

impl std::fmt::Debug for Handle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let shared = self.hold();
        f.debug_struct("Handle")
            .field("lines", &shared.log.total())
            .field("version", &shared.version)
            .field("ended", &shared.ended)
            .finish()
    }
}

/// Everything the viewer needs about a run's state, read under one lock.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct State {
    pub activity: Activity,
    /// Lines printed so far, including any already forgotten.
    pub total: usize,
    pub session: Option<String>,
}

/// The lines to draw, handed over only when they have changed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Snapshot {
    pub version: u64,
    /// Every line the log still holds, oldest first.
    pub lines: Vec<StreamLine>,
    pub dropped: usize,
}

impl Handle {
    pub fn new() -> Self {
        Handle::default()
    }

    fn hold(&self) -> MutexGuard<'_, Shared> {
        self.inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Take another chunk from one of the child's streams.
    pub fn append(&self, origin: Origin, chunk: &str) {
        if chunk.is_empty() {
            return;
        }
        let mut shared = self.hold();
        shared.log.push(origin, chunk);
        shared.version += 1;
        shared.updated = Some(Instant::now());
    }

    /// The run is over: commit any unterminated last line and stop
    /// calling it live. Idempotent - the runner ends its own run, and the
    /// app ends it again when it collects the outcome.
    pub fn end(&self) {
        let mut shared = self.hold();
        if shared.ended {
            return;
        }
        shared.log.flush();
        shared.ended = true;
        shared.version += 1;
    }

    /// Whether the run is still going.
    pub fn running(&self) -> bool {
        !self.hold().ended
    }

    /// The session identifier the provider announced, if any.
    pub fn session(&self) -> Option<String> {
        self.hold().log.session().map(str::to_string)
    }

    /// What the run is doing, as of `now`.
    pub fn state(&self, now: Instant) -> State {
        let shared = self.hold();
        let quiet = shared.updated.map(|at| now.saturating_duration_since(at));
        State {
            activity: activity(!shared.ended, shared.log.total(), quiet),
            total: shared.log.total(),
            session: shared.log.session().map(str::to_string),
        }
    }

    /// The lines to draw, or `None` when nothing has changed since
    /// `seen`. A log that has not moved is not laid out again.
    pub fn snapshot_since(&self, seen: u64) -> Option<Snapshot> {
        let shared = self.hold();
        if shared.version == seen {
            return None;
        }
        Some(Snapshot {
            version: shared.version,
            lines: shared.log.lines().cloned().collect(),
            dropped: shared.log.dropped(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn texts(log: &StreamLog) -> Vec<String> {
        log.lines().map(|line| line.text.clone()).collect()
    }

    /// A character split by a chunk boundary waits for the rest of
    /// itself; bytes that are not UTF-8 at all are consumed, because
    /// waiting for those would stall every line after them for the rest
    /// of the run.
    #[test]
    fn a_split_character_waits_and_an_invalid_one_does_not() {
        let mut tail: Vec<u8> = Vec::new();
        tail.extend_from_slice("done ".as_bytes());
        tail.extend_from_slice(&[0xe2, 0x94]);
        assert_eq!(decode(&mut tail), "done ");
        assert_eq!(tail, vec![0xe2, 0x94]);

        tail.extend_from_slice(&[0x80]);
        tail.extend_from_slice(" more\n".as_bytes());
        assert_eq!(decode(&mut tail), "\u{2500} more\n");
        assert!(tail.is_empty());

        // Bytes that will never be valid: one replacement character, and
        // everything after them still arrives.
        tail.extend_from_slice(&[0xff, 0xfe]);
        tail.extend_from_slice("after\n".as_bytes());
        assert_eq!(decode(&mut tail), "\u{FFFD}\u{FFFD}after\n");
        assert!(tail.is_empty());
    }

    /// Whatever the chunks were, the decoded stream is byte for byte what
    /// reading the whole pipe lossily would have produced - that text is
    /// still the summary when a provider prints one instead of writing it.
    #[test]
    fn decoding_in_chunks_reads_the_same_as_reading_it_all_at_once() {
        let bytes: Vec<u8> = b"one \xe2\x94\x80 two \xff\xfe three \xf0\x9f".to_vec();
        let whole = String::from_utf8_lossy(&bytes).into_owned();
        for size in 1..=bytes.len() {
            let mut tail: Vec<u8> = Vec::new();
            let mut text = String::new();
            for chunk in bytes.chunks(size) {
                tail.extend_from_slice(chunk);
                text.push_str(&decode(&mut tail));
            }
            text.push_str(&String::from_utf8_lossy(&tail));
            assert_eq!(text, whole, "chunks of {size}");
        }
    }

    #[test]
    fn a_line_is_held_back_until_its_newline_arrives() {
        let mut log = StreamLog::new();
        log.push(Origin::Out, "reading ");
        assert!(texts(&log).is_empty());
        log.push(Origin::Out, "files\nwriting");
        assert_eq!(texts(&log), vec!["reading files"]);
        log.flush();
        assert_eq!(texts(&log), vec!["reading files", "writing"]);
    }

    #[test]
    fn the_two_streams_buffer_independently() {
        let mut log = StreamLog::new();
        log.push(Origin::Out, "half of stdout");
        log.push(Origin::Err, "a whole stderr line\n");
        log.push(Origin::Out, " and the rest\n");
        let lines: Vec<(Origin, usize, String)> = log
            .lines()
            .map(|l| (l.origin, l.number, l.text.clone()))
            .collect();
        assert_eq!(
            lines,
            vec![
                (Origin::Err, 1, "a whole stderr line".to_string()),
                (Origin::Out, 2, "half of stdout and the rest".to_string()),
            ]
        );
    }

    #[test]
    fn numbering_counts_every_line_and_the_oldest_are_forgotten() {
        let mut log = StreamLog::new();
        for i in 1..=MAX_LINES + 10 {
            log.push(Origin::Out, &format!("line {i}\n"));
        }
        assert_eq!(log.total(), MAX_LINES + 10);
        assert_eq!(log.dropped(), 10);
        let first = log.lines().next().unwrap();
        assert_eq!(first.number, 11);
        assert_eq!(first.text, "line 11");
        let last = log.lines().last().unwrap();
        assert_eq!(last.number, MAX_LINES + 10);
    }

    /// A provider streaming one enormous line must not be able to grow
    /// the buffer without bound while it does.
    #[test]
    fn an_endless_line_is_committed_rather_than_buffered_forever() {
        let mut log = StreamLog::new();
        for _ in 0..4 {
            log.push(Origin::Out, &"x".repeat(MAX_PARTIAL / 2));
        }
        assert!(log.total() >= 2, "got {}", log.total());
        assert!(texts(&log).iter().all(|line| line.len() <= MAX_PARTIAL));
    }

    #[test]
    fn a_rewritten_line_keeps_only_what_the_terminal_would_show() {
        let mut log = StreamLog::new();
        log.push(Origin::Out, "10%\r50%\r100% done\n");
        log.push(Origin::Out, "carriage return line feed\r\n");
        assert_eq!(texts(&log), vec!["100% done", "carriage return line feed"]);
    }

    #[test]
    fn escape_sequences_do_not_survive_into_the_log() {
        let mut log = StreamLog::new();
        log.push(Origin::Err, "\u{1b}[1;31merror\u{1b}[0m: nope\n");
        log.push(Origin::Out, "\u{1b}]0;a title\u{7}plain\n");
        log.push(Origin::Out, "\u{1b}]8;;http://x\u{1b}\\link\n");
        log.push(Origin::Out, "\u{1b}(Bplain again\n");
        assert_eq!(
            texts(&log),
            vec!["error: nope", "plain", "link", "plain again"]
        );
        for line in log.lines() {
            assert!(!line.text.contains('\u{1b}'), "{:?}", line.text);
        }
    }

    #[test]
    fn a_truncated_escape_sequence_cannot_hang_the_stripper() {
        assert_eq!(strip_ansi("done\u{1b}"), "done");
        assert_eq!(strip_ansi("done\u{1b}["), "done");
        assert_eq!(strip_ansi("done\u{1b}]0;t"), "done");
        assert_eq!(strip_ansi("done\u{1b}("), "done");
    }

    /// Every shape the stripper knows, and text on both sides of it, so
    /// a sequence can never take a neighbouring character with it.
    #[test]
    fn every_escape_shape_is_removed_whole() {
        for (raw, want) in [
            ("a\u{1b}[0mb", "ab"),
            ("a\u{1b}[38;5;200mb", "ab"),
            ("a\u{1b}[2Kb", "ab"),
            ("a\u{1b}]0;title\u{7}b", "ab"),
            ("a\u{1b}]8;;http://x\u{1b}\\b", "ab"),
            ("a\u{1b}(Bb", "ab"),
            ("a\u{1b}=b", "ab"),
            ("a\u{1b}\u{1b}[0mb", "ab"),
            ("plain", "plain"),
        ] {
            assert_eq!(strip_ansi(raw), want, "{raw:?}");
        }
    }

    /// The session is read out of the stream as it goes, so the header
    /// can name it while the run is still going - and the first one seen
    /// is the run's own.
    #[test]
    fn the_session_is_picked_up_from_whichever_stream_announces_it() {
        let mut log = StreamLog::new();
        log.push(Origin::Out, "starting\n");
        assert_eq!(log.session(), None);
        log.push(Origin::Err, "session id: 01a04eef-d4a6-7232\n");
        assert_eq!(log.session(), Some("01a04eef-d4a6-7232"));
        log.push(Origin::Err, "session id: 99999999-9999-9999\n");
        assert_eq!(log.session(), Some("01a04eef-d4a6-7232"));
    }

    #[test]
    fn a_drawn_line_carries_its_number_and_its_stream() {
        let mut log = StreamLog::new();
        log.push(Origin::Out, "out\n");
        log.push(Origin::Err, "err\n");
        let drawn: Vec<String> = log.lines().map(StreamLine::numbered).collect();
        assert_eq!(drawn, vec!["    1 | out", "    2 ! err"]);
    }

    #[test]
    fn activity_reads_the_run_rather_than_guessing() {
        assert_eq!(activity(false, 10, Some(Duration::ZERO)), Activity::Ended);
        assert_eq!(activity(true, 0, None), Activity::Waiting);
        assert_eq!(
            activity(true, 3, Some(Duration::from_millis(200))),
            Activity::Streaming
        );
        assert_eq!(activity(true, 3, Some(THINKING_AFTER)), Activity::Thinking);
        // Every state says what it is in words.
        for state in [
            Activity::Waiting,
            Activity::Thinking,
            Activity::Streaming,
            Activity::Ended,
        ] {
            assert!(!state.label().is_empty());
        }
    }

    #[test]
    fn a_handle_is_only_laid_out_again_when_it_changes() {
        let handle = Handle::new();
        let first = handle
            .snapshot_since(0)
            .expect("a new handle has a version");
        assert_eq!(first.version, 1);
        assert!(first.lines.is_empty());
        assert_eq!(handle.snapshot_since(first.version), None);

        handle.append(Origin::Out, "hello\n");
        let second = handle.snapshot_since(first.version).expect("it changed");
        assert_eq!(
            second
                .lines
                .iter()
                .map(StreamLine::numbered)
                .collect::<Vec<_>>(),
            vec!["    1 | hello"]
        );
        assert_eq!(handle.snapshot_since(second.version), None);
    }

    #[test]
    fn ending_a_run_commits_its_last_line_once() {
        let handle = Handle::new();
        handle.append(Origin::Out, "no newline at the end");
        assert!(handle.running());
        handle.end();
        assert!(!handle.running());
        let version = handle.snapshot_since(0).unwrap().version;
        assert_eq!(
            handle
                .snapshot_since(0)
                .unwrap()
                .lines
                .iter()
                .map(StreamLine::numbered)
                .collect::<Vec<_>>(),
            vec!["    1 | no newline at the end"]
        );
        // Ending twice is not a change.
        handle.end();
        assert_eq!(handle.snapshot_since(version), None);
    }

    #[test]
    fn a_shared_handle_reports_what_the_writers_put_in_it() {
        let handle = Handle::new();
        let writer = handle.clone();
        let worker = std::thread::spawn(move || {
            for i in 1..=50 {
                writer.append(Origin::Out, &format!("line {i}\n"));
            }
            writer.append(Origin::Err, "session_id: abc-123456\n");
        });
        worker.join().unwrap();
        let state = handle.state(Instant::now());
        assert_eq!(state.total, 51);
        assert_eq!(state.session.as_deref(), Some("abc-123456"));
        assert_eq!(state.activity, Activity::Streaming);
        handle.end();
        assert_eq!(handle.state(Instant::now()).activity, Activity::Ended);
    }
}
