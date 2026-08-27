//! BBS-style screen rendering with ratatui.
//!
//! One stable full-screen layout: banner border, ancestor ladder, listing
//! with a position rail, speakable status line, message log, command
//! prompt, and a one-line key hint. High contrast by design: selection
//! uses reverse video, kinds carry textual markers (`/`, `@`, `@!`) and
//! message levels carry textual prefixes, so color is never the only
//! signal. Colors are the terminal's own ANSI palette and can be disabled
//! entirely with `NO_COLOR`; `FILECRAFT_ASCII` additionally drops every
//! drawing character outside printable ASCII.
//!
//! The screen has two halves and the boundary is a rule, not a habit:
//! **everything above the listing is orientation only** - it cannot be
//! focused and no key that starts there mutates anything - and the
//! listing is the single operating locus every command acts on.
//!
//! All the arithmetic lives in [`crate::bearings`], which is pure; this
//! module only turns it into spans.

use std::fmt::Write as _;
use std::path::Path;
use std::time::SystemTime;

use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::symbols::border;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Clear, Paragraph};
use ratatui::Frame;

use crate::app::{App, Level, Mode};
use crate::bearings::{
    self, display_width, pad_to_width, pad_to_width_with, sanitize, Bearings, Glyphs, RailCell,
};
use crate::fsops::FsError;
use crate::nav::{EntryKind, NavState};
use crate::preview::{format_size, format_timestamp};

/// Rows used by everything except the file list (borders, path, status,
/// messages, prompt, hints). `main` uses this to size PageUp/PageDown.
pub const CHROME_ROWS: u16 = 9;

/// Columns the left and right borders take; `main` uses this to tell the
/// app how wide the ladder row really is.
pub const BORDER_COLS: u16 = 2;

/// How many recent messages the BBS log shows. `M` opens the rest.
const MESSAGE_ROWS: usize = 3;

/// Rows of lookahead kept below the cursor, so descending never pins the
/// selection to the bottom edge.
const SCROLL_MARGIN: usize = 3;

/// Columns the listing spends on everything that is not the name: the
/// rail, the cursor marker, the size field, and the relative time.
const LISTING_FURNITURE: usize = 1 + 2 + 8 + 7;

/// Styling switchboard. With `use_color: false` (the `NO_COLOR`
/// convention) every style falls back to bold/reverse on the terminal's
/// default colors.
#[derive(Debug, Clone, Copy)]
pub struct Theme {
    pub use_color: bool,
    /// Restrict every drawing character to printable ASCII.
    pub ascii: bool,
}

impl Theme {
    /// `NO_COLOR` disables color when set to any non-empty value.
    pub fn from_no_color_env(no_color: Option<&str>) -> Self {
        Theme {
            use_color: no_color.is_none_or(str::is_empty),
            ascii: false,
        }
    }

    /// `FILECRAFT_ASCII` set to any non-empty value swaps the box-drawing
    /// and block characters for ASCII, for braille displays, serial
    /// terminals, and locales where UTF-8 is not reliable.
    pub fn from_env(no_color: Option<&str>, ascii: Option<&str>) -> Self {
        Theme {
            ascii: ascii.is_some_and(|value| !value.is_empty()),
            ..Theme::from_no_color_env(no_color)
        }
    }

    /// The drawing characters in force.
    pub fn glyphs(&self) -> Glyphs {
        Glyphs::for_ascii(self.ascii)
    }

    fn border_set(&self) -> border::Set {
        if self.ascii {
            border::Set {
                top_left: "+",
                top_right: "+",
                bottom_left: "+",
                bottom_right: "+",
                vertical_left: "|",
                vertical_right: "|",
                horizontal_top: "-",
                horizontal_bottom: "-",
            }
        } else {
            BorderType::Double.to_border_set()
        }
    }

    fn pager_border_set(&self) -> border::Set {
        if self.ascii {
            self.border_set()
        } else {
            BorderType::Plain.to_border_set()
        }
    }

    fn banner_title(&self) -> String {
        let version = env!("CARGO_PKG_VERSION");
        if self.ascii {
            format!(" === FILECRAFT v{version} === ")
        } else {
            format!(" ░▒▓ FILECRAFT v{version} ▓▒░ ")
        }
    }

    fn color(&self, style: Style) -> Style {
        if self.use_color {
            style
        } else {
            Style::default().add_modifier(
                style
                    .add_modifier
                    .intersection(Modifier::BOLD | Modifier::REVERSED | Modifier::UNDERLINED),
            )
        }
    }

    /// Orientation chrome. The ladder is a chain of directories, so it
    /// reads in the directory style - but it is never focusable.
    pub fn bearing(&self) -> Style {
        self.dir()
    }

    pub fn banner(&self) -> Style {
        self.color(
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        )
    }

    pub fn selected(&self) -> Style {
        // Reverse video: readable on every terminal, color or not.
        Style::default().add_modifier(Modifier::REVERSED | Modifier::BOLD)
    }

    pub fn dir(&self) -> Style {
        self.color(
            Style::default()
                .fg(Color::Blue)
                .add_modifier(Modifier::BOLD),
        )
    }

    pub fn symlink(&self) -> Style {
        self.color(Style::default().fg(Color::Cyan))
    }

