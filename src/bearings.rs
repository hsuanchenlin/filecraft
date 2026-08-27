//! Bearings: the pure orientation layer.
//!
//! Everything here is a total function of state Filecraft already holds -
//! the current path, the listing snapshot, the cursor, and the viewport
//! geometry. Nothing in this module touches the filesystem, the clock, or
//! the terminal: `now` is always passed in, so every bearing is
//! deterministic and testable without a TTY.
//!
//! The elements are deliberately paired with a textual dual, because the
//! project rule is that color - and now shape - is never the only signal:
//!
//! - [`ladder`] answers *where am I and how do I get back up* (dual: `depth N`).
//! - [`rail`] answers *how big is here and where am I in it* (dual: [`speakable_parts`]'s
//!   `rows A-B of N`).
//! - [`speakable_parts`] states the whole locus in words, on one row.

use std::path::{Component, Path, PathBuf};
use std::time::SystemTime;

use unicode_width::UnicodeWidthChar;

use crate::nav::{EntryKind, NavState};
use crate::preview::format_size;

/// The drawing characters the bearings are rendered with.
///
/// [`Glyphs::ASCII`] keeps the whole screen inside printable ASCII for
/// braille displays, serial terminals, and locales where the box-drawing
/// range is unreliable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Glyphs {
    /// Between two ladder rungs.
    pub ladder_sep: &'static str,
    /// The middle-elision mark in the ladder.
    pub ladder_gap: &'static str,
    /// Between a rung's digit and its label.
    pub rung_join: &'static str,
    /// The separator between spoken status segments.
    pub dot: &'static str,
    /// Rail cell inside the viewport.
    pub rail_thumb: &'static str,
    /// Rail cell outside the viewport.
    pub rail_track: &'static str,
    /// Prompt cursor block.
    pub caret: &'static str,
    /// Truncation mark appended by [`pad_to_width_with`].
    pub ellipsis: &'static str,
}

impl Glyphs {
    pub const UNICODE: Glyphs = Glyphs {
        ladder_sep: "▸",
        ladder_gap: "…",
        rung_join: "·",
        dot: "·",
        rail_thumb: "█",
        rail_track: "│",
        caret: "█",
        ellipsis: "…",
    };

    pub const ASCII: Glyphs = Glyphs {
        ladder_sep: ">",
        ladder_gap: "...",
        rung_join: ":",
        dot: "-",
        rail_thumb: "#",
        rail_track: "|",
        caret: "_",
        ellipsis: "~",
    };

    pub fn for_ascii(ascii: bool) -> Glyphs {
        if ascii {
            Glyphs::ASCII
        } else {
            Glyphs::UNICODE
        }
    }
}

/// Display columns `text` occupies. Wide (CJK) characters count as two,
/// so every column budget in this module is in real cells.
pub fn display_width(text: &str) -> usize {
    text.chars().map(|c| c.width().unwrap_or(0)).sum()
}

/// Replace control characters with `U+FFFD` so filesystem-derived names,
/// paths, and messages can never inject terminal escape sequences into
/// the screen. Display-only: stored names keep their real bytes so
/// move/rename/edit still operate on the actual file.
pub fn sanitize(text: &str) -> String {
    text.chars()
        .map(|c| if c.is_control() { '\u{FFFD}' } else { c })
        .collect()
}

/// Pad or truncate `text` to exactly `width` display columns, appending
/// `…` when truncated. Width-aware so CJK names keep columns aligned.
pub fn pad_to_width(text: &str, width: usize) -> String {
    pad_to_width_with(text, width, "…")
}

/// [`pad_to_width`] with a caller-chosen truncation mark, so ASCII mode
/// can spend three columns on `~` instead of one on `…`.
pub fn pad_to_width_with(text: &str, width: usize, ellipsis: &str) -> String {
    if width == 0 {
        return String::new();
    }
    let text_width = display_width(text);
    if text_width <= width {
        let mut out = text.to_string();
        out.extend(std::iter::repeat_n(' ', width - text_width));
        return out;
    }
    let mark_width = display_width(ellipsis);
    // No room for the mark: a hard cut still has to land on a cell boundary.
    let (mark, mark_width) = if mark_width >= width {
        ("", 0)
    } else {
        (ellipsis, mark_width)
    };
    let mut out = String::new();
    let mut used = 0usize;
    for c in text.chars() {
        let w = c.width().unwrap_or(0);
        if used + w > width - mark_width {
            break;
        }
        out.push(c);
        used += w;
    }
    out.push_str(mark);
    used += mark_width;
    while used < width {
        out.push(' ');
        used += 1;
    }
    out
}

