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
use ratatui::widgets::{Block, BorderType, Borders, Clear, Padding, Paragraph};
use ratatui::Frame;

use crate::app::{App, Level, Mode};
use crate::bearings::{
    self, display_width, pad_to_width, pad_to_width_with, sanitize, Bearings, Glyphs, RailCell,
};
use crate::fsops::FsError;
use crate::i18n::{self, Lang};
use crate::joblog::{LogPane, HEADER_ROWS};
use crate::markdown::{self, Ink, Kind, Row};
use crate::multiselect::FileSelector;
use crate::nav::{EntryKind, NavState};
use crate::pager::Pager;
use crate::picker::FolderPicker;
use crate::preview::{format_size, format_timestamp_in};
use crate::summarize;

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
///
/// The age field is the one part that is not the same width in every
/// language - a Han character owns two cells, so `59分鐘前` needs eight
/// columns where `59m` needs three - so the furniture is measured
/// against [`Lang::age_width`] rather than pinned to English.
fn listing_furniture(lang: Lang) -> usize {
    1 + 2 + 8 + 1 + lang.age_width()
}

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

    /// Reader heading style. Level is part of it, but so is the `#`
    /// marker the line keeps, so a colorless screen still ranks them.
    pub fn heading(&self, level: u8) -> Style {
        let bold = Style::default().add_modifier(Modifier::BOLD);
        match level {
            1 => self.color(bold.fg(Color::Cyan)),
            2 => self.color(bold.fg(Color::Yellow)),
            _ => self.color(bold),
        }
    }

    /// Code, fenced or inline.
    pub fn code(&self) -> Style {
        self.color(Style::default().fg(Color::Green))
    }

    /// Blockquote bar and body.
    pub fn quote(&self) -> Style {
        self.color(Style::default().fg(Color::Cyan))
    }

    /// Rules, fences, and reader notices: present but quiet.
    pub fn meta(&self) -> Style {
        self.color(Style::default().fg(Color::DarkGray))
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
pub fn render_static_listing(dir: &Path, lang: Lang) -> Result<String, FsError> {
    let nav = NavState::new(dir)?;
    let mut out = String::new();
    let _ = writeln!(
        out,
        "filecraft {}  {}",
        env!("CARGO_PKG_VERSION"),
        sanitize(&nav.cwd.display().to_string())
    );
    let _ = writeln!(out, "{}", lang.static_listing_note());
    let _ = writeln!(out);

    let visible = nav.visible();
    if visible.is_empty() {
        let _ = writeln!(out, "  {}", lang.empty_directory());
    } else {
        for &i in &visible {
            let entry = &nav.entries[i];
            let size = if entry.is_parent || entry.is_enterable() {
                lang.dir_marker().to_string()
            } else {
                format_size(entry.size)
            };
            let date = entry
                .modified
                .map(|m| format_timestamp_in(m, lang))
                .unwrap_or_default();
            let name = pad_to_width(&sanitize(&entry.display_name()), 40);
            let _ = writeln!(out, "  {name} {size:>8}  {date}");
        }
    }
    let _ = writeln!(out);
    let _ = writeln!(out, "{}", lang.static_listing_keys());
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
    let lang = app.lang;
    let bearings = Bearings::from_nav(&app.nav, offset, rows, lang);
    match &app.mode {
        Mode::Pager(pager) => draw_pager(frame, theme, list_area, pager, lang),
        Mode::JobLog(pane) => draw_job_log(frame, theme, list_area, pane, lang),
        Mode::FolderPicker(picker) => draw_picker(frame, theme, list_area, picker, lang),
        Mode::FileSelector(selector) => draw_selector(frame, theme, list_area, selector, lang),
        Mode::ProviderMenu { files } => {
            draw_provider_menu(frame, theme, list_area, files.len(), lang)
        }
        _ => draw_listing(
            frame, app, theme, list_area, &visible, offset, &bearings, now,
        ),
    }
    draw_status(
        frame,
        theme,
        status_row,
        &bearings,
        now,
        app.job_status(),
        lang,
    );
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
    let lang = app.lang;
    let width = area.width as usize;
    let name_width = width.saturating_sub(listing_furniture(lang));
    // Every listing row is the rail plus this much content.
    let body_width = width.saturating_sub(1);
    let rail = bearings::rail(visible.len(), offset, rows);
    // A filter that matched nothing must say so: the `..` row always
    // passes, so a bare `../` would otherwise look like a real result.
    let note = if visible.is_empty() {
        Some(lang.no_matching_entries().to_string())
    } else if bearings::filter_matched_nothing(bearings) {
        Some(lang.no_entries_match(&bearings.filter))
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
            lang.dir_marker().to_string()
        } else {
            format_size(entry.size)
        };
        // Relative time needs no timezone, and costs a handful of
        // columns instead of twenty - which is what pays for the rail.
        // Padded by display width, not by character count, so a CJK age
        // does not push the row past the border.
        let age = pad_to_width_with(
            &entry
                .modified
                .map(|m| bearings::relative_time(now, m, lang))
                .unwrap_or_default(),
            lang.age_width(),
            glyphs.ellipsis,
        );
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
            Span::styled(format!(" {age}"), base_style),
        ]));
    }
    frame.render_widget(Paragraph::new(lines), area);
}

/// The full-screen reader. The frame names what is being read at the
/// top and says where in it the view sits at the bottom, in words - the
/// same rule the status row follows, so the position is never carried by
/// the scroll thumb alone.
fn draw_pager(frame: &mut Frame<'_>, theme: &Theme, area: Rect, pager: &Pager, lang: Lang) {
    let glyphs = theme.glyphs();
    let frame_only = Block::default()
        .borders(Borders::ALL)
        .border_set(theme.pager_border_set())
        .padding(Padding::horizontal(1));
    let inner = frame_only.inner(area);
    let width = inner.width as usize;
    let view = inner.height as usize;

    let block = frame_only
        .border_style(theme.banner())
        .title(Span::styled(
            format!(" {} ", sanitize(&pager.title)),
            theme.prompt(),
        ))
        .title_bottom(
            Line::from(Span::styled(
                format!(" {} ", pager.position(width, view, &glyphs, lang)),
                theme.bearing(),
            ))
            .right_aligned(),
        );
    frame.render_widget(Clear, area);
    frame.render_widget(block, area);

    let rows = pager.rows(width, &glyphs);
    let scroll = pager.scroll.min(Pager::max_scroll(rows.len(), view));
    let lines: Vec<Line> = rows
        .iter()
        .skip(scroll)
        .take(view)
        .map(|row| reader_line(row, theme, &pager.query))
        .collect();
    frame.render_widget(Paragraph::new(lines), inner);
}

/// The live log viewer. The reader's frame with two pinned header rows
/// above the log: what the run is doing, and which session it opened.
///
/// The rows it reserves are [`crate::joblog::FRAME_ROWS`], which is what
/// [`crate::app::App::log_rows`] subtracts. If the two ever disagree the
/// log scrolls past rows the screen never drew.
fn draw_job_log(frame: &mut Frame<'_>, theme: &Theme, area: Rect, pane: &LogPane, lang: Lang) {
    let glyphs = theme.glyphs();
    let frame_only = Block::default()
        .borders(Borders::ALL)
        .border_set(theme.pager_border_set())
        .padding(Padding::horizontal(1));
    let inner = frame_only.inner(area);
    let width = inner.width as usize;
    let view = (inner.height as usize).saturating_sub(HEADER_ROWS);

    let block = frame_only
        .border_style(theme.banner())
        .title(Span::styled(
            format!(" {} ", sanitize(&pane.pager.title)),
            theme.prompt(),
        ))
        .title_bottom(
            Line::from(Span::styled(
                format!(" {} ", pane.pager.position(width, view, &glyphs, lang)),
                theme.bearing(),
            ))
            .right_aligned(),
        );
    frame.render_widget(Clear, area);
    frame.render_widget(block, area);

    let [header_area, log_area] =
        Layout::vertical([Constraint::Length(HEADER_ROWS as u16), Constraint::Min(0)]).areas(inner);

    // The header is chrome: it is never scrolled and never operated on.
    let header: Vec<Line> = pane
        .header(&glyphs, lang)
        .iter()
        .map(|row| {
            Line::from(Span::styled(
                pad_to_width_with(&sanitize(row), width, glyphs.ellipsis),
                theme.meta(),
            ))
        })
        .collect();
    frame.render_widget(Paragraph::new(header), header_area);

    let rows = pane.pager.rows(width, &glyphs);
    let scroll = pane.pager.scroll.min(Pager::max_scroll(rows.len(), view));
    let lines: Vec<Line> = rows
        .iter()
        .skip(scroll)
        .take(view)
        .map(|row| reader_line(row, theme, &pane.pager.query))
        .collect();
    frame.render_widget(Paragraph::new(lines), log_area);
}