    pub fn broken(&self) -> Style {
        self.color(Style::default().fg(Color::Red).add_modifier(Modifier::BOLD))
    }

    pub fn error(&self) -> Style {
        self.color(Style::default().fg(Color::Red).add_modifier(Modifier::BOLD))
    }

    pub fn ok(&self) -> Style {
        self.color(Style::default().fg(Color::Green))
    }

    pub fn prompt(&self) -> Style {
        self.color(
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        )
    }

    pub fn confirm(&self) -> Style {
        self.color(
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD | Modifier::REVERSED),
        )
    }
}

/// Static, TTY-free listing used when stdin/stdout are not a terminal
/// (or when `--list` is passed). Mirrors the interactive list: current
/// directory, textual kind markers, sizes, and a compact key hint.
pub fn render_static_listing(dir: &Path) -> Result<String, FsError> {
    let nav = NavState::new(dir)?;
    let mut out = String::new();
    let _ = writeln!(
        out,
        "filecraft {}  {}",
        env!("CARGO_PKG_VERSION"),
        sanitize(&nav.cwd.display().to_string())
    );
    let _ = writeln!(
        out,
        "static listing (no TTY). run in a real terminal for the interactive BBS screen."
    );
    let _ = writeln!(out);

    let visible = nav.visible();
    if visible.is_empty() {
        let _ = writeln!(out, "  (empty directory)");
    } else {
        for &i in &visible {
            let entry = &nav.entries[i];
            let size = if entry.is_parent || entry.is_enterable() {
                "<DIR>".to_string()
            } else {
                format_size(entry.size)
            };
            let date = entry.modified.map(format_timestamp).unwrap_or_default();
            let name = pad_to_width(&sanitize(&entry.display_name()), 40);
            let _ = writeln!(out, "  {name} {size:>8}  {date}");
        }
    }
    let _ = writeln!(out);
    let _ = writeln!(
        out,
        "keys: j/k move  Enter/l open  Backspace/h up  / filter  : cmd  ? help  q quit"
    );
    Ok(out)
}

/// Render the whole screen for the current state.
pub fn draw(frame: &mut Frame<'_>, app: &App, theme: &Theme) {
    draw_at(frame, app, theme, SystemTime::now());
}

/// [`draw`] with the clock injected, so golden-frame tests are
/// deterministic and relative times are reproducible.
pub fn draw_at(frame: &mut Frame<'_>, app: &App, theme: &Theme, now: SystemTime) {
    let area = frame.area();
    let outer = Block::default()
        .borders(Borders::ALL)
        .border_set(theme.border_set())
        .border_style(theme.banner())
        .title(Line::from(Span::styled(
            theme.banner_title(),
            theme.banner(),
        )));
    let inner = outer.inner(area);
    frame.render_widget(outer, area);

    let [ladder_row, list_area, status_row, message_area, prompt_row, hint_row] =
        Layout::vertical([
            Constraint::Length(1),
            Constraint::Min(1),
            Constraint::Length(1),
            Constraint::Length(MESSAGE_ROWS as u16),
            Constraint::Length(1),
            Constraint::Length(1),
        ])
        .areas(inner);

    draw_ladder(frame, app, theme, ladder_row);
    let rows = list_area.height as usize;
    let visible = app.nav.visible();
    let offset = bearings::viewport_offset(app.nav.cursor, visible.len(), rows, SCROLL_MARGIN);
    // One reading of the locus, shared by the listing and the words that
    // describe it, so the two can never disagree.
    let bearings = Bearings::from_nav(&app.nav, offset, rows);
    match &app.mode {
        Mode::Pager(pager) => draw_pager(frame, theme, list_area, pager),
        _ => draw_listing(
            frame, app, theme, list_area, &visible, offset, &bearings, now,
        ),
    }
    draw_status(frame, theme, status_row, &bearings, now);
    draw_messages(frame, app, theme, message_area);
    draw_prompt(frame, app, theme, prompt_row);
    draw_hints(frame, app, theme, hint_row);
}

/// The ancestor ladder: a numbered, keyboard-jumpable chain that
/// middle-elides instead of clipping, so the anchor and the current
/// directory are both always on screen. Read-only - digits jump, nothing
/// here can be operated on.
fn draw_ladder(frame: &mut Frame<'_>, app: &App, theme: &Theme, area: Rect) {
    let glyphs = theme.glyphs();
    let width = area.width as usize;
    let summary = app.ladder_summary_with(&glyphs);
    let layout = bearings::ladder_row(width, display_width(&summary));
    let chain = bearings::ladder_line(&app.ladder_in(width, &glyphs), &glyphs);
    let line = Line::from(vec![
        Span::raw(" "),
        Span::styled(
            pad_to_width_with(&chain, layout.chain_width, glyphs.ellipsis),
            theme.bearing(),
        ),
        Span::raw(if layout.show_summary {
            summary
        } else {
            String::new()
        }),
    ]);
    frame.render_widget(Paragraph::new(line), area);
}