/// Join `parts` with `sep`, dropping whole trailing parts that do not
/// fit. The result never exceeds `width` columns and never ends inside a
/// word - which is what makes the hint row safe at the documented 80x24
/// minimum.
pub fn fit_joined(parts: &[String], sep: &str, width: usize) -> String {
    let sep_width = display_width(sep);
    let mut out = String::new();
    let mut used = 0usize;
    for part in parts {
        let part_width = display_width(part);
        let extra = if out.is_empty() {
            part_width
        } else {
            sep_width + part_width
        };
        if used + extra > width {
            break;
        }
        if !out.is_empty() {
            out.push_str(sep);
        }
        out.push_str(part);
        used += extra;
    }
    // A single part wider than the whole row is better truncated than dropped.
    if out.is_empty() {
        if let Some(first) = parts.first() {
            out = pad_to_width(first, width).trim_end().to_string();
        }
    }
    out
}

/// One jumpable ancestor on the ladder. `digit` is literally the key that
/// jumps there.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rung {
    pub digit: u8,
    pub label: String,
    pub path: PathBuf,
}

/// The ancestor chain for the current directory, already fitted to a
/// column budget.
///
/// `elided` marks a middle-elision: the anchor (`~` or `/`) and the
/// current directory are always both present, so the ladder can shorten
/// but never loses the origin or the destination the way a clipped path
/// line does.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ladder {
    pub rungs: Vec<Rung>,
    pub elided: bool,
    /// Steps below the anchor, counted on the real path rather than on
    /// what fitted.
    pub depth: usize,
}

impl Ladder {
    /// The rung a digit key addresses, if that digit is on screen.
    pub fn rung(&self, digit: u8) -> Option<&Rung> {
        self.rungs.iter().find(|r| r.digit == digit)
    }
}

/// Digits available for jumping; `0` is always the anchor.
const MAX_RUNGS: usize = 10;

/// Steps between the anchor (`~` or `/`) and `cwd`. Independent of any
/// column budget, so the `depth N` dual is the true depth even when the
/// ladder middle-elides.
pub fn depth_of(cwd: &Path, home: Option<&Path>) -> usize {
    split_path(cwd, home).2.len()
}

/// Build the ancestor chain for `cwd`, fitted into `width` columns.
///
/// The anchor is `~` when `cwd` is inside `home`, otherwise the
/// filesystem root. Numbering is the depth of each ancestor while that
/// fits in a single digit; below ten steps the visible tail is renumbered
/// so every digit shown is a key that works.
pub fn ladder(cwd: &Path, home: Option<&Path>, width: usize, glyphs: &Glyphs) -> Ladder {
    let (anchor_label, anchor_path, steps) = split_path(cwd, home);
    let depth = steps.len();

    // The current directory is never dropped, so the ladder shortens
    // toward `anchor … current` but never loses either end.
    let floor = depth.min(1);
    let mut keep = depth.min(MAX_RUNGS - 1);
    loop {
        let candidate = assemble(&anchor_label, &anchor_path, &steps, keep, depth);
        if keep == floor || ladder_width(&candidate, glyphs) <= width {
            return candidate;
        }
        keep -= 1;
    }
}

/// Rendered width of a ladder, including separators and the elision mark.
pub fn ladder_width(ladder: &Ladder, glyphs: &Glyphs) -> usize {
    let mut items: Vec<usize> = ladder
        .rungs
        .iter()
        .map(|r| {
            display_width(&r.digit.to_string())
                + display_width(glyphs.rung_join)
                + display_width(&r.label)
        })
        .collect();
    if ladder.elided && !items.is_empty() {
        items.insert(1, display_width(glyphs.ladder_gap));
    }
    let sep = 1 + display_width(glyphs.ladder_sep) + 1;
    items.iter().sum::<usize>() + sep * items.len().saturating_sub(1)
}

/// Plain-text rendering of a ladder - the exact characters the screen
/// shows, so width assertions can be made without a terminal.
pub fn ladder_line(ladder: &Ladder, glyphs: &Glyphs) -> String {
    let sep = format!(" {} ", glyphs.ladder_sep);
    let mut items: Vec<String> = ladder
        .rungs
        .iter()
        .map(|r| format!("{}{}{}", r.digit, glyphs.rung_join, r.label))
        .collect();
    if ladder.elided && !items.is_empty() {
        items.insert(1, glyphs.ladder_gap.to_string());
    }
    items.join(&sep)
}