/// The folder picker popup. Same listing-area frame as the reader, so
/// the listing underneath is unchanged and cancelling lands on the same
/// row. The dest header is the dual of the focused folder: color never
/// carries the target by itself.
fn draw_picker(
    frame: &mut Frame<'_>,
    theme: &Theme,
    area: Rect,
    picker: &FolderPicker,
    lang: Lang,
) {
    let glyphs = theme.glyphs();
    let frame_only = Block::default()
        .borders(Borders::ALL)
        .border_set(theme.pager_border_set())
        .padding(Padding::horizontal(1));
    let inner = frame_only.inner(area);
    let block = frame_only
        .border_style(theme.banner())
        .title(Span::styled(lang.picker_title(), theme.prompt()))
        .title_bottom(
            Line::from(Span::styled(
                i18n::keys_row(lang.picker_keys(), glyphs.dot),
                theme.bearing(),
            ))
            .right_aligned(),
        );
    frame.render_widget(Clear, area);
    frame.render_widget(block, area);

    if inner.height == 0 || inner.width == 0 {
        return;
    }
    let [dest_row, list_area] =
        Layout::vertical([Constraint::Length(1), Constraint::Min(1)]).areas(inner);
    let dest = pad_to_width_with(
        &sanitize(&picker.dest_line(lang)),
        dest_row.width as usize,
        glyphs.ellipsis,
    );
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(dest, theme.prompt()))),
        dest_row,
    );

    let rows = list_area.height as usize;
    if rows == 0 {
        return;
    }
    let name_width = (list_area.width as usize).saturating_sub(2);
    let offset = bearings::viewport_offset(picker.cursor, picker.entries.len(), rows, 1);
    let mut lines: Vec<Line> = Vec::with_capacity(rows);
    for row in 0..rows {
        let index = offset + row;
        let Some(entry) = picker.entries.get(index) else {
            lines.push(Line::from(""));
            continue;
        };
        let selected = index == picker.cursor;
        let marker = if selected { "> " } else { "  " };
        let name = pad_to_width_with(
            &sanitize(&entry.display_name()),
            name_width,
            glyphs.ellipsis,
        );
        let style = if selected {
            theme.selected()
        } else {
            theme.dir()
        };
        lines.push(Line::from(Span::styled(format!("{marker}{name}"), style)));
    }
    frame.render_widget(Paragraph::new(lines), list_area);
}

/// The multi-file selector popup. Same listing-area frame as the folder
/// picker, so the listing underneath is unchanged and cancelling lands on
/// the same row. Selection is drawn as a `[x]` box, never as color alone,
/// and the header counts what is selected in words.
fn draw_selector(
    frame: &mut Frame<'_>,
    theme: &Theme,
    area: Rect,
    selector: &FileSelector,
    lang: Lang,
) {
    let glyphs = theme.glyphs();
    let frame_only = Block::default()
        .borders(Borders::ALL)
        .border_set(theme.pager_border_set())
        .padding(Padding::horizontal(1));
    let inner = frame_only.inner(area);
    let block = frame_only
        .border_style(theme.banner())
        .title(Span::styled(lang.selector_title(), theme.prompt()))
        .title_bottom(
            Line::from(Span::styled(
                i18n::keys_row(lang.selector_keys(), glyphs.dot),
                theme.bearing(),
            ))
            .right_aligned(),
        );
    frame.render_widget(Clear, area);
    frame.render_widget(block, area);

    if inner.height == 0 || inner.width == 0 {
        return;
    }
    let [header_row, list_area] =
        Layout::vertical([Constraint::Length(1), Constraint::Min(1)]).areas(inner);
    let header = pad_to_width_with(
        &sanitize(&selector.header_line(lang)),
        header_row.width as usize,
        glyphs.ellipsis,
    );
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(header, theme.prompt()))),
        header_row,
    );

    let rows = list_area.height as usize;
    if rows == 0 {
        return;
    }
    // Two columns of cursor marker plus four of `[x] `.
    let name_width = (list_area.width as usize).saturating_sub(6);
    let offset = bearings::viewport_offset(selector.cursor, selector.entries.len(), rows, 1);
    let mut lines: Vec<Line> = Vec::with_capacity(rows);
    for row in 0..rows {
        let index = offset + row;
        let Some(entry) = selector.entries.get(index) else {
            lines.push(Line::from(""));
            continue;
        };
        let selected = index == selector.cursor;
        let marker = if selected { "> " } else { "  " };
        let name = pad_to_width_with(
            &sanitize(&entry.display_name()),
            name_width,
            glyphs.ellipsis,
        );
        let style = match () {
            _ if selected => theme.selected(),
            _ if entry.is_enterable() => theme.dir(),
            _ if selector.is_chosen(&entry.path) => theme.ok(),
            _ => Style::default(),
        };
        lines.push(Line::from(Span::styled(
            format!("{marker}{} {name}", selector.mark(entry)),
            style,
        )));
    }
    frame.render_widget(Paragraph::new(lines), list_area);
}

/// The provider dialog. Every row names its digit and its exact command
/// line, and the default is marked in words, so nothing about the choice
/// is carried by position or color.
fn draw_provider_menu(frame: &mut Frame<'_>, theme: &Theme, area: Rect, files: usize, lang: Lang) {
    let glyphs = theme.glyphs();
    let frame_only = Block::default()
        .borders(Borders::ALL)
        .border_set(theme.pager_border_set())
        .padding(Padding::horizontal(1));
    let inner = frame_only.inner(area);
    let block = frame_only
        .border_style(theme.banner())
        .title(Span::styled(lang.provider_title(), theme.prompt()))
        .title_bottom(
            Line::from(Span::styled(
                i18n::keys_row(lang.provider_keys(), glyphs.dot),
                theme.bearing(),
            ))
            .right_aligned(),
        );
    frame.render_widget(Clear, area);
    frame.render_widget(block, area);

    if inner.height == 0 || inner.width == 0 {
        return;
    }
    // No blank under the count: at 60x20 this dialog has exactly nine
    // rows, and the widest command line needs two of them.
    let mut lines = vec![Line::from(Span::styled(
        sanitize(&lang.files_selected(files)),
        theme.prompt(),
    ))];
    for line in summarize::menu_lines(lang) {
        for row in bearings::wrap_hanging(
            &sanitize(&line),
            inner.width as usize,
            summarize::MENU_INDENT,
        ) {
            lines.push(Line::from(Span::raw(row)));
        }
    }
    lines.push(Line::from(""));
    for row in bearings::wrap_hanging(lang.provider_scope_note(), inner.width as usize, 0) {
        lines.push(Line::from(Span::styled(row, theme.meta())));
    }
    // No `Paragraph::wrap` on top: `wrap_hanging` is the only thing that
    // decides where a row of this dialog breaks, so a continuation always
    // sits under its own command line rather than at the left edge where
    // it would read as one more provider.
    frame.render_widget(Paragraph::new(lines), inner);
}

/// One drawn row of the reader, with the active search query picked out.
fn reader_line(row: &Row, theme: &Theme, query: &str) -> Line<'static> {
    let spans = markdown::highlight(&row.spans, query);
    Line::from(
        spans
            .into_iter()
            .map(|span| Span::styled(sanitize(&span.text), ink_style(row.kind, span.ink, theme)))
            .collect::<Vec<_>>(),
    )
}