#[allow(clippy::too_many_arguments)]
fn draw_listing(
    frame: &mut Frame<'_>,
    app: &App,
    theme: &Theme,
    area: Rect,
    visible: &[usize],
    offset: usize,
    bearings: &Bearings,
    now: SystemTime,
) {
    let rows = area.height as usize;
    if rows == 0 {
        return;
    }
    let glyphs = theme.glyphs();
    let width = area.width as usize;
    let name_width = width.saturating_sub(LISTING_FURNITURE);
    // Every listing row is the rail plus this much content.
    let body_width = width.saturating_sub(1);
    let rail = bearings::rail(visible.len(), offset, rows);
    // A filter that matched nothing must say so: the `..` row always
    // passes, so a bare `../` would otherwise look like a real result.
    let note = if visible.is_empty() {
        Some("(no matching entries)".to_string())
    } else if bearings::filter_matched_nothing(bearings) {
        Some(format!("(no entries match '{}')", bearings.filter))
    } else {
        None
    };

    let mut lines: Vec<Line> = Vec::with_capacity(rows);
    for row in 0..rows {
        let rail_span = Span::styled(
            rail.get(row)
                .copied()
                .unwrap_or(RailCell::Track)
                .glyph(&glyphs),
            theme.bearing(),
        );
        let index = offset + row;
        let Some(&entry_index) = visible.get(index) else {
            // The note sits directly under the last row that survived.
            let filler = match (&note, index == visible.len()) {
                (Some(text), true) => {
                    pad_to_width_with(&format!("  {text}"), body_width, glyphs.ellipsis)
                }
                _ => " ".repeat(body_width),
            };
            lines.push(Line::from(vec![rail_span, Span::raw(filler)]));
            continue;
        };
        let entry = &app.nav.entries[entry_index];
        let selected = index == app.nav.cursor;
        let marker = if selected { "> " } else { "  " };

        let size = if entry.is_parent || entry.is_enterable() {
            "<DIR>".to_string()
        } else {
            format_size(entry.size)
        };
        // Relative time needs no timezone, and costs seven columns
        // instead of twenty - which is what pays for the rail.
        let age = entry
            .modified
            .map(|m| bearings::relative_time(now, m))
            .unwrap_or_default();
        let name = pad_to_width_with(
            &sanitize(&entry.display_name()),
            name_width,
            glyphs.ellipsis,
        );

        let base_style = match entry.kind {
            _ if selected => theme.selected(),
            EntryKind::Dir => theme.dir(),
            EntryKind::SymlinkDir | EntryKind::SymlinkFile => theme.symlink(),
            EntryKind::SymlinkBroken => theme.broken(),
            _ => Style::default(),
        };
        lines.push(Line::from(vec![
            rail_span,
            Span::styled(marker.to_string(), base_style),
            Span::styled(name, base_style),
            Span::styled(format!(" {size:>6} "), base_style),
            Span::styled(format!(" {age:<6}"), base_style),
        ]));
    }
    frame.render_widget(Paragraph::new(lines), area);
}

fn draw_pager(frame: &mut Frame<'_>, theme: &Theme, area: Rect, pager: &crate::app::Pager) {
    let block = Block::default()
        .borders(Borders::ALL)
        .border_set(theme.pager_border_set())
        .border_style(theme.banner())
        .title(Span::styled(
            format!(" {} ", sanitize(&pager.title)),
            theme.prompt(),
        ));
    let inner = block.inner(area);
    frame.render_widget(Clear, area);
    frame.render_widget(block, area);

    let rows = inner.height as usize;
    let lines: Vec<Line> = pager
        .lines
        .iter()
        .skip(pager.scroll)
        .take(rows)
        .map(|l| Line::from(Span::raw(sanitize(l))))
        .collect();
    frame.render_widget(Paragraph::new(lines), inner);
}

/// The speakable status row: the whole locus in words, on a row that
/// never moves. It is the textual dual of the rail and the ladder, so
/// nothing on screen is carried by shape or color alone.
fn draw_status(
    frame: &mut Frame<'_>,
    theme: &Theme,
    area: Rect,
    bearings: &Bearings,
    now: SystemTime,
) {
    let glyphs = theme.glyphs();
    let speakable = bearings::speakable(bearings, now);
    let separator = format!(" {} ", glyphs.dot);
    let text = bearings::fit_joined_pinned(
        &speakable.parts,
        &separator,
        (area.width as usize).saturating_sub(1),
        glyphs.ellipsis,
        speakable.pinned,
    );
    frame.render_widget(
        Paragraph::new(Line::from(Span::raw(format!(" {text}")))),
        area,
    );
}

fn draw_messages(frame: &mut Frame<'_>, app: &App, theme: &Theme, area: Rect) {
    let glyphs = theme.glyphs();
    // Five columns of level prefix, one of padding.
    let text_width = (area.width as usize).saturating_sub(6);
    let start = app.messages.len().saturating_sub(MESSAGE_ROWS);
    let lines: Vec<Line> = app.messages[start..]
        .iter()
        .map(|message| {
            let style = match message.level {
                Level::Info => Style::default(),
                Level::Ok => theme.ok(),
                Level::Error => theme.error(),
            };
            let text = pad_to_width_with(&sanitize(&message.text), text_width, glyphs.ellipsis);
            Line::from(vec![
                Span::styled(message.level.prefix(&glyphs), style),
                Span::styled(format!(" {}", text.trim_end()), style),
            ])
        })
        .collect();
    frame.render_widget(Paragraph::new(lines), area);
}