fn assemble(
    anchor_label: &str,
    anchor_path: &Path,
    steps: &[(String, PathBuf)],
    keep: usize,
    depth: usize,
) -> Ladder {
    let mut rungs = vec![Rung {
        digit: 0,
        label: anchor_label.to_string(),
        path: anchor_path.to_path_buf(),
    }];
    let start = depth - keep;
    for (offset, (label, path)) in steps[start..].iter().enumerate() {
        // Within nine steps the digit is the true depth; deeper, the
        // visible tail is renumbered so the last rung is always `9`.
        let digit = if depth < MAX_RUNGS {
            start + offset + 1
        } else {
            MAX_RUNGS - keep + offset
        };
        rungs.push(Rung {
            digit: digit as u8,
            label: label.clone(),
            path: path.clone(),
        });
    }
    Ladder {
        rungs,
        elided: keep < depth,
        depth,
    }
}

/// Split `cwd` into an anchor plus one entry per step below it.
fn split_path(cwd: &Path, home: Option<&Path>) -> (String, PathBuf, Vec<(String, PathBuf)>) {
    if let Some(home) = home {
        if let Ok(rest) = cwd.strip_prefix(home) {
            let mut steps = Vec::new();
            let mut path = home.to_path_buf();
            for component in rest.components() {
                path.push(component);
                steps.push((
                    sanitize(&component.as_os_str().to_string_lossy()),
                    path.clone(),
                ));
            }
            return ("~".to_string(), home.to_path_buf(), steps);
        }
    }
    let mut steps = Vec::new();
    let mut path = PathBuf::new();
    let mut anchor_label = String::from("/");
    let mut anchor_path = PathBuf::from("/");
    for (index, component) in cwd.components().enumerate() {
        match component {
            Component::RootDir if index == 0 => {
                path.push(component);
                anchor_path = path.clone();
            }
            // A relative or prefixed path still gets an anchor: its head.
            _ if index == 0 => {
                path.push(component);
                anchor_label = sanitize(&component.as_os_str().to_string_lossy());
                anchor_path = path.clone();
            }
            _ => {
                path.push(component);
                steps.push((
                    sanitize(&component.as_os_str().to_string_lossy()),
                    path.clone(),
                ));
            }
        }
    }
    (anchor_label, anchor_path, steps)
}

/// One cell of the position rail.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RailCell {
    /// Inside the viewport.
    Thumb,
    /// Elsewhere in the listing.
    Track,
}

impl RailCell {
    pub fn glyph(self, glyphs: &Glyphs) -> &'static str {
        match self {
            RailCell::Thumb => glyphs.rail_thumb,
            RailCell::Track => glyphs.rail_track,
        }
    }
}

/// Proportional position gutter: one cell per viewport row, thumb cells
/// covering the slice of the listing currently on screen.
///
/// When everything fits the rail is all track, because a full-height
/// thumb would claim there is somewhere else to be.
pub fn rail(total: usize, offset: usize, rows: usize) -> Vec<RailCell> {
    if rows == 0 {
        return Vec::new();
    }
    if total <= rows || total == 0 {
        return vec![RailCell::Track; rows];
    }
    let thumb = ((rows * rows) / total).max(1).min(rows);
    let travel = rows - thumb;
    let span = total - rows;
    let offset = offset.min(span);
    // Rounded so the thumb touches the top at offset 0 and the bottom at
    // the last page, with no off-by-one at either end.
    let start = if travel == 0 {
        0
    } else {
        (offset * travel + span / 2) / span
    };
    let start = start.min(travel);
    (0..rows)
        .map(|row| {
            if row >= start && row < start + thumb {
                RailCell::Thumb
            } else {
                RailCell::Track
            }
        })
        .collect()
}

/// First listing row to draw, keeping `margin` rows of lookahead below
/// the cursor so descending never pins it to the bottom edge.
///
/// Deterministic in the cursor alone - there is no scroll state to drift
/// out of sync with the selection.
pub fn viewport_offset(cursor: usize, total: usize, rows: usize, margin: usize) -> usize {
    if rows == 0 || total <= rows {
        return 0;
    }
    let max_offset = total - rows;
    let lookahead = rows.saturating_sub(1).saturating_sub(margin);
    cursor.saturating_sub(lookahead).min(max_offset)
}