/// How a run of reader text is drawn: the line's kind sets the base, the
/// inline ink modifies it. Everything that carries meaning also carries a
/// character (`#`, a bullet, a quote bar, backticks), so `NO_COLOR` loses
/// nothing but color.
fn ink_style(kind: Kind, ink: Ink, theme: &Theme) -> Style {
    let base = match kind {
        Kind::Heading(level) => theme.heading(level),
        Kind::Code => theme.code(),
        Kind::Quote => theme.quote(),
        Kind::Fence | Kind::Rule | Kind::Meta => theme.meta(),
        Kind::Body | Kind::Bullet => Style::default(),
    };
    match ink {
        Ink::Match => Style::default().add_modifier(Modifier::REVERSED),
        Ink::Marker if matches!(kind, Kind::Bullet) => theme.prompt(),
        // The one body line that has a marker is a log line, whose
        // gutter is chrome: it recedes so the provider's own words are
        // what the eye lands on. Textual either way - the number and the
        // stream character are there whatever the palette.
        Ink::Marker if matches!(kind, Kind::Body) => theme.meta(),
        Ink::Marker => base,
        Ink::Code => theme.code(),
        Ink::Strong => base.add_modifier(Modifier::BOLD),
        Ink::Emph => base.add_modifier(Modifier::UNDERLINED),
        Ink::Meta => theme.meta(),
        Ink::Plain => base,
    }
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
    job: Option<String>,
    lang: Lang,
) {
    let glyphs = theme.glyphs();
    let mut speakable = bearings::speakable(bearings, now, lang);
    let separator = format!(" {} ", glyphs.dot);
    // A running summary claims the head of the row and keeps it: it is
    // the one thing on screen that is happening rather than being shown,
    // so it is never what a narrow terminal drops.
    let job = job.map(|status| sanitize(&status));
    let claimed = job
        .as_deref()
        .map(|status| display_width(status) + 1)
        .unwrap_or(0);
    let width = (area.width as usize).saturating_sub(1 + claimed);
    bearings::bound_speakable_filter(&mut speakable, &separator, width, glyphs.ellipsis);
    let text = bearings::fit_joined_pinned(
        &speakable.parts,
        &separator,
        width,
        glyphs.ellipsis,
        speakable.pinned,
    );
    let mut spans = vec![Span::raw(" ")];
    if let Some(status) = job {
        spans.push(Span::styled(format!("{status} "), theme.prompt()));
    }
    spans.push(Span::raw(text));
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
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
    let lang = app.lang;
    let caret = glyphs.caret;
    let line = match &app.mode {
        Mode::Command { input } => Line::from(vec![
            Span::styled(lang.prompt_command(), theme.prompt()),
            Span::raw(sanitize(input)),
            Span::styled(caret, theme.prompt()),
        ]),
        Mode::Filter { input } => Line::from(vec![
            Span::styled(lang.prompt_filter(), theme.prompt()),
            Span::raw(sanitize(input)),
            Span::styled(caret, theme.prompt()),
        ]),
        Mode::ConfirmOp => {
            let description = app
                .pending
                .as_ref()
                .map(|op| op.describe(lang))
                .unwrap_or_default();
            Line::from(vec![
                Span::styled(lang.prompt_confirm(), theme.confirm()),
                Span::styled(lang.prompt_yes_no(), theme.prompt()),
                Span::raw(sanitize(&description)),
            ])
        }
        Mode::Pager(pager) => match &pager.find {
            Some(input) => Line::from(vec![
                Span::styled(lang.prompt_find(), theme.prompt()),
                Span::raw(sanitize(input)),
                Span::styled(caret, theme.prompt()),
            ]),
            None => Line::from(vec![
                Span::styled(lang.prompt_read(), theme.prompt()),
                Span::raw(i18n::keys_row(lang.reader_keys(), glyphs.dot)),
            ]),
        },
        Mode::JobLog(pane) => match &pane.pager.find {
            Some(input) => Line::from(vec![
                Span::styled(lang.prompt_find(), theme.prompt()),
                Span::raw(sanitize(input)),
                Span::styled(caret, theme.prompt()),
            ]),
            None => Line::from(vec![
                Span::styled(lang.prompt_watch(), theme.prompt()),
                Span::raw(sanitize(&format!(
                    "{} {dot} {}",
                    pane.activity().label(lang),
                    match pane.session() {
                        Some(id) => lang.watch_session(id),
                        None => lang.no_session_reported().to_string(),
                    },
                    dot = glyphs.dot
                ))),
            ]),
        },
        Mode::FolderPicker(picker) => Line::from(vec![
            Span::styled(lang.prompt_pick(), theme.prompt()),
            Span::raw(sanitize(&lang.moving_to(
                &picker.source_name,
                &picker.destination().display().to_string(),
            ))),
        ]),
        Mode::ConfirmQuit => Line::from(vec![
            Span::styled(lang.prompt_confirm(), theme.confirm()),
            Span::styled(lang.prompt_yes_no(), theme.prompt()),
            Span::raw(lang.quit_question()),
        ]),
        Mode::FileSelector(selector) => Line::from(vec![
            Span::styled(lang.prompt_pick(), theme.prompt()),
            Span::raw(lang.selector_prompt(selector.count())),
        ]),
        Mode::ProviderMenu { files } => Line::from(vec![
            Span::styled(lang.prompt_pick(), theme.prompt()),
            Span::raw(lang.provider_prompt(files.len(), summarize::Provider::DEFAULT.code())),
        ]),
        Mode::Browse => Line::from(vec![
            Span::styled(lang.prompt_command(), theme.prompt()),
            Span::raw(lang.prompt_browse_hint()),
        ]),
    };
    frame.render_widget(Paragraph::new(line), area);
}