fn draw_prompt(frame: &mut Frame<'_>, app: &App, theme: &Theme, area: Rect) {
    let glyphs = theme.glyphs();
    let caret = glyphs.caret;
    let line = match &app.mode {
        Mode::Command { input } => Line::from(vec![
            Span::styled(" cmd> ", theme.prompt()),
            Span::raw(sanitize(input)),
            Span::styled(caret, theme.prompt()),
        ]),
        Mode::Filter { input } => Line::from(vec![
            Span::styled(" filter> ", theme.prompt()),
            Span::raw(sanitize(input)),
            Span::styled(caret, theme.prompt()),
        ]),
        Mode::ConfirmOp => {
            let description = app
                .pending
                .as_ref()
                .map(|op| op.describe())
                .unwrap_or_default();
            Line::from(vec![
                Span::styled(" confirm ", theme.confirm()),
                Span::styled("[y]es / [n]o  ", theme.prompt()),
                Span::raw(sanitize(&description)),
            ])
        }
        Mode::Pager(_) => Line::from(vec![
            Span::styled(" view ", theme.prompt()),
            Span::raw(format!(
                " j/k scroll {dot} g/G top/bottom {dot} q close",
                dot = glyphs.dot
            )),
        ]),
        Mode::Browse => Line::from(vec![
            Span::styled(" cmd> ", theme.prompt()),
            Span::raw("press : to type a command"),
        ]),
    };
    frame.render_widget(Paragraph::new(line), area);
}