/// 1-based inclusive row range on screen, or `None` when nothing is.
pub fn viewport_range(total: usize, offset: usize, rows: usize) -> Option<(usize, usize)> {
    if total == 0 || rows == 0 {
        return None;
    }
    let first = offset.min(total - 1);
    let last = (first + rows).min(total) - 1;
    Some((first + 1, last + 1))
}

/// Compact age of a timestamp: at most four columns, no timezone needed.
pub fn relative_time(now: SystemTime, then: SystemTime) -> String {
    let seconds = now
        .duration_since(then)
        .map(|d| d.as_secs())
        .unwrap_or_default();
    const MINUTE: u64 = 60;
    const HOUR: u64 = 60 * MINUTE;
    const DAY: u64 = 24 * HOUR;
    const WEEK: u64 = 7 * DAY;
    const YEAR: u64 = 365 * DAY;
    if seconds < MINUTE {
        format!("{seconds}s")
    } else if seconds < HOUR {
        format!("{}m", seconds / MINUTE)
    } else if seconds < DAY {
        format!("{}h", seconds / HOUR)
    } else if seconds < WEEK {
        format!("{}d", seconds / DAY)
    } else if seconds < YEAR {
        format!("{}w", seconds / WEEK)
    } else {
        format!("{}y", seconds / YEAR)
    }
}

/// The word for an entry's kind. Spoken, not drawn - the `/ @ @!`
/// markers stay the compact form in the listing itself.
pub fn kind_word(kind: &EntryKind, is_parent: bool) -> &'static str {
    if is_parent {
        return "parent directory";
    }
    match kind {
        EntryKind::Dir => "directory",
        EntryKind::File => "file",
        EntryKind::SymlinkDir => "symlink to directory",
        EntryKind::SymlinkFile => "symlink to file",
        EntryKind::SymlinkBroken => "broken symlink",
        EntryKind::Other => "special file",
    }
}

/// Everything the status row states in words, gathered from state that is
/// already in memory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Bearings {
    /// 1-based cursor row, or `None` when the listing shows nothing.
    pub row: Option<usize>,
    /// Rows the filter lets through.
    pub rows_total: usize,
    pub name: Option<String>,
    pub kind: Option<&'static str>,
    /// `None` for things whose size is not meaningful.
    pub size: Option<u64>,
    pub modified: Option<SystemTime>,
    /// 1-based inclusive range of rows on screen.
    pub viewport: Option<(usize, usize)>,
    pub filter: String,
    /// Real entries the filter matched (the `..` row is not a match).
    pub filter_matches: usize,
    /// Real entries in the directory.
    pub entries_total: usize,
    pub show_hidden: bool,
}

impl Bearings {
    /// Read the current locus off a [`NavState`]. No filesystem access:
    /// every field comes from the listing snapshot already in memory.
    pub fn from_nav(nav: &NavState, offset: usize, rows: usize) -> Bearings {
        let visible = nav.visible();
        let selected = visible.get(nav.cursor).map(|&i| &nav.entries[i]);
        let entries_total = nav.entries.iter().filter(|e| !e.is_parent).count();
        let filter_matches = visible
            .iter()
            .filter(|&&i| !nav.entries[i].is_parent)
            .count();
        Bearings {
            row: if visible.is_empty() {
                None
            } else {
                Some(nav.cursor + 1)
            },
            rows_total: visible.len(),
            name: selected.map(|e| sanitize(&e.display_name())),
            kind: selected.map(|e| kind_word(&e.kind, e.is_parent)),
            size: selected
                .filter(|e| !e.is_parent && !e.is_enterable())
                .map(|e| e.size),
            modified: selected.and_then(|e| e.modified),
            viewport: viewport_range(visible.len(), offset, rows),
            filter: sanitize(&nav.filter),
            filter_matches,
            entries_total,
            show_hidden: nav.show_hidden,
        }
    }
}