/// Mode-appropriate keys, fitted by dropping whole hints. The row never
/// ends inside a word, including at the documented 80x24 minimum.
fn draw_hints(frame: &mut Frame<'_>, app: &App, theme: &Theme, area: Rect) {
    let lang = app.lang;
    let hints: &[&str] = match &app.mode {
        Mode::Browse => lang.hints_browse(),
        Mode::Command { .. } => lang.hints_command(),
        Mode::Filter { .. } => lang.hints_filter(),
        Mode::ConfirmOp => lang.hints_confirm_op(),
        Mode::ConfirmQuit => lang.hints_confirm_quit(),
        Mode::FileSelector(_) => lang.hints_file_selector(),
        Mode::ProviderMenu { .. } => lang.hints_provider_menu(),
        Mode::FolderPicker(_) => lang.hints_folder_picker(),
        Mode::Pager(pager) if pager.find.is_some() => lang.hints_pager_find(),
        Mode::Pager(_) => lang.hints_pager(),
        Mode::JobLog(pane) if pane.pager.find.is_some() => lang.hints_joblog_find(),
        Mode::JobLog(pane) if pane.follow => lang.hints_joblog_following(),
        Mode::JobLog(_) => lang.hints_joblog(),
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
    use crate::app::{App, Effect, KeyInput};
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
        app.glyphs = theme.glyphs();
        app.set_viewport(
            height.saturating_sub(CHROME_ROWS) as usize,
            width.saturating_sub(BORDER_COLS) as usize,
        );
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
        (tmp, App::new(nav, None, false, None, Lang::En))
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
        app_at_in(dir, Lang::En)
    }

    /// Put the cursor on a named row.
    fn select(app: &mut App, name: &str) {
        let visible = app.nav.visible();
        let pos = visible
            .iter()
            .position(|&i| app.nav.entries[i].name == name)
            .unwrap_or_else(|| panic!("entry '{name}' not visible"));
        app.nav.cursor = pos;
    }

    /// The same app, speaking `lang` from the moment it is built - so
    /// even the welcome line in the message ring is in it.
    fn app_at_in(dir: &Path, lang: Lang) -> App {
        let home = dir.canonicalize().unwrap().parent().unwrap().to_path_buf();
        App::new(NavState::new(dir).unwrap(), None, false, Some(home), lang)
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

    /// A document tree the summarizer's screens can be drawn against.
    fn summary_fixture() -> tempfile::TempDir {
        let tmp = tempfile::tempdir().unwrap();
        fs::create_dir(tmp.path().join("archive")).unwrap();
        fs::write(tmp.path().join("report.pdf"), "%PDF-1.4").unwrap();
        fs::write(tmp.path().join("notes.md"), "# notes").unwrap();
        fs::write(tmp.path().join("log.txt"), "log").unwrap();
        fs::write(tmp.path().join("photo.png"), "png").unwrap();
        tmp
    }

    /// A job that never finishes, so a screen can be drawn mid-run.
    struct StalledJob;

    impl crate::summarize::Job for StalledJob {
        fn poll(&mut self) -> Option<crate::summarize::Outcome> {
            None
        }
        fn terminate(&mut self) {}
    }

    /// Put `app` into a state where a summary is running over `files`.
    fn with_running_job(app: &mut App, files: &[&str]) {
        let files: Vec<std::path::PathBuf> = files.iter().map(std::path::PathBuf::from).collect();
        let output = crate::summarize::output_path_with(&files[0], "20260829-101500", &|_| false);
        let spec =
            crate::summarize::JobSpec::new(crate::summarize::Provider::DEFAULT, files, output)
                .unwrap();
        app.job = Some(crate::app::ActiveJob::new(spec, Box::new(StalledJob)));
    }

    /// A job the app is watching: it is running, and its log already has
    /// `lines` in it. `end` is what a finished run looks like, which is
    /// also what makes the header's word deterministic.
    fn with_watched_job(app: &mut App, files: &[&str], lines: &[&str], end: bool) {
        with_running_job(app, files);
        let live = crate::stream::Handle::new();
        for line in lines {
            live.append(crate::stream::Origin::Out, &format!("{line}\n"));
        }
        if end {
            live.end();
        }
        app.run_log = Some(crate::app::RunLog {
            provider: crate::summarize::Provider::DEFAULT,
            output: std::path::PathBuf::from("/docs/a-summary.md"),
            stream: live,
        });
    }

    /// Open the file selector and select every named file.
    fn open_selector(app: &mut App, pick: &[&str]) {
        app.handle_key(KeyInput::Char('S'));
        for name in pick {
            let Mode::FileSelector(selector) = &mut app.mode else {
                panic!("expected the file selector");
            };
            selector.cursor = selector
                .entries
                .iter()
                .position(|e| e.name == *name)
                .unwrap_or_else(|| panic!("row '{name}' not in the selector"));
            app.handle_key(KeyInput::Char(' '));
        }
    }

    #[test]
    fn the_file_selector_draws_boxes_folders_and_a_count() {
        let tmp = summary_fixture();
        let mut app = app_at(tmp.path());
        open_selector(&mut app, &["notes.md"]);
        let screen = render(&app);
        assert!(screen.contains("summarize: pick files"), "{screen}");
        assert!(screen.contains("selected: 1 file"), "{screen}");
        assert!(screen.contains("[x] notes.md"), "{screen}");
        assert!(screen.contains("[ ] report.pdf"), "{screen}");
        assert!(screen.contains("archive/"), "{screen}");
        // A file the summarizer cannot read is not offered at all.
        assert!(!screen.contains("photo.png"), "{screen}");
        assert!(screen.contains("Space pick"), "{screen}");
    }

    /// The provider dialog's inner width: the outer frame's two borders,
    /// the dialog's own two, and one column of padding either side.
    fn dialog_width(frame_width: u16) -> usize {
        frame_width as usize - 6
    }

    /// One dialog line as the rows it is actually drawn on.
    fn dialog_rows(line: &str, frame_width: u16) -> Vec<String> {
        bearings::wrap_hanging(line, dialog_width(frame_width), summarize::MENU_INDENT)
    }

    #[test]
    fn the_provider_dialog_draws_every_command_line_and_marks_the_default() {
        let tmp = summary_fixture();
        let mut app = app_at(tmp.path());
        open_selector(&mut app, &["notes.md", "report.pdf"]);
        app.handle_key(KeyInput::Enter);
        // Every size, not just the default one: these rows are the widest
        // thing the dialog draws, and a row clipped at 60 columns would
        // name a command line that is not the one that runs.
        for (width, height) in SIZES {
            let screen = render_size(&app, width, height);
            assert!(screen.contains("summarize: pick a provider"), "{screen}");
            assert!(screen.contains("2 files selected"), "{screen}");
            for line in [
                "[1] ag: agy --dangerously-skip-permissions  [Default]",
                "[2] cc: claude --dangerously-skip-permissions",
                "[3] co: codex exec -s workspace-write --skip-git-repo-check",
                "[4] gk: grok --always-approve",
                "[5] ki: kimi",
            ] {
                // A row too wide for the dialog is continued under
                // itself, not cut short: asserting every piece is what
                // proves the whole command line reached the screen.
                for piece in dialog_rows(line, width) {
                    assert!(
                        screen.contains(&piece),
                        "{width}x{height} lost '{piece}' of '{line}':\n{screen}"
                    );
                }
            }
            // The safety statement is the point of the dialog, so it
            // has to survive the narrowest terminal whole.
            assert!(
                screen.contains("the provider runs locally and reads only these files"),
                "{width}x{height} clipped the safety line:\n{screen}"
            );
            assert!(screen.contains("Enter default"), "{screen}");
        }
    }

    /// Well below the documented 60x20 minimum, where every row wraps and
    /// `--dangerously-skip-permissions` is wider than the whole dialog.
    /// Nothing may be lost there either: a clipped row names a command
    /// line that is not the one that runs. No row may *begin* with a
    /// flag, so a continuation can never be read as one more provider -
    /// which holds only while `wrap_hanging` is the single thing
    /// deciding where a row breaks.
    #[test]
    fn a_narrow_dialog_still_draws_every_command_line_whole() {
        let tmp = summary_fixture();
        let mut app = app_at(tmp.path());
        open_selector(&mut app, &["notes.md", "report.pdf"]);
        app.handle_key(KeyInput::Enter);
        let screen = render_size(&app, 40, 30);

        for line in summarize::menu_lines(Lang::En) {
            let drawn = dialog_rows(&line, 40);
            for piece in &drawn {
                assert!(
                    screen.contains(piece),
                    "40x30 clipped '{piece}' of '{line}':\n{screen}"
                );
            }
            // The pieces put the row back: the widest flag here does not
            // fit one row at this width, and it still reaches the screen
            // in full rather than being cut in half.
            assert_eq!(
                drawn.concat().replace(' ', ""),
                line.replace(' ', ""),
                "40x30 lost part of '{line}'"
            );
        }
        assert!(
            screen.contains("--dangerously-skip-permiss"),
            "the widest flag never reached the screen:\n{screen}"
        );
        assert!(
            !screen.contains("\u{2502} -"),
            "a row started with a flag instead of continuing under one:\n{screen}"
        );
    }

    #[test]
    fn a_running_summary_holds_the_head_of_the_status_row() {
        let tmp = summary_fixture();
        let mut app = app_at(tmp.path());
        with_running_job(&mut app, &["/docs/a.pdf", "/docs/b.md", "/docs/c.txt"]);
        for (width, height) in SIZES {
            let screen = render_size(&app, width, height);
            let status = row(&screen, status_row(height));
            assert!(
                status.contains("[AI: summarizing 3 files with agy]"),
                "{width}x{height} lost the run from the status row: {status:?}"
            );
        }
    }

    #[test]
    fn the_quit_prompt_names_the_question_and_both_answers() {
        let tmp = summary_fixture();
        let mut app = app_at(tmp.path());
        with_running_job(&mut app, &["/docs/a.pdf"]);
        assert_eq!(app.handle_key(KeyInput::Char('q')), Effect::None);
        let screen = render(&app);
        assert!(
            screen
                .contains("confirm [y]es / [n]o  task in progress: terminate AI summary and quit?"),
            "{screen}"
        );
        assert!(screen.contains("y terminate and quit"), "{screen}");
        assert!(screen.contains("n/Esc keep running"), "{screen}");
    }

    #[test]
    fn every_summarizer_screen_keeps_its_frame_at_every_size() {
        let tmp = summary_fixture();
        for stage in 0..5 {
            for (width, height) in SIZES {
                for (lang, theme) in [
                    (Lang::En, Theme::from_no_color_env(None)),
                    (Lang::En, Theme::from_no_color_env(Some("1"))),
                    (Lang::En, Theme::from_env(None, Some("1"))),
                    (Lang::ZhTw, Theme::from_no_color_env(None)),
                    (Lang::ZhTw, Theme::from_env(None, Some("1"))),
                ] {
                    let mut app = app_at_in(tmp.path(), lang);
                    match stage {
                        0 => open_selector(&mut app, &["notes.md"]),
                        1 => {
                            open_selector(&mut app, &["notes.md"]);
                            app.handle_key(KeyInput::Enter);
                        }
                        // A summary running under the quit prompt, and
                        // under the reader: the status row carries the
                        // run in both, over a popup that covers the list.
                        2 => {
                            with_running_job(&mut app, &["/docs/a.pdf", "/docs/b.md"]);
                            app.handle_key(KeyInput::Char('q'));
                        }
                        3 => {
                            with_running_job(&mut app, &["/docs/a.pdf", "/docs/b.md"]);
                            app.handle_key(KeyInput::Char('?'));
                        }
                        // The log viewer, over a run that has printed
                        // more than fits and one line far too wide.
                        _ => {
                            let mut lines: Vec<String> =
                                (1..=60).map(|i| format!("provider line {i}")).collect();
                            lines.push("w".repeat(300));
                            lines.push("session id: 01a04eef-d4a6-7232".to_string());
                            let lines: Vec<&str> = lines.iter().map(String::as_str).collect();
                            with_watched_job(
                                &mut app,
                                &["/docs/a.pdf", "/docs/b.md"],
                                &lines,
                                false,
                            );
                            app.handle_key(KeyInput::Char('L'));
                        }
                    }
                    let screen = render_themed(&mut app, width, height, &theme);
                    let lines: Vec<&str> = screen.lines().collect();
                    assert_eq!(lines.len(), height as usize);
                    for (index, line) in lines.iter().enumerate() {
                        assert_eq!(
                            display_width(line),
                            width as usize,
                            "{} stage {stage} at {width}x{height} row {index}: {line:?}",
                            lang.code()
                        );
                    }
                    // `FILECRAFT_ASCII` is about the characters filecraft
                    // *draws*; a language written in Han characters is
                    // still written in them, so only the English screen
                    // can be asserted to be all-ASCII.
                    if theme.ascii && lang == Lang::En {
                        for c in screen.chars().filter(|c| *c != '\n') {
                            assert!(
                                (' '..='~').contains(&c),
                                "stage {stage} drew non-ascii {c:?} on an ascii screen:\n{screen}"
                            );
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn the_selector_and_provider_screens_survive_without_color_or_unicode() {
        let tmp = summary_fixture();
        let theme = Theme::from_env(Some("1"), Some("1"));
        let mut app = app_at(tmp.path());
        open_selector(&mut app, &["notes.md"]);
        let screen = render_themed(&mut app, 80, 24, &theme);
        // Selection is a box, not a color, so it survives both.
        assert!(screen.contains("[x] notes.md"), "{screen}");
        assert!(screen.contains("[ ] report.pdf"), "{screen}");

        app.handle_key(KeyInput::Enter);
        let screen = render_themed(&mut app, 80, 24, &theme);
        assert!(screen.contains("[Default]"), "{screen}");
        assert!(screen.contains("[1] ag:"), "{screen}");
    }

    #[test]
    fn the_log_viewer_draws_the_header_the_stream_marks_and_where_it_is() {
        let tmp = summary_fixture();
        let mut app = app_at(tmp.path());
        with_watched_job(
            &mut app,
            &["/docs/a.pdf"],
            &["session id: 01a04eef-d4a6-7232", "reading the files"],
            true,
        );
        app.handle_key(KeyInput::Char('L'));
        let theme = Theme::from_no_color_env(None);
        let screen = render_themed(&mut app, 90, 30, &theme);

        // The pane names itself, and the two header rows say what the
        // run is and how to get back into it outside filecraft.
        assert!(screen.contains("job log: agy"), "{screen}");
        assert!(screen.contains("agy · finished · 2 lines"), "{screen}");
        assert!(
            screen.contains(
                "session 01a04eef-d4a6-7232 · resume: agy --conversation 01a04eef-d4a6-7232"
            ),
            "{screen}"
        );
        // Numbered lines, and the position in words like every other pane.
        assert!(screen.contains("    2 | reading the files"), "{screen}");
        // stderr's mark, on a line the pane wrapped, still hangs from its
        // own gutter rather than restarting at the frame's edge.
        assert!(screen.contains("    1 | session id: 01a04eef"), "{screen}");
        assert!(screen.contains("line 1 of 2"), "{screen}");
        // The prompt row says what is being watched, not what to type.
        assert!(screen.contains("finished · session 01a04eef"), "{screen}");
    }

    #[test]
    fn the_log_viewer_keeps_every_signal_in_text_without_color_or_unicode() {
        let tmp = summary_fixture();
        let mut app = app_at(tmp.path());
        with_watched_job(
            &mut app,
            &["/docs/a.pdf"],
            &["out line", "session_id: abc-123456"],
            true,
        );
        app.handle_key(KeyInput::Char('L'));
        let theme = Theme::from_env(Some("1"), Some("1"));
        let screen = render_themed(&mut app, 80, 24, &theme);
        // stdout and stderr are told apart by a character, the run's
        // state is a word, and the resume command is spelled out.
        assert!(screen.contains("    1 | out line"), "{screen}");
        assert!(screen.contains("agy - finished - 2 lines"), "{screen}");
        assert!(
            screen.contains("resume: agy --conversation abc-123456"),
            "{screen}"
        );
        for c in screen.chars().filter(|c| *c != '\n') {
            assert!((' '..='~').contains(&c), "non-ascii {c:?}:\n{screen}");
        }
    }

    /// The coupling `joblog::FRAME_ROWS` exists for: the pane scrolls by
    /// exactly the rows the frame draws, and wraps at exactly the columns
    /// it draws in. If they disagree the log scrolls past rows nobody saw.
    #[test]
    fn the_log_viewer_is_given_exactly_the_geometry_it_scrolls_by() {
        let tmp = summary_fixture();
        let mut app = app_at(tmp.path());
        let mut lines: Vec<String> = (1..=200).map(|i| format!("entry {i}")).collect();
        lines.push("x".repeat(300));
        let lines: Vec<&str> = lines.iter().map(String::as_str).collect();
        with_watched_job(&mut app, &["/docs/a.pdf"], &lines, true);
        app.handle_key(KeyInput::Char('L'));

        let theme = Theme::from_no_color_env(None);
        let screen = render_themed(&mut app, 80, 24, &theme);
        let (cols, rows) = (app.log_cols(), app.log_rows());

        // Everything the frame drew between its own two borders: the
        // pinned header, then the log itself and nothing else.
        let top = screen
            .lines()
            .position(|line| line.contains("job log: agy"))
            .expect(&screen);
        let Mode::JobLog(pane) = &app.mode else {
            panic!("expected the log viewer");
        };
        let position = pane.pager.position(cols, rows, &app.glyphs, Lang::En);
        let bottom = screen
            .lines()
            .position(|line| line.contains(&position))
            .expect(&screen);
        let all: Vec<&str> = screen.lines().collect();
        let inside = &all[top + 1..bottom];
        assert_eq!(inside.len(), rows + HEADER_ROWS, "{screen}");

        // The last line is one 300-column word: it wraps at exactly the
        // width the pane laid it out for - the columns left over once its
        // own gutter has been paid for - and its rows are all on screen.
        let gutter = crate::stream::StreamLine {
            origin: crate::stream::Origin::Out,
            number: 201,
            text: String::new(),
        }
        .gutter()
        .chars()
        .count()
            + 1;
        let widest = inside
            .iter()
            .map(|line| line.matches('x').count())
            .max()
            .unwrap();
        assert_eq!(widest, cols - gutter, "{screen}");
    }

    #[test]
    fn every_frame_size_keeps_its_border_and_row_width() {
        let tmp = listing_fixture(73);
        // Every language, because a Han character owns two cells: a
        // phrase measured by character count instead of display width
        // pushes its row past the border, and this is the assertion that
        // catches it wherever it is written.
        for lang in Lang::ALL {
            let mut app = app_at_in(tmp.path(), lang);
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
                            "{} {width}x{height} row {index} is the wrong width: {line:?}",
                            lang.code()
                        );
                        let last = line.chars().last().unwrap();
                        let expected: &[char] = if theme.ascii {
                            &['|', '+']
                        } else {
                            &['║', '╗', '╝']
                        };
                        assert!(
                            expected.contains(&last),
                            "{} {width}x{height} row {index} lost its right border: {line:?}",
                            lang.code()
                        );
                    }
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
        let mut app = App::new(
            NavState::new(&deep).unwrap(),
            None,
            false,
            Some(home),
            Lang::En,
        );
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
        let name_width = 78 - listing_furniture(Lang::En);
        assert!(name_width >= 46, "name field shrank to {name_width}");
    }

    /// Text the *user* types is as wide as they make it, and in a CJK
    /// locale every character of it owns two cells. The prompt row, the
    /// listing note, and the status row all have to survive that at the
    /// documented minimum size.
    #[test]
    fn a_long_cjk_filter_never_breaks_the_frame() {
        const TYPED: &str = "這是一段非常長的中文篩選字串用來測試邊界是否會被撐破以及游標位置";
        let tmp = tempfile::tempdir().unwrap();
        fs::write(tmp.path().join("notes.md"), "hi").unwrap();
        for lang in Lang::ALL {
            for (width, height) in SIZES {
                for mode in ["filter", "command"] {
                    let mut app = app_at_in(tmp.path(), lang);
                    app.handle_key(KeyInput::Char(if mode == "filter" { '/' } else { ':' }));
                    for c in TYPED.chars() {
                        app.handle_key(KeyInput::Char(c));
                    }
                    let screen =
                        render_themed(&mut app, width, height, &Theme::from_no_color_env(None));
                    for (index, line) in screen.lines().enumerate() {
                        assert_eq!(
                            display_width(line),
                            width as usize,
                            "{} {mode} at {width}x{height} row {index}: {line:?}",
                            lang.code()
                        );
                    }
                }
            }
        }
    }

    /// The browse screen a reader of Traditional Chinese sees: the same
    /// one locus, said in their language. Every element the brief names
    /// is asserted where it is drawn, not where it is written.
    #[test]
    fn the_traditional_chinese_browse_screen_reads_as_one_locus() {
        let tmp = tempfile::tempdir().unwrap();
        fs::create_dir(tmp.path().join("archive")).unwrap();
        fs::write(tmp.path().join("notes.md"), "hi").unwrap();
        let mut app = app_at_in(tmp.path(), Lang::ZhTw);
        app.nav.cursor = 2; // notes.md
        let theme = Theme::from_no_color_env(None);
        let screen = render_themed(&mut app, 80, 24, &theme);

        // The ladder row: the chain, then how deep and how big in words.
        assert!(row(&screen, 1).contains("階層 1 · 2 個項目"), "{screen}");
        // The listing keeps the language-neutral kind markers.
        assert!(screen.contains("archive/"), "{screen}");
        assert!(screen.contains("<DIR>"), "{screen}");
        // The status row states the whole locus.
        let status = row(&screen, status_row(24));
        assert!(status.contains("第 3 列，共 3 列"), "{status}");
        assert!(status.contains("所有項目已顯示"), "{status}");
        assert!(status.contains("notes.md"), "{status}");
        assert!(status.contains("檔案"), "{status}");
        assert!(
            status.contains("前"),
            "an age with no 前 is a duration: {status}"
        );
        // The prompt row and the hint row.
        assert!(row(&screen, 21).contains("指令> 按 : 輸入指令"), "{screen}");
        assert!(
            row(&screen, 22).contains("j/k 移動 · l/Enter 進入"),
            "{screen}"
        );
        // The welcome line was written in the language the app was built
        // with, not translated afterwards.
        assert!(screen.contains("歡迎使用 filecraft"), "{screen}");
    }

    #[test]
    fn a_filter_that_matches_nothing_says_so_in_traditional_chinese() {
        let tmp = tempfile::tempdir().unwrap();
        fs::write(tmp.path().join("notes.md"), "hi").unwrap();
        let mut app = app_at_in(tmp.path(), Lang::ZhTw);
        app.nav.set_filter("zzz".to_string());
        let screen = render_themed(&mut app, 80, 24, &Theme::from_no_color_env(None));
        assert!(screen.contains("沒有項目符合 'zzz'"), "{screen}");
        assert!(
            row(&screen, status_row(24)).contains("篩選 'zzz'"),
            "{screen}"
        );
    }

    /// Every overlay, in Traditional Chinese, saying what the brief says
    /// it should - drawn, not merely defined.
    #[test]
    fn every_overlay_speaks_traditional_chinese() {
        let tmp = summary_fixture();
        let theme = Theme::from_no_color_env(None);

        // The reader.
        let mut app = app_at_in(tmp.path(), Lang::ZhTw);
        select(&mut app, "notes.md");
        app.handle_key(KeyInput::Char('l'));
        let screen = render_themed(&mut app, 80, 24, &theme);
        assert!(screen.contains("第 1 行，共 1 行 · 100%"), "{screen}");
        assert!(screen.contains("閱讀模式"), "{screen}");
        app.handle_key(KeyInput::Char('/'));
        let screen = render_themed(&mut app, 80, 24, &theme);
        assert!(screen.contains("搜尋: "), "{screen}");

        // The folder picker.
        let mut app = app_at_in(tmp.path(), Lang::ZhTw);
        select(&mut app, "notes.md");
        app.execute_line("move");
        let screen = render_themed(&mut app, 80, 24, &theme);
        assert!(screen.contains("目錄選擇器"), "{screen}");
        assert!(screen.contains("目標: "), "{screen}");
        assert!(
            screen.contains("j/k 瀏覽 · l 進入 · h 上層 · Enter/m 選取 · q 取消"),
            "{screen}"
        );

        // The file selector and the provider dialog.
        let mut app = app_at_in(tmp.path(), Lang::ZhTw);
        open_selector(&mut app, &["notes.md"]);
        let screen = render_themed(&mut app, 80, 24, &theme);
        assert!(screen.contains("摘要：選擇檔案"), "{screen}");
        assert!(screen.contains("已選取: 1 個檔案"), "{screen}");
        app.handle_key(KeyInput::Enter);
        let screen = render_themed(&mut app, 80, 24, &theme);
        assert!(screen.contains("選擇 AI 模型"), "{screen}");
        assert!(screen.contains("[預設]"), "{screen}");
        assert!(
            screen.contains("1-5 選擇 · Enter 使用預設 (ag) · q 取消"),
            "{screen}"
        );
        assert!(screen.contains("已選取 1 個檔案"), "{screen}");

        // The live log viewer, over a run that is still going.
        let mut app = app_at_in(tmp.path(), Lang::ZhTw);
        with_watched_job(
            &mut app,
            &["/docs/a.pdf", "/docs/b.md"],
            &["session id: 01a04eef-d4a6-7232", "reading"],
            false,
        );
        app.handle_key(KeyInput::Char('L'));
        let screen = render_themed(&mut app, 100, 30, &theme);
        assert!(screen.contains("日誌檢視: agy"), "{screen}");
        assert!(screen.contains("工作階段 01a04eef-d4a6-7232"), "{screen}");
        assert!(screen.contains("續接: "), "{screen}");
        assert!(
            screen.contains("[AI: 正在使用 agy 摘要 2 個檔案]"),
            "{screen}"
        );
        assert!(screen.contains("行"), "{screen}");
    }

    #[test]
    fn the_log_header_says_finished_in_traditional_chinese_once_a_run_ends() {
        let tmp = summary_fixture();
        let mut app = app_at_in(tmp.path(), Lang::ZhTw);
        with_watched_job(&mut app, &["/docs/a.pdf"], &["done"], true);
        app.handle_key(KeyInput::Char('L'));
        let screen = render_themed(&mut app, 100, 30, &Theme::from_no_color_env(None));
        assert!(screen.contains("完成"), "{screen}");
        assert!(screen.contains("工作階段：agy 未回報"), "{screen}");
    }

    /// The two confirmations the brief pins down, on the row that asks
    /// them.
    #[test]
    fn the_confirmations_ask_in_traditional_chinese() {
        let tmp = summary_fixture();
        let theme = Theme::from_no_color_env(None);

        let mut app = app_at_in(tmp.path(), Lang::ZhTw);
        select(&mut app, "notes.md");
        app.handle_key(KeyInput::Char('d'));
        let screen = render_themed(&mut app, 80, 24, &theme);
        assert!(screen.contains("確認"), "{screen}");
        assert!(screen.contains("[y]是 / [n]否"), "{screen}");
        assert!(screen.contains("將 'notes.md' 移至垃圾桶"), "{screen}");
        assert!(screen.contains("y 確認"), "{screen}");

        let mut app = app_at_in(tmp.path(), Lang::ZhTw);
        with_running_job(&mut app, &["/docs/a.pdf", "/docs/b.md"]);
        app.handle_key(KeyInput::Char('q'));
        let screen = render_themed(&mut app, 100, 30, &theme);
        assert!(
            screen.contains("背景任務執行中：確認終止 AI 摘要並離開？(y/n)"),
            "{screen}"
        );
        assert!(screen.contains("y 終止並離開"), "{screen}");
    }

    /// A CJK name, a CJK age, and a CJK status row on the same screen:
    /// the columns still line up, because every one of them is measured
    /// rather than counted.
    #[test]
    fn a_chinese_screen_of_chinese_names_keeps_its_columns() {
        let tmp = tempfile::tempdir().unwrap();
        fs::write(tmp.path().join("ascii_name.txt"), "").unwrap();
        fs::write(tmp.path().join("中文檔案名稱測試用範例.txt"), "").unwrap();
        let mut app = app_at_in(tmp.path(), Lang::ZhTw);
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
        for line in screen.lines() {
            assert_eq!(display_width(line), 80, "{line:?}");
        }
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
    fn the_delete_prompt_names_the_entry_and_the_two_answers() {
        let (tmp, mut app) = fixture_app();
        let visible = app.nav.visible();
        let pos = visible
            .iter()
            .position(|&i| app.nav.entries[i].name == "readme.md")
            .unwrap();
        app.nav.cursor = pos;
        app.execute_line("delete");
        let screen = render(&app);
        assert!(
            screen.contains("confirm [y]es / [n]o  trash 'readme.md'"),
            "the confirmation must read as one sentence:\n{screen}"
        );
        assert!(
            screen.contains("nothing happens without y"),
            "the hint row must say what inaction does:\n{screen}"
        );
        drop(tmp);
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
        // The commands sit below the first pager page; scrolling brings
        // them into view without skipping anything.
        let mut found = false;
        for _ in 0..80 {
            let screen = render(&app);
            if screen.contains("COMMANDS") && screen.contains("move [destination]") {
                found = true;
                break;
            }
            app.handle_key(KeyInput::Char('j'));
        }
        assert!(
            found,
            "COMMANDS never appeared while scrolling the help pager"
        );
    }

    #[test]
    fn folder_picker_popup_names_the_destination() {
        let (_tmp, mut app) = fixture_app();
        let visible = app.nav.visible();
        let pos = visible
            .iter()
            .position(|&i| app.nav.entries[i].name == "readme.md")
            .unwrap();
        app.nav.cursor = pos;
        app.execute_line("move");
        let theme = Theme::from_no_color_env(None);
        let screen = render_themed(&mut app, 80, 24, &theme);
        assert!(screen.contains("folder picker"), "{screen}");
        assert!(screen.contains("dest:"), "{screen}");
        assert!(screen.contains("./"), "{screen}");
        assert!(screen.contains("../"), "{screen}");
        assert!(screen.contains("projects/"), "{screen}");
        assert!(screen.contains("pick"), "{screen}");
        assert!(screen.contains("moving 'readme.md'"), "{screen}");
        assert!(!screen.contains("readme.md/"), "{screen}");
    }

    #[test]
    fn folder_picker_ascii_theme_stays_inside_printable_ascii_box() {
        let (_tmp, mut app) = fixture_app();
        let visible = app.nav.visible();
        app.nav.cursor = visible
            .iter()
            .position(|&i| app.nav.entries[i].name == "readme.md")
            .unwrap();
        app.execute_line("move");
        let theme = Theme::from_env(Some("1"), Some("1"));
        let screen = render_themed(&mut app, 80, 24, &theme);
        assert!(screen.contains("folder picker"), "{screen}");
        assert!(screen.contains("dest:"), "{screen}");
        assert!(screen.contains("+"), "{screen}");
        assert!(screen.contains("|"), "{screen}");
        assert!(!screen.contains('╔'), "{screen}");
        assert!(!screen.contains('─'), "{screen}");
        for (index, line) in screen.lines().enumerate() {
            assert_eq!(display_width(line), 80, "row {index}: {line:?}");
            let last = line.chars().last().unwrap();
            assert!(
                ['|', '+'].contains(&last),
                "row {index} lost its ascii border: {line:?}"
            );
        }
    }

    #[test]
    fn folder_picker_select_handoff_draws_the_confirm_prompt() {
        let (_tmp, mut app) = fixture_app();
        let visible = app.nav.visible();
        app.nav.cursor = visible
            .iter()
            .position(|&i| app.nav.entries[i].name == "readme.md")
            .unwrap();
        app.execute_line("move");
        let crate::app::Mode::FolderPicker(picker) = &mut app.mode else {
            panic!("expected folder picker");
        };
        picker.cursor = picker
            .entries
            .iter()
            .position(|e| e.name == "projects")
            .expect("projects/ in picker");
        app.handle_key(KeyInput::Enter);
        let theme = Theme::from_no_color_env(None);
        let screen = render_themed(&mut app, 80, 24, &theme);
        assert!(screen.contains("confirm"), "{screen}");
        assert!(screen.contains("[y]es / [n]o"), "{screen}");
        assert!(screen.contains("readme.md"), "{screen}");
        assert!(screen.contains("projects"), "{screen}");
    }

    #[test]
    fn folder_picker_hint_row_never_breaks_a_word() {
        let (_tmp, mut app) = fixture_app();
        let visible = app.nav.visible();
        app.nav.cursor = visible
            .iter()
            .position(|&i| app.nav.entries[i].name == "readme.md")
            .unwrap();
        app.execute_line("move");
        let words = [
            "j/k focus",
            "l in",
            "h up",
            "Enter/m select",
            "q/Esc cancel",
        ];
        for (width, height) in SIZES {
            let theme = Theme::from_no_color_env(None);
            let screen = render_themed(&mut app, width, height, &theme);
            let hints = row(&screen, height as usize - 2);
            let hints = hints.trim_matches(['║', ' ', '|', '+']);
            assert!(!hints.is_empty(), "{width}x{height} lost the hint row");
            assert!(
                words.iter().any(|w| hints.ends_with(w)),
                "{width}x{height} hint row ended mid-word: {hints:?}"
            );
            assert!(
                hints.starts_with("j/k focus"),
                "{width}x{height}: {hints:?}"
            );
        }
    }

    /// The reader over a fixture file, driven exactly as `main` does.
    fn reader_app(name: &str, body: &str) -> (tempfile::TempDir, App) {
        let tmp = tempfile::tempdir().unwrap();
        fs::write(tmp.path().join(name), body).unwrap();
        let mut app = app_at(tmp.path());
        let visible = app.nav.visible();
        app.nav.cursor = visible
            .iter()
            .position(|&i| app.nav.entries[i].name == name)
            .unwrap();
        app.handle_key(KeyInput::Char('l'));
        (tmp, app)
    }

    #[test]
    fn the_reader_draws_markdown_structure_and_says_where_it_is() {
        let (_tmp, mut app) = reader_app(
            "notes.md",
            "# Title\n\nbody with `code` here\n\n- one\n- two\n\n> quoted\n\n---\n",
        );
        let theme = Theme::from_no_color_env(None);
        let screen = render_themed(&mut app, 80, 24, &theme);
        assert!(screen.contains("notes.md"), "{screen}");
        assert!(screen.contains("# Title"), "{screen}");
        assert!(screen.contains("• one"), "{screen}");
        assert!(screen.contains("│ quoted"), "{screen}");
        assert!(
            screen.contains("`code`"),
            "the backticks are the dual: {screen}"
        );
        assert!(screen.contains("line 1 of 10 · 100%"), "{screen}");
        assert!(screen.contains("/ find"), "{screen}");
    }

    #[test]
    fn the_reader_is_given_exactly_the_geometry_it_scrolls_by() {
        // A line one column wider than the reader has must wrap into two
        // rows: the width the app clamps scrolling to is the width the
        // frame actually draws in.
        let (_tmp, mut app) = {
            let tmp = tempfile::tempdir().unwrap();
            fs::write(tmp.path().join("wide.log"), "x".repeat(200)).unwrap();
            let mut app = app_at(tmp.path());
            app.set_viewport(
                24usize - CHROME_ROWS as usize,
                80usize - BORDER_COLS as usize,
            );
            let visible = app.nav.visible();
            app.nav.cursor = visible
                .iter()
                .position(|&i| app.nav.entries[i].name == "wide.log")
                .unwrap();
            app.handle_key(KeyInput::Char('l'));
            (tmp, app)
        };
        let theme = Theme::from_no_color_env(None);
        let cols = app.pager_cols();
        let rows = app.pager_rows();
        let screen = render_themed(&mut app, 80, 24, &theme);
        // Only the rows inside the reader's own frame.
        let body: Vec<&str> = screen
            .lines()
            .skip_while(|line| !line.contains("wide.log"))
            .skip(1)
            .take_while(|line| !line.contains("line 1 of 1"))
            .collect();
        let widths: Vec<usize> = body
            .iter()
            .map(|line| line.matches('x').count())
            .filter(|n| *n > 0)
            .collect();
        // 200 columns of one unbroken word, cut at exactly the width the
        // app clamps scrolling to - and every row of it on screen.
        assert!(rows >= widths.len(), "{screen}");
        assert_eq!(widths, vec![cols, cols, 200 - 2 * cols], "{screen}");
    }

    #[test]
    fn the_picker_is_given_exactly_the_geometry_it_pages_by() {
        // The rows PageUp/PageDown move by are the rows the popup draws:
        // the dest header and both borders are already subtracted.
        let tmp = tempfile::tempdir().unwrap();
        for i in 1..=40 {
            fs::create_dir(tmp.path().join(format!("pick_{i:03}"))).unwrap();
        }
        fs::write(tmp.path().join("note.txt"), "n").unwrap();
        let mut app = app_at(tmp.path());
        let visible = app.nav.visible();
        app.nav.cursor = visible
            .iter()
            .position(|&i| app.nav.entries[i].name == "note.txt")
            .unwrap();
        app.execute_line("move");
        let theme = Theme::from_no_color_env(None);
        let screen = render_themed(&mut app, 80, 24, &theme);
        let rows = app.picker_rows();
        let dest = screen
            .lines()
            .position(|line| line.contains("dest:"))
            .unwrap_or_else(|| panic!("no dest header:\n{screen}"));
        // More folders than fit, so every drawn row carries one.
        let body: Vec<&str> = screen.lines().skip(dest + 1).take(rows).collect();
        assert_eq!(body.len(), rows, "{screen}");
        assert!(body[0].contains("../"), "{screen}");
        assert!(body[1].contains("> ./"), "{screen}");
        assert!(
            body[2..].iter().all(|line| line.contains("pick_")),
            "picker drew fewer rows than it pages by:\n{screen}"
        );
        let after = screen
            .lines()
            .nth(dest + 1 + rows)
            .unwrap_or_else(|| panic!("no row below the folder list:\n{screen}"));
        assert!(
            !after.contains("pick_"),
            "picker drew more rows than it pages by:\n{screen}"
        );
    }

    #[test]
    fn the_reader_never_lets_wide_characters_break_the_frame() {
        let paragraph = "檔案總管視窗介面設計與鍵盤操作".repeat(12);
        let (_tmp, mut app) = reader_app("cjk.md", &format!("# 標題\n\n{paragraph}\n"));
        let theme = Theme::from_no_color_env(None);
        for (width, height) in SIZES {
            let screen = render_themed(&mut app, width, height, &theme);
            for line in screen.lines() {
                assert_eq!(
                    display_width(line),
                    width as usize,
                    "row width drifted at {width}x{height}:\n{screen}"
                );
            }
        }
    }

    #[test]
    fn the_reader_keeps_every_signal_in_text_without_color_or_unicode() {
        let (_tmp, mut app) = reader_app(
            "notes.md",
            "# Title\n\n- one\n\n> quoted\n\n```sh\ncargo test\n```\n",
        );
        let theme = Theme::from_env(Some("1"), Some("1"));
        let screen = render_themed(&mut app, 80, 24, &theme);
        for c in screen.chars().filter(|c| *c != '\n') {
            assert!(
                (' '..='~').contains(&c),
                "non-ascii {c:?} in the ascii reader:\n{screen}"
            );
        }
        // Structure survives with no color and no drawing characters.
        assert!(screen.contains("# Title"), "{screen}");
        assert!(screen.contains("* one"), "{screen}");
        assert!(screen.contains("| quoted"), "{screen}");
        assert!(screen.contains("cargo test"), "{screen}");
    }

    #[test]
    fn the_find_prompt_shows_what_is_being_typed() {
        let (_tmp, mut app) = reader_app("notes.md", "alpha\nbeta\ngamma\n");
        app.handle_key(KeyInput::Char('/'));
        for c in "bet".chars() {
            app.handle_key(KeyInput::Char(c));
        }
        let theme = Theme::from_no_color_env(None);
        let screen = render_themed(&mut app, 80, 24, &theme);
        assert!(screen.contains("find> bet"), "{screen}");
        assert!(screen.contains("Enter search"), "{screen}");
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
        let app = App::new(nav, None, false, None, Lang::En);
        let screen = render(&app);
        assert!(!screen.contains('\u{1b}'));
        assert!(screen.contains("evil\u{FFFD}[31m.txt"));

        let listing = render_static_listing(tmp.path(), Lang::En).unwrap();
        assert!(!listing.contains('\u{1b}'));
        assert!(listing.contains("evil\u{FFFD}[31m.txt"));
    }

    #[test]
    fn control_characters_in_confirm_and_messages_never_reach_the_screen() {
        let tmp = tempfile::tempdir().unwrap();
        fs::create_dir(tmp.path().join("archive")).unwrap();
        fs::write(tmp.path().join("evil\u{1b}]0;pwned\u{7}.txt"), "x").unwrap();
        let nav = NavState::new(tmp.path()).unwrap();
        let mut app = App::new(nav, None, false, None, Lang::En);

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

        let listing = render_static_listing(tmp.path(), Lang::En).unwrap();
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
        let err = render_static_listing(&tmp.path().join("nope"), Lang::En).unwrap_err();
        assert!(matches!(err, FsError::NotFound(_)));
    }
}