/// Mode-appropriate keys, fitted by dropping whole hints. The row never
/// ends inside a word, including at the documented 80x24 minimum.
fn draw_hints(frame: &mut Frame<'_>, app: &App, theme: &Theme, area: Rect) {
    let hints: &[&str] = match &app.mode {
        // Ordered by how often they are needed: what falls off a narrow
        // terminal is what the user needs least.
        Mode::Browse => &[
            "j/k move",
            "l/Enter in",
            "h out",
            "0-9 jump",
            "/ find",
            ": cmd",
            "? help",
            "q quit",
            ". dotfiles",
            "M log",
        ],
        Mode::Command { .. } => &[
            "Enter run",
            "Esc cancel",
            "try: help, cd, move, rename, preview",
        ],
        Mode::Filter { .. } => &["type to filter", "Enter keep", "Esc clear"],
        Mode::ConfirmOp => &["y confirm", "n cancel", "nothing happens without y"],
        Mode::Pager(_) => &["j/k scroll", "PgUp/PgDn page", "q/Esc back to files"],
    };
    let hints: Vec<String> = hints.iter().map(|h| (*h).to_string()).collect();
    let glyphs = theme.glyphs();
    let separator = format!(" {} ", glyphs.dot);
    let text = bearings::fit_joined(
        &hints,
        &separator,
        (area.width as usize).saturating_sub(1),
        glyphs.ellipsis,
    );
    frame.render_widget(
        Paragraph::new(Line::from(Span::raw(format!(" {text}")))),
        area,
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::{App, KeyInput};
    use crate::bearings::display_width;
    use crate::nav::NavState;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;
    use std::fs;
    use unicode_width::UnicodeWidthChar;

    fn render(app: &App) -> String {
        render_size(app, 90, 30)
    }

    fn render_size(app: &App, width: u16, height: u16) -> String {
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).unwrap();
        let theme = Theme::from_no_color_env(None);
        terminal.draw(|f| draw(f, app, &theme)).unwrap();
        buffer_text(terminal.backend().buffer())
    }

    /// The cell buffer as text. A wide character owns two cells; only the
    /// first carries the symbol, so the text keeps the same display width
    /// as the screen.
    fn buffer_text(buffer: &ratatui::buffer::Buffer) -> String {
        let mut out = String::new();
        for y in 0..buffer.area.height {
            let mut x = 0;
            while x < buffer.area.width {
                let symbol = buffer[(x, y)].symbol();
                out.push_str(symbol);
                x += display_width(symbol).max(1) as u16;
            }
            out.push('\n');
        }
        out
    }

    /// Render with an explicit theme and geometry, exactly as `main`
    /// drives it: the app is told the same width the frame has.
    fn render_themed(app: &mut App, width: u16, height: u16, theme: &Theme) -> String {
        app.viewport_rows = height.saturating_sub(CHROME_ROWS).max(1) as usize;
        app.viewport_cols = width.saturating_sub(BORDER_COLS) as usize;
        app.glyphs = theme.glyphs();
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).unwrap();
        // An hour after the newest entry: every fixture then reads as
        // `1h`, whatever wall clock the test runs on.
        let now = app
            .nav
            .entries
            .iter()
            .filter_map(|e| e.modified)
            .max()
            .unwrap_or(SystemTime::UNIX_EPOCH)
            + std::time::Duration::from_secs(3600);
        terminal.draw(|f| draw_at(f, app, theme, now)).unwrap();
        buffer_text(terminal.backend().buffer())
    }

    fn fixture_app() -> (tempfile::TempDir, App) {
        let tmp = tempfile::tempdir().unwrap();
        fs::create_dir(tmp.path().join("projects")).unwrap();
        fs::write(tmp.path().join("readme.md"), "hi").unwrap();
        let nav = NavState::new(tmp.path()).unwrap();
        (tmp, App::new(nav, None, false, None))
    }

    fn listing_fixture(entries: usize) -> tempfile::TempDir {
        let tmp = tempfile::tempdir().unwrap();
        fs::create_dir(tmp.path().join("nested")).unwrap();
        for i in 1..=entries {
            fs::write(tmp.path().join(format!("file_{i:03}.txt")), "").unwrap();
        }
        tmp
    }

    fn app_at(dir: &Path) -> App {
        let home = dir.canonicalize().unwrap().parent().unwrap().to_path_buf();
        App::new(NavState::new(dir).unwrap(), None, false, Some(home))
    }

    fn row(screen: &str, index: usize) -> String {
        screen.lines().nth(index).unwrap().to_string()
    }

    const SIZES: [(u16, u16); 4] = [(80, 24), (100, 30), (132, 40), (60, 20)];

    /// Frame row of the speakable status line: it is a fixed distance
    /// from the bottom, which is what makes "read the current line" work.
    fn status_row(height: u16) -> usize {
        height as usize - 7
    }

    #[test]
    fn every_frame_size_keeps_its_border_and_row_width() {
        let tmp = listing_fixture(73);
        let mut app = app_at(tmp.path());
        app.nav.cursor_to_end();
        for (width, height) in SIZES {
            for theme in [
                Theme::from_no_color_env(None),
                Theme::from_no_color_env(Some("1")),
                Theme::from_env(None, Some("1")),
            ] {
                let screen = render_themed(&mut app, width, height, &theme);
                let lines: Vec<&str> = screen.lines().collect();
                assert_eq!(lines.len(), height as usize);
                for (index, line) in lines.iter().enumerate() {
                    assert_eq!(
                        display_width(line),
                        width as usize,
                        "{width}x{height} row {index} is the wrong width: {line:?}"
                    );
                    let last = line.chars().last().unwrap();
                    let expected: &[char] = if theme.ascii {
                        &['|', '+']
                    } else {
                        &['║', '╗', '╝']
                    };
                    assert!(
                        expected.contains(&last),
                        "{width}x{height} row {index} lost its right border: {line:?}"
                    );
                }
            }
        }
    }

    #[test]
    fn hint_row_never_breaks_a_word_at_any_size() {
        let tmp = listing_fixture(4);
        let mut app = app_at(tmp.path());
        // The complete browse hint vocabulary; the row may drop hints but
        // must never cut one in half.
        let words = [
            "j/k move",
            "l/Enter in",
            "h out",
            "0-9 jump",
            "/ find",
            ": cmd",
            "? help",
            "q quit",
            ". dotfiles",
            "M log",
        ];
        for (width, height) in SIZES {
            let theme = Theme::from_no_color_env(None);
            let screen = render_themed(&mut app, width, height, &theme);
            let hints = row(&screen, height as usize - 2);
            let hints = hints.trim_matches(['║', ' ']);
            assert!(!hints.is_empty(), "{width}x{height} lost the hint row");
            assert!(
                words.iter().any(|w| hints.ends_with(w)),
                "{width}x{height} hint row ended mid-word: {hints:?}"
            );
            assert!(hints.starts_with("j/k move"));
        }
    }

    #[test]
    fn ladder_replaces_the_clipped_path_and_keeps_both_ends() {
        let tmp = tempfile::tempdir().unwrap();
        let deep = tmp
            .path()
            .join("clients")
            .join("acme-holdings")
            .join("2026")
            .join("q3")
            .join("deliverables")
            .join("final")
            .join("assets");
        fs::create_dir_all(&deep).unwrap();
        let home = tmp.path().canonicalize().unwrap();
        let mut app = App::new(NavState::new(&deep).unwrap(), None, false, Some(home));
        let theme = Theme::from_no_color_env(None);
        let screen = render_themed(&mut app, 80, 24, &theme);
        let ladder = row(&screen, 1);

        assert!(ladder.contains("0·~"), "{ladder}");
        assert!(ladder.contains("…"), "deep path should elide: {ladder}");
        assert!(ladder.contains("7·assets"), "{ladder}");
        assert!(ladder.contains("depth 7"), "{ladder}");
        // Every digit drawn is a key that works.
        let drawn: Vec<u8> = app.ladder().rungs.iter().map(|r| r.digit).collect();
        for digit in drawn {
            assert!(
                ladder.contains(&format!("{digit}·")),
                "digit {digit} is jumpable but not drawn: {ladder}"
            );
        }
    }

    #[test]
    fn rail_shows_position_and_the_status_row_says_it_in_words() {
        let tmp = listing_fixture(73);
        let mut app = app_at(tmp.path());
        let theme = Theme::from_no_color_env(None);

        // At the top of a long listing the thumb is at the top.
        let screen = render_themed(&mut app, 80, 24, &theme);
        let status = status_row(24);
        assert!(row(&screen, 2).starts_with("║█"), "{}", row(&screen, 2));
        assert!(row(&screen, 16).starts_with("║│"), "{}", row(&screen, 16));
        assert!(row(&screen, status).contains("rows 1-15 of 75"), "{screen}");

        // At the bottom it is at the bottom, and the words agree.
        app.nav.cursor_to_end();
        let screen = render_themed(&mut app, 80, 24, &theme);
        assert!(row(&screen, 2).starts_with("║│"), "{}", row(&screen, 2));
        assert!(row(&screen, 16).starts_with("║█"), "{}", row(&screen, 16));
        assert!(
            row(&screen, status).contains("rows 61-75 of 75"),
            "{screen}"
        );
        assert!(row(&screen, status).contains("row 75 of 75"), "{screen}");

        // A listing that fits has no thumb at all: there is nowhere else
        // to be, and a full-height thumb would imply otherwise.
        let small = listing_fixture(3);
        let mut app = app_at(small.path());
        let screen = render_themed(&mut app, 80, 24, &theme);
        assert!(!screen.contains('█'));
        assert!(row(&screen, status).contains("all rows shown"), "{screen}");
    }

    #[test]
    fn a_long_filter_never_evicts_the_rails_textual_dual() {
        // A filter long enough to crowd the row, still matching enough
        // entries that the listing overflows and the rail draws a thumb.
        let tmp = tempfile::tempdir().unwrap();
        for i in 1..=40 {
            fs::write(
                tmp.path().join(format!("2026-q3-deliverable-{i:03}.txt")),
                "",
            )
            .unwrap();
        }
        let mut app = app_at(tmp.path());
        app.nav.set_filter("2026-q3-deliverable".to_string());
        app.nav.cursor_to_end();
        let theme = Theme::from_no_color_env(None);
        let screen = render_themed(&mut app, 80, 24, &theme);
        let status = row(&screen, status_row(24));
        assert!(screen.contains('█'), "{screen}");
        assert!(status.contains("rows 27-41 of 41"), "{status}");
        assert_eq!(display_width(&status), 80, "{status}");
    }

    #[test]
    fn descending_keeps_a_scroll_margin_below_the_cursor() {
        let tmp = listing_fixture(73);
        let mut app = app_at(tmp.path());
        let theme = Theme::from_no_color_env(None);
        for _ in 0..30 {
            app.handle_key(KeyInput::Char('j'));
        }
        let screen = render_themed(&mut app, 80, 24, &theme);
        let listing: Vec<String> = (2..status_row(24)).map(|i| row(&screen, i)).collect();
        let cursor = listing
            .iter()
            .position(|line| line.contains("> "))
            .expect("the cursor row is on screen");
        // Three rows of what is coming stay visible below the cursor.
        assert_eq!(listing.len() - 1 - cursor, SCROLL_MARGIN);
    }

    #[test]
    fn a_filter_that_matches_nothing_says_so() {
        let tmp = listing_fixture(6);
        let mut app = app_at(tmp.path());
        app.handle_key(KeyInput::Char('/'));
        for c in "zzz".chars() {
            app.handle_key(KeyInput::Char(c));
        }
        let theme = Theme::from_no_color_env(None);
        let screen = render_themed(&mut app, 80, 24, &theme);
        // The `..` row always passes the filter, so without this the
        // screen would show a bare `../` and claim `[1/1]`.
        assert!(screen.contains("../"));
        assert!(screen.contains("(no entries match 'zzz')"), "{screen}");
        assert!(screen.contains("filter 'zzz': 0 of 7 match"), "{screen}");
    }

    #[test]
    fn relative_times_replace_the_utc_stamp_in_the_listing() {
        let tmp = tempfile::tempdir().unwrap();
        fs::write(tmp.path().join("note.txt"), "x").unwrap();
        let mut app = app_at(tmp.path());
        let theme = Theme::from_no_color_env(None);
        let screen = render_themed(&mut app, 80, 24, &theme);
        assert!(!screen.contains("UTC"), "{screen}");
        // The fixture was written after the injected clock, so it reads
        // as brand new rather than as an error.
        assert!(screen.contains("note.txt"));
        assert!(row(&screen, 3).contains(" 1h"), "{}", row(&screen, 3));
        // The reclaimed columns went to the name, not away: today's
        // 46-column name field is the floor.
        let name_width = 78 - LISTING_FURNITURE;
        assert!(name_width >= 46, "name field shrank to {name_width}");
    }

    #[test]
    fn wide_characters_keep_the_columns_aligned() {
        let tmp = tempfile::tempdir().unwrap();
        fs::write(tmp.path().join("ascii_name.txt"), "").unwrap();
        fs::write(tmp.path().join("中文檔案名稱測試用範例.txt"), "").unwrap();
        let mut app = app_at(tmp.path());
        let theme = Theme::from_no_color_env(None);
        let screen = render_themed(&mut app, 80, 24, &theme);
        let column_of_size = |needle: &str| {
            let line = screen
                .lines()
                .find(|l| l.contains(needle))
                .unwrap_or_else(|| panic!("row for {needle} missing in\n{screen}"));
            let cut = line.find(needle).unwrap() + needle.len();
            let offset = line[cut..].find("0B").expect("size column");
            display_width(&line[..cut + offset])
        };
        assert_eq!(
            column_of_size("ascii_name.txt"),
            column_of_size("範例.txt"),
            "{screen}"
        );
    }

    #[test]
    fn no_color_keeps_every_new_signal_in_text() {
        let tmp = listing_fixture(73);
        let mut app = app_at(tmp.path());
        let theme = Theme::from_no_color_env(Some("1"));
        assert!(!theme.use_color);
        let top = render_themed(&mut app, 80, 24, &theme);
        // Kind is still a marker, not a color.
        assert!(top.contains("nested/"), "{top}");
        assert!(top.contains("../"), "{top}");
        assert!(top.contains("rows 1-15 of 75"), "{top}");

        app.nav.cursor_to_end();
        let screen = render_themed(&mut app, 80, 24, &theme);
        assert!(screen.contains("depth "), "{screen}");
        assert!(screen.contains("row 75 of 75"), "{screen}");
        assert!(screen.contains("rows 61-75 of 75"), "{screen}");
        assert!(screen.contains('█'), "the rail is still drawn");
    }

    #[test]
    fn ascii_mode_draws_nothing_outside_printable_ascii() {
        let tmp = listing_fixture(73);
        let mut app = app_at(tmp.path());
        app.nav.cursor_to_end();
        let theme = Theme::from_env(None, Some("1"));
        let screen = render_themed(&mut app, 80, 24, &theme);
        for c in screen.chars().filter(|c| *c != '\n') {
            assert!(
                (' '..='~').contains(&c),
                "non-ascii {c:?} on an ascii screen:\n{screen}"
            );
        }
        // The bearings are still all there, in words and in ASCII shapes.
        assert!(screen.contains("depth 1"), "{screen}");
        assert!(screen.contains("rows 61-75 of 75"), "{screen}");
        assert!(screen.contains('#'), "the rail still draws");
        assert!(screen.contains("0:~"), "{screen}");

        // The message-history pager is drawn from app-built lines, so it
        // must honor the same invariant as the frame around it.
        app.push_msg(Level::Info, "an info line in the log".to_string());
        app.handle_key(KeyInput::Char('M'));
        let pager = render_themed(&mut app, 80, 24, &theme);
        for c in pager.chars().filter(|c| *c != '\n') {
            assert!(
                (' '..='~').contains(&c),
                "non-ascii {c:?} on an ascii pager:\n{pager}"
            );
        }
        assert!(pager.contains("an info line in the log"), "{pager}");

        assert!(!Theme::from_env(None, None).ascii);
        assert!(!Theme::from_env(None, Some("")).ascii);
    }

    #[test]
    fn message_history_pager_renders_over_the_listing() {
        let (_tmp, mut app) = fixture_app();
        for i in 0..40 {
            app.push_msg(Level::Info, format!("event {i}"));
        }
        app.handle_key(KeyInput::Char('M'));
        let theme = Theme::from_no_color_env(None);
        let screen = render_themed(&mut app, 80, 24, &theme);
        assert!(screen.contains("messages ("), "{screen}");
        assert!(screen.contains("event 0"), "{screen}");
    }

    #[test]
    fn long_messages_are_truncated_not_broken_at_the_border() {
        let (_tmp, mut app) = fixture_app();
        app.push_msg(Level::Error, "e".repeat(200));
        let theme = Theme::from_no_color_env(None);
        let screen = render_themed(&mut app, 80, 24, &theme);
        let message = row(&screen, status_row(24) + 2);
        assert!(message.contains('…'), "{message}");
        assert_eq!(display_width(&message), 80);
    }

    #[test]
    fn screen_shows_banner_ladder_listing_and_hints() {
        let (_tmp, app) = fixture_app();
        let screen = render(&app);
        assert!(screen.contains("FILECRAFT"));
        // The raw path line is gone; the ladder names the current
        // directory and says how deep it is.
        let current = app.nav.cwd.file_name().unwrap().to_string_lossy();
        assert!(screen.contains(&format!("·{current}")), "{screen}");
        assert!(screen.contains("depth "));
        assert!(screen.contains("projects/"));
        assert!(screen.contains("readme.md"));
        assert!(screen.contains("<DIR>"));
        assert!(screen.contains("? help"));
        assert!(screen.contains("cmd>"));
    }

    #[test]
    fn command_mode_prompt_shows_typed_input() {
        let (_tmp, mut app) = fixture_app();
        app.handle_key(KeyInput::Char(':'));
        for c in "move dst".chars() {
            app.handle_key(KeyInput::Char(c));
        }
        let screen = render(&app);
        assert!(screen.contains("cmd> move dst"));
    }

    #[test]
    fn confirm_mode_shows_operation_target() {
        let (tmp, mut app) = fixture_app();
        let visible = app.nav.visible();
        let pos = visible
            .iter()
            .position(|&i| app.nav.entries[i].name == "readme.md")
            .unwrap();
        app.nav.cursor = pos;
        app.execute_line("move projects");
        let screen = render(&app);
        assert!(screen.contains("confirm"));
        assert!(screen.contains("[y]es / [n]o"));
        assert!(screen.contains("readme.md"));
        drop(tmp);
    }

    #[test]
    fn help_pager_renders_over_listing() {
        let (_tmp, mut app) = fixture_app();
        app.handle_key(KeyInput::Char('?'));
        let screen = render(&app);
        assert!(screen.contains("KEYS"));
        assert!(screen.contains("jump to that ancestor"));
        // The commands sit just below the first pager page; scrolling
        // brings them into view without skipping anything.
        for _ in 0..6 {
            app.handle_key(KeyInput::Char('j'));
        }
        let screen = render(&app);
        assert!(screen.contains("COMMANDS"));
        assert!(screen.contains("move <destination>"));
    }

    #[test]
    fn error_messages_carry_textual_prefix() {
        let (_tmp, mut app) = fixture_app();
        app.execute_line("cd /definitely/not/here");
        let screen = render(&app);
        assert!(screen.contains("err:"));
    }

    #[test]
    fn no_color_theme_drops_colors_keeps_emphasis() {
        let theme = Theme::from_no_color_env(Some("1"));
        assert!(!theme.use_color);
        let style = theme.error();
        assert_eq!(style.fg, None);
        assert!(style.add_modifier.contains(Modifier::BOLD));
        // Selection stays visible without color.
        assert!(theme.selected().add_modifier.contains(Modifier::REVERSED));

        assert!(Theme::from_no_color_env(None).use_color);
        assert!(Theme::from_no_color_env(Some("")).use_color);
    }

    #[test]
    fn sanitize_neutralizes_control_characters() {
        assert_eq!(sanitize("plain näme 檔"), "plain näme 檔");
        assert_eq!(sanitize("a\u{1b}[31mb"), "a\u{FFFD}[31mb");
        assert_eq!(
            sanitize("bell\u{7}tab\tnl\n"),
            "bell\u{FFFD}tab\u{FFFD}nl\u{FFFD}"
        );
    }

    #[test]
    fn control_characters_in_names_never_reach_the_screen() {
        let tmp = tempfile::tempdir().unwrap();
        fs::write(tmp.path().join("evil\u{1b}[31m.txt"), "x").unwrap();

        let nav = NavState::new(tmp.path()).unwrap();
        let app = App::new(nav, None, false, None);
        let screen = render(&app);
        assert!(!screen.contains('\u{1b}'));
        assert!(screen.contains("evil\u{FFFD}[31m.txt"));

        let listing = render_static_listing(tmp.path()).unwrap();
        assert!(!listing.contains('\u{1b}'));
        assert!(listing.contains("evil\u{FFFD}[31m.txt"));
    }

    #[test]
    fn control_characters_in_confirm_and_messages_never_reach_the_screen() {
        let tmp = tempfile::tempdir().unwrap();
        fs::create_dir(tmp.path().join("archive")).unwrap();
        fs::write(tmp.path().join("evil\u{1b}]0;pwned\u{7}.txt"), "x").unwrap();
        let nav = NavState::new(tmp.path()).unwrap();
        let mut app = App::new(nav, None, false, None);

        let visible = app.nav.visible();
        let pos = visible
            .iter()
            .position(|&i| app.nav.entries[i].name.starts_with("evil"))
            .unwrap();
        app.nav.cursor = pos;
        app.execute_line("move archive");

        let screen = render_size(&app, 160, 30);
        assert!(screen.contains("confirm"));
        assert!(!screen.contains('\u{1b}'));
        assert!(!screen.contains('\u{7}'));
        drop(tmp);
    }

    #[test]
    fn pad_to_width_handles_ascii_and_cjk() {
        assert_eq!(pad_to_width("abc", 6), "abc   ");
        assert_eq!(pad_to_width("abcdef", 6), "abcdef");
        assert_eq!(pad_to_width("abcdefg", 6), "abcde…");
        // '檔' is 2 columns wide.
        let padded = pad_to_width("檔案名稱很長的檔案", 8);
        let width: usize = padded.chars().map(|c| c.width().unwrap_or(0)).sum();
        assert_eq!(width, 8);
        assert!(padded.trim_end().ends_with('…'));
        assert_eq!(pad_to_width("", 4), "    ");
        assert_eq!(pad_to_width("x", 0), "");
    }

    #[test]
    fn static_listing_shows_path_entries_and_tty_note() {
        let tmp = tempfile::tempdir().unwrap();
        fs::create_dir(tmp.path().join("projects")).unwrap();
        fs::write(tmp.path().join("readme.md"), "hi").unwrap();
        fs::write(tmp.path().join("ünïcødé 檔.md"), "u").unwrap();

        let listing = render_static_listing(tmp.path()).unwrap();
        assert!(listing.contains("filecraft"));
        assert!(listing.contains("no TTY"));
        assert!(listing.contains("projects/"));
        assert!(listing.contains("readme.md"));
        assert!(listing.contains("ünïcødé 檔.md"));
        assert!(listing.contains("<DIR>"));
        assert!(listing.contains("q quit"));
    }

    #[test]
    fn static_listing_missing_directory_is_an_error() {
        let tmp = tempfile::tempdir().unwrap();
        let err = render_static_listing(&tmp.path().join("nope")).unwrap_err();
        assert!(matches!(err, FsError::NotFound(_)));
    }
}