/// The speakable status line, as segments the caller joins with ` · `.
///
/// This is the textual dual of every graphic on screen: the rail's thumb
/// is `rows A-B of N`, the ladder's shape is `depth N`, and the cursor's
/// highlight is `row R of T`. Read aloud it is a sentence about exactly
/// one locus.
pub fn speakable_parts(bearings: &Bearings, now: SystemTime) -> Vec<String> {
    let mut parts = Vec::new();
    match bearings.row {
        Some(row) => parts.push(format!("row {row} of {}", bearings.rows_total)),
        None => parts.push("no rows".to_string()),
    }
    // Directly after the row count, because a filter is what makes that
    // count mean something other than the size of the directory.
    if !bearings.filter.is_empty() {
        parts.push(format!(
            "filter '{}': {} of {} match",
            bearings.filter, bearings.filter_matches, bearings.entries_total
        ));
    }
    if let Some(name) = &bearings.name {
        parts.push(name.clone());
    }
    if let Some(kind) = bearings.kind {
        parts.push(kind.to_string());
    }
    if let Some(size) = bearings.size {
        parts.push(format_size(size));
    }
    if let Some(modified) = bearings.modified {
        parts.push(format!("{} ago", relative_time(now, modified)));
    }
    match bearings.viewport {
        Some((first, last)) if first == 1 && last == bearings.rows_total => {
            parts.push("all rows shown".to_string())
        }
        Some((first, last)) => {
            parts.push(format!("rows {first}-{last} of {}", bearings.rows_total))
        }
        None => {}
    }
    if bearings.show_hidden {
        parts.push("dotfiles shown".to_string());
    }
    parts
}

/// True when a filter is active and nothing but the `..` row survived it.
/// The listing says so explicitly rather than showing a bare `../`.
pub fn filter_matched_nothing(bearings: &Bearings) -> bool {
    !bearings.filter.is_empty() && bearings.filter_matches == 0
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    const UNI: Glyphs = Glyphs::UNICODE;

    fn at(base: SystemTime, seconds: u64) -> SystemTime {
        base + Duration::from_secs(seconds)
    }

    #[test]
    fn ladder_anchors_on_home_and_numbers_every_step() {
        let home = Path::new("/Users/x");
        let ladder = ladder(
            Path::new("/Users/x/Projects/filecraft"),
            Some(home),
            80,
            &UNI,
        );
        assert_eq!(ladder.depth, 2);
        assert!(!ladder.elided);
        assert_eq!(
            ladder
                .rungs
                .iter()
                .map(|r| (r.digit, r.label.as_str()))
                .collect::<Vec<_>>(),
            vec![(0, "~"), (1, "Projects"), (2, "filecraft")]
        );
        assert_eq!(ladder_line(&ladder, &UNI), "0·~ ▸ 1·Projects ▸ 2·filecraft");
        assert_eq!(ladder.rung(1).unwrap().path, Path::new("/Users/x/Projects"));
    }

    #[test]
    fn ladder_anchors_on_root_outside_home() {
        let ladder = ladder(
            Path::new("/private/tmp"),
            Some(Path::new("/Users/x")),
            80,
            &UNI,
        );
        assert_eq!(ladder_line(&ladder, &UNI), "0·/ ▸ 1·private ▸ 2·tmp");
        assert_eq!(ladder.rung(0).unwrap().path, Path::new("/"));
    }

    #[test]
    fn ladder_at_the_anchor_is_just_the_anchor() {
        let home = Path::new("/Users/x");
        let ladder = ladder(home, Some(home), 80, &UNI);
        assert_eq!(ladder.depth, 0);
        assert!(!ladder.elided);
        assert_eq!(ladder_line(&ladder, &UNI), "0·~");

        let root = super::ladder(Path::new("/"), None, 80, &UNI);
        assert_eq!(root.depth, 0);
        assert_eq!(ladder_line(&root, &UNI), "0·/");
    }

    #[test]
    fn ladder_middle_elides_and_keeps_both_ends() {
        let home = Path::new("/Users/x");
        let deep =
            Path::new("/Users/x/Projects/clients/acme-holdings/2026/q3/deliverables/final/assets");
        let full = ladder(deep, Some(home), 200, &UNI);
        assert_eq!(full.depth, 8);
        assert!(!full.elided);

        let tight = ladder(deep, Some(home), 30, &UNI);
        assert!(tight.elided);
        assert_eq!(ladder_line(&tight, &UNI), "0·~ ▸ … ▸ 7·final ▸ 8·assets");
        assert!(ladder_width(&tight, &UNI) <= 30);
        // Origin and current location both survive; the middle is elided.
        assert_eq!(tight.rungs.first().unwrap().label, "~");
        assert_eq!(tight.rungs.last().unwrap().label, "assets");
        assert_eq!(tight.rungs.last().unwrap().path, deep);
    }

    #[test]
    fn ladder_never_shows_a_digit_it_cannot_jump_to() {
        let home = Path::new("/Users/x");
        let mut deep = home.to_path_buf();
        for i in 0..14 {
            deep.push(format!("level{i}"));
        }
        let ladder = ladder(&deep, Some(home), 200, &UNI);
        assert_eq!(ladder.depth, 14);
        assert!(ladder.elided);
        assert_eq!(ladder.rungs.len(), MAX_RUNGS);
        let digits: Vec<u8> = ladder.rungs.iter().map(|r| r.digit).collect();
        assert_eq!(digits, (0..=9).collect::<Vec<u8>>());
        // The last rung is always the current directory.
        assert_eq!(ladder.rungs.last().unwrap().path, deep);
        assert_eq!(ladder.rungs.last().unwrap().label, "level13");
    }

    #[test]
    fn ladder_shrinks_to_fit_any_budget() {
        let home = Path::new("/Users/x");
        let deep = Path::new("/Users/x/a/bb/ccc/dddd/eeeee/ffffff");
        for width in 1..60 {
            let ladder = ladder(deep, Some(home), width, &UNI);
            assert!(!ladder.rungs.is_empty(), "width {width} produced no rungs");
            assert_eq!(ladder.depth, 6);
            assert_eq!(ladder.rungs.last().unwrap().label, "ffffff");
            assert_eq!(ladder.rungs[0].label, "~");
            // `0·~ ▸ … ▸ 6·ffffff` is the shortest possible form: below
            // that the ladder keeps both ends and lets the row clip.
            if width >= 18 {
                assert!(
                    ladder_width(&ladder, &UNI) <= width,
                    "width {width} overflowed"
                );
            }
        }
    }

    #[test]
    fn ladder_sanitizes_control_characters_in_names() {
        let home = Path::new("/Users/x");
        let ladder = ladder(&home.join("evil\u{1b}[31m"), Some(home), 80, &UNI);
        let line = ladder_line(&ladder, &UNI);
        assert!(!line.contains('\u{1b}'));
        assert!(line.contains("evil\u{FFFD}[31m"));
    }

    #[test]
    fn ladder_renders_in_ascii() {
        let home = Path::new("/Users/x");
        let deep = Path::new("/Users/x/a/b/c/d/e/f/g/h");
        let line = ladder_line(
            &ladder(deep, Some(home), 24, &Glyphs::ASCII),
            &Glyphs::ASCII,
        );
        assert!(line.is_ascii(), "{line}");
        assert!(line.contains("..."));
        assert!(line.starts_with("0:~"));
    }

    #[test]
    fn rail_is_all_track_when_everything_fits() {
        assert_eq!(rail(12, 0, 18), vec![RailCell::Track; 18]);
        assert_eq!(rail(0, 0, 3), vec![RailCell::Track; 3]);
        assert!(rail(10, 0, 0).is_empty());
    }

    #[test]
    fn rail_thumb_tracks_the_viewport() {
        let top = rail(73, 0, 15);
        assert_eq!(top[0], RailCell::Thumb);
        assert_eq!(*top.last().unwrap(), RailCell::Track);

        let bottom = rail(73, 58, 15);
        assert_eq!(*bottom.last().unwrap(), RailCell::Thumb);
        assert_eq!(bottom[0], RailCell::Track);

        for offset in 0..=58 {
            let cells = rail(73, offset, 15);
            assert_eq!(cells.len(), 15);
            let thumbs = cells.iter().filter(|c| **c == RailCell::Thumb).count();
            assert!(
                (1..=15).contains(&thumbs),
                "offset {offset}: {thumbs} thumbs"
            );
            // The thumb is always one contiguous run.
            let first = cells.iter().position(|c| *c == RailCell::Thumb).unwrap();
            assert!(cells[first..first + thumbs]
                .iter()
                .all(|c| *c == RailCell::Thumb));
        }
    }

    #[test]
    fn viewport_offset_keeps_a_scroll_margin() {
        // Everything fits: no scrolling at all.
        assert_eq!(viewport_offset(11, 12, 18, 3), 0);
        // Cursor stays put until it reaches the margin.
        assert_eq!(viewport_offset(5, 73, 15, 3), 0);
        assert_eq!(viewport_offset(11, 73, 15, 3), 0);
        // Past it, three rows of lookahead stay below the cursor.
        let offset = viewport_offset(20, 73, 15, 3);
        assert_eq!(offset, 9);
        assert_eq!(20 - offset, 11);
        assert!(offset + 15 - 1 - 20 == 3);
        // The last page is not scrolled past.
        assert_eq!(viewport_offset(72, 73, 15, 3), 58);
        // A viewport smaller than the margin still shows the cursor.
        assert_eq!(viewport_offset(9, 20, 2, 3), 9);
    }

    #[test]
    fn viewport_range_is_one_based_and_inclusive() {
        assert_eq!(viewport_range(73, 58, 15), Some((59, 73)));
        assert_eq!(viewport_range(12, 0, 18), Some((1, 12)));
        assert_eq!(viewport_range(0, 0, 18), None);
        assert_eq!(viewport_range(10, 0, 0), None);
    }

    #[test]
    fn relative_time_is_compact_and_needs_no_timezone() {
        let base = SystemTime::UNIX_EPOCH + Duration::from_secs(1_800_000_000);
        assert_eq!(relative_time(base, base), "0s");
        assert_eq!(relative_time(at(base, 45), base), "45s");
        assert_eq!(relative_time(at(base, 60), base), "1m");
        assert_eq!(relative_time(at(base, 23 * 60), base), "23m");
        assert_eq!(relative_time(at(base, 3600), base), "1h");
        assert_eq!(relative_time(at(base, 2 * 86400), base), "2d");
        assert_eq!(relative_time(at(base, 8 * 86400), base), "1w");
        assert_eq!(relative_time(at(base, 400 * 86400), base), "1y");
        // A file dated in the future reads as brand new, never as an error.
        assert_eq!(relative_time(base, at(base, 500)), "0s");
        for seconds in [0, 59, 61, 3599, 90_000, 700_000, 40_000_000, 4_000_000_000] {
            assert!(relative_time(at(base, seconds), base).len() <= 7);
        }
    }

    #[test]
    fn fit_joined_never_breaks_a_word() {
        let parts: Vec<String> = ["j/k move", "l/Enter in", "h out", "0-9 jump"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert_eq!(
            fit_joined(&parts, " · ", 80),
            "j/k move · l/Enter in · h out · 0-9 jump"
        );
        let clipped = fit_joined(&parts, " · ", 25);
        assert_eq!(clipped, "j/k move · l/Enter in");
        assert!(display_width(&clipped) <= 25);
        // Every width produces a complete-word prefix, never a half word.
        for width in 0..60 {
            let line = fit_joined(&parts, " · ", width);
            assert!(display_width(&line) <= width.max(0));
            if !line.is_empty() && width >= 8 {
                assert!(
                    parts.iter().any(|p| line.ends_with(p.as_str())),
                    "width {width} ended mid-word: {line:?}"
                );
            }
        }
    }

    #[test]
    fn pad_to_width_with_ascii_mark() {
        assert_eq!(pad_to_width_with("abcdefg", 6, "~"), "abcde~");
        assert_eq!(pad_to_width_with("abc", 6, "~"), "abc   ");
        assert_eq!(pad_to_width_with("abcdefg", 2, "..."), "ab");
        assert_eq!(display_width(&pad_to_width_with("檔案名稱很長", 7, "~")), 7);
    }

    #[test]
    fn kind_words_cover_every_kind() {
        assert_eq!(kind_word(&EntryKind::Dir, true), "parent directory");
        assert_eq!(kind_word(&EntryKind::Dir, false), "directory");
        assert_eq!(kind_word(&EntryKind::File, false), "file");
        assert_eq!(
            kind_word(&EntryKind::SymlinkDir, false),
            "symlink to directory"
        );
        assert_eq!(kind_word(&EntryKind::SymlinkFile, false), "symlink to file");
        assert_eq!(
            kind_word(&EntryKind::SymlinkBroken, false),
            "broken symlink"
        );
        assert_eq!(kind_word(&EntryKind::Other, false), "special file");
    }

    fn bearings_fixture() -> Bearings {
        Bearings {
            row: Some(73),
            rows_total: 73,
            name: Some("file_060.txt".to_string()),
            kind: Some("file"),
            size: Some(0),
            modified: None,
            viewport: Some((59, 73)),
            filter: String::new(),
            filter_matches: 0,
            entries_total: 73,
            show_hidden: false,
        }
    }

    #[test]
    fn speakable_states_the_whole_locus_in_words() {
        let base = SystemTime::UNIX_EPOCH + Duration::from_secs(1_800_000_000);
        let mut bearings = bearings_fixture();
        bearings.modified = Some(base);
        let line = speakable_parts(&bearings, at(base, 3600)).join(" · ");
        assert_eq!(
            line,
            "row 73 of 73 · file_060.txt · file · 0B · 1h ago · rows 59-73 of 73"
        );
    }

    #[test]
    fn speakable_says_all_rows_shown_when_nothing_is_off_screen() {
        let now = SystemTime::UNIX_EPOCH;
        let bearings = Bearings {
            row: Some(4),
            rows_total: 12,
            name: Some("src/".to_string()),
            kind: Some("directory"),
            size: None,
            modified: None,
            viewport: Some((1, 12)),
            ..bearings_fixture()
        };
        let parts = speakable_parts(&bearings, now);
        assert_eq!(
            parts.join(" · "),
            "row 4 of 12 · src/ · directory · all rows shown"
        );
    }

    #[test]
    fn speakable_reports_a_filter_that_matched_nothing() {
        let now = SystemTime::UNIX_EPOCH;
        let bearings = Bearings {
            row: Some(1),
            rows_total: 1,
            name: Some("../".to_string()),
            kind: Some("parent directory"),
            size: None,
            modified: None,
            viewport: Some((1, 1)),
            filter: "zzz".to_string(),
            filter_matches: 0,
            entries_total: 12,
            show_hidden: false,
        };
        assert!(filter_matched_nothing(&bearings));
        assert!(speakable_parts(&bearings, now)
            .join(" · ")
            .contains("filter 'zzz': 0 of 12 match"));

        let matching = Bearings {
            filter: "app".to_string(),
            filter_matches: 2,
            ..bearings
        };
        assert!(!filter_matched_nothing(&matching));
        assert!(speakable_parts(&matching, now)
            .join(" · ")
            .contains("filter 'app': 2 of 12 match"));
    }

    #[test]
    fn speakable_reports_an_empty_listing_and_dotfiles() {
        let now = SystemTime::UNIX_EPOCH;
        let bearings = Bearings {
            row: None,
            rows_total: 0,
            name: None,
            kind: None,
            size: None,
            modified: None,
            viewport: None,
            filter: String::new(),
            filter_matches: 0,
            entries_total: 0,
            show_hidden: true,
        };
        assert_eq!(
            speakable_parts(&bearings, now).join(" · "),
            "no rows · dotfiles shown"
        );
    }

    #[test]
    fn bearings_from_nav_reads_only_state_in_memory() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir(tmp.path().join("sub")).unwrap();
        std::fs::write(tmp.path().join("note.txt"), "hello").unwrap();
        let mut nav = NavState::new(tmp.path()).unwrap();
        nav.set_filter("note".to_string());
        nav.move_cursor(1);

        let bearings = Bearings::from_nav(&nav, 0, 10);
        assert_eq!(bearings.row, Some(2));
        assert_eq!(bearings.rows_total, 2); // ".." plus the match
        assert_eq!(bearings.name.as_deref(), Some("note.txt"));
        assert_eq!(bearings.kind, Some("file"));
        assert_eq!(bearings.size, Some(5));
        assert_eq!(bearings.entries_total, 2);
        assert_eq!(bearings.filter_matches, 1);
        assert_eq!(bearings.viewport, Some((1, 2)));

        nav.set_filter("nothing-here".to_string());
        let bearings = Bearings::from_nav(&nav, 0, 10);
        assert!(filter_matched_nothing(&bearings));
        assert_eq!(bearings.rows_total, 1); // the ".." row still passes
    }

    #[test]
    fn bearings_size_is_omitted_for_directories_and_the_parent_row() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir(tmp.path().join("sub")).unwrap();
        let nav = NavState::new(tmp.path()).unwrap();
        let bearings = Bearings::from_nav(&nav, 0, 10);
        assert_eq!(bearings.kind, Some("parent directory"));
        assert_eq!(bearings.size, None);

        let mut nav = nav;
        nav.move_cursor(1);
        let bearings = Bearings::from_nav(&nav, 0, 10);
        assert_eq!(bearings.kind, Some("directory"));
        assert_eq!(bearings.size, None);
    }
}
