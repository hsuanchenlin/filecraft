//! BBS-style screen rendering with ratatui.
//!
//! One stable full-screen layout: banner border, path bar, listing, status
//! bar, message log, command prompt, and a one-line key hint. High
//! contrast by design: selection uses reverse video, kinds carry textual
//! markers (`/`, `@`, `@!`) and message levels carry textual prefixes, so
//! color is never the only signal. Colors are the terminal's own ANSI
//! palette and can be disabled entirely with `NO_COLOR`.

use std::fmt::Write as _;
use std::path::Path;

use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Clear, Paragraph};
use ratatui::Frame;
use unicode_width::UnicodeWidthChar;

use crate::app::{App, Level, Mode};
use crate::fsops::FsError;
use crate::nav::{EntryKind, NavState};
use crate::preview::{format_size, format_timestamp};

/// Rows used by everything except the file list (borders, path, status,
/// messages, prompt, hints). `main` uses this to size PageUp/PageDown.
pub const CHROME_ROWS: u16 = 9;

/// How many recent messages the BBS log shows.
const MESSAGE_ROWS: usize = 3;

/// Styling switchboard. With `use_color: false` (the `NO_COLOR`
/// convention) every style falls back to bold/reverse on the terminal's
/// default colors.
#[derive(Debug, Clone, Copy)]
pub struct Theme {
    pub use_color: bool,
}

impl Theme {
    /// `NO_COLOR` disables color when set to any non-empty value.
    pub fn from_no_color_env(no_color: Option<&str>) -> Self {
        Theme {
            use_color: no_color.is_none_or(str::is_empty),
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
        "keys: j/k move  Enter open  Backspace/h/l up  / filter  : cmd  ? help  q quit"
    );
    Ok(out)
}

/// Render the whole screen for the current state.
pub fn draw(frame: &mut Frame<'_>, app: &App, theme: &Theme) {
    let area = frame.area();
    let outer = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Double)
        .border_style(theme.banner())
        .title(Line::from(Span::styled(
            format!(" ░▒▓ FILECRAFT v{} ▓▒░ ", env!("CARGO_PKG_VERSION")),
            theme.banner(),
        )));
    let inner = outer.inner(area);
    frame.render_widget(outer, area);

    let [path_row, list_area, status_row, message_area, prompt_row, hint_row] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(1),
        Constraint::Length(1),
        Constraint::Length(MESSAGE_ROWS as u16),
        Constraint::Length(1),
        Constraint::Length(1),
    ])
    .areas(inner);

    draw_path(frame, app, theme, path_row);
    match &app.mode {
        Mode::Pager(pager) => draw_pager(frame, theme, list_area, pager),
        _ => draw_listing(frame, app, theme, list_area),
    }
    draw_status(frame, app, status_row);
    draw_messages(frame, app, theme, message_area);
    draw_prompt(frame, app, theme, prompt_row);
    draw_hints(frame, app, hint_row);
}

fn draw_path(frame: &mut Frame<'_>, app: &App, theme: &Theme, area: Rect) {
    let path = Paragraph::new(Line::from(vec![
        Span::styled(" dir ", theme.prompt()),
        Span::raw(sanitize(&app.nav.cwd.display().to_string())),
    ]));
    frame.render_widget(path, area);
}

fn draw_listing(frame: &mut Frame<'_>, app: &App, theme: &Theme, area: Rect) {
    let visible = app.nav.visible();
    let rows = area.height as usize;
    if rows == 0 {
        return;
    }
    let offset = app.nav.cursor.saturating_sub(rows.saturating_sub(1));

    let mut lines: Vec<Line> = Vec::with_capacity(rows);
    if visible.is_empty() {
        lines.push(Line::from(Span::raw("  (no matching entries)")));
    }
    for (row, &entry_index) in visible.iter().enumerate().skip(offset).take(rows) {
        let entry = &app.nav.entries[entry_index];
        let selected = row == app.nav.cursor;
        let marker = if selected { "> " } else { "  " };

        let size = if entry.is_parent || entry.is_enterable() {
            "<DIR>".to_string()
        } else {
            format_size(entry.size)
        };
        let date = entry.modified.map(format_timestamp).unwrap_or_default();

        let name_width = (area.width as usize).saturating_sub(2 + 8 + 22);
        let name = pad_to_width(&sanitize(&entry.display_name()), name_width);

        let base_style = match entry.kind {
            _ if selected => theme.selected(),
            EntryKind::Dir | EntryKind::SymlinkDir if entry.is_parent => theme.dir(),
            EntryKind::Dir => theme.dir(),
            EntryKind::SymlinkDir | EntryKind::SymlinkFile => theme.symlink(),
            EntryKind::SymlinkBroken => theme.broken(),
            _ => Style::default(),
        };
        lines.push(Line::from(vec![
            Span::styled(marker.to_string(), base_style),
            Span::styled(name, base_style),
            Span::styled(format!(" {size:>6} "), base_style),
            Span::styled(format!(" {date:<16}"), base_style),
        ]));
    }
    frame.render_widget(Paragraph::new(lines), area);
}

fn draw_pager(frame: &mut Frame<'_>, theme: &Theme, area: Rect, pager: &crate::app::Pager) {
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Plain)
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
        .map(|l| Line::from(Span::raw(l.clone())))
        .collect();
    frame.render_widget(Paragraph::new(lines), inner);
}

fn draw_status(frame: &mut Frame<'_>, app: &App, area: Rect) {
    let visible = app.nav.visible();
    let position = if visible.is_empty() {
        "0/0".to_string()
    } else {
        format!("{}/{}", app.nav.cursor + 1, visible.len())
    };
    let filter = if app.nav.filter.is_empty() {
        String::new()
    } else {
        format!("  filter: '{}'", app.nav.filter)
    };
    let hidden = if app.nav.show_hidden {
        "  dotfiles: shown"
    } else {
        ""
    };
    let status = Paragraph::new(Line::from(Span::raw(format!(
        " [{position}]{filter}{hidden}"
    ))));
    frame.render_widget(status, area);
}

fn draw_messages(frame: &mut Frame<'_>, app: &App, theme: &Theme, area: Rect) {
    let start = app.messages.len().saturating_sub(MESSAGE_ROWS);
    let lines: Vec<Line> = app.messages[start..]
        .iter()
        .map(|message| {
            let (prefix, style) = match message.level {
                Level::Info => ("  ·  ", Style::default()),
                Level::Ok => (" ok: ", theme.ok()),
                Level::Error => (" err:", theme.error()),
            };
            Line::from(vec![
                Span::styled(prefix.to_string(), style),
                Span::styled(format!(" {}", sanitize(&message.text)), style),
            ])
        })
        .collect();
    frame.render_widget(Paragraph::new(lines), area);
}

fn draw_prompt(frame: &mut Frame<'_>, app: &App, theme: &Theme, area: Rect) {
    let line = match &app.mode {
        Mode::Command { input } => Line::from(vec![
            Span::styled(" cmd> ", theme.prompt()),
            Span::raw(input.clone()),
            Span::styled("█", theme.prompt()),
        ]),
        Mode::Filter { input } => Line::from(vec![
            Span::styled(" filter> ", theme.prompt()),
            Span::raw(input.clone()),
            Span::styled("█", theme.prompt()),
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
            Span::raw(" j/k scroll · g/G top/bottom · q close"),
        ]),
        Mode::Browse => Line::from(vec![
            Span::styled(" cmd> ", theme.prompt()),
            Span::raw("press : to type a command"),
        ]),
    };
    frame.render_widget(Paragraph::new(line), area);
}

fn draw_hints(frame: &mut Frame<'_>, app: &App, area: Rect) {
    let hints = match &app.mode {
        Mode::Browse => {
            " j/k move · Enter open · h up · / filter · : cmd · . dotfiles · ? help · q quit"
        }
        Mode::Command { .. } => " Enter run · Esc cancel · try: help, cd, move, rename, preview",
        Mode::Filter { .. } => " type to filter · Enter keep · Esc clear",
        Mode::ConfirmOp => " y confirm · n cancel · nothing happens without y",
        Mode::Pager(_) => " j/k scroll · PgUp/PgDn page · q/Esc back to files",
    };
    frame.render_widget(Paragraph::new(Line::from(Span::raw(hints))), area);
}

/// Replace control characters with `U+FFFD` so filesystem-derived names,
/// paths, and messages can never inject terminal escape sequences into
/// the screen. Display-only: stored names keep their real bytes so
/// move/rename/edit still operate on the actual file.
fn sanitize(text: &str) -> String {
    text.chars()
        .map(|c| if c.is_control() { '\u{FFFD}' } else { c })
        .collect()
}

/// Pad or truncate `text` to exactly `width` display columns, appending
/// `…` when truncated. Width-aware so CJK names keep columns aligned.
fn pad_to_width(text: &str, width: usize) -> String {
    if width == 0 {
        return String::new();
    }
    let text_width: usize = text.chars().map(|c| c.width().unwrap_or(0)).sum();
    if text_width <= width {
        let mut out = text.to_string();
        out.extend(std::iter::repeat_n(' ', width - text_width));
        return out;
    }
    let mut out = String::new();
    let mut used = 0usize;
    for c in text.chars() {
        let w = c.width().unwrap_or(0);
        if used + w > width - 1 {
            break;
        }
        out.push(c);
        used += w;
    }
    out.push('…');
    used += 1;
    while used < width {
        out.push(' ');
        used += 1;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::{App, KeyInput};
    use crate::nav::NavState;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;
    use std::fs;

    fn render(app: &App) -> String {
        render_size(app, 90, 30)
    }

    fn render_size(app: &App, width: u16, height: u16) -> String {
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).unwrap();
        let theme = Theme { use_color: true };
        terminal.draw(|f| draw(f, app, &theme)).unwrap();
        let buffer = terminal.backend().buffer().clone();
        let mut out = String::new();
        for y in 0..buffer.area.height {
            for x in 0..buffer.area.width {
                out.push_str(buffer[(x, y)].symbol());
            }
            out.push('\n');
        }
        out
    }

    fn fixture_app() -> (tempfile::TempDir, App) {
        let tmp = tempfile::tempdir().unwrap();
        fs::create_dir(tmp.path().join("projects")).unwrap();
        fs::write(tmp.path().join("readme.md"), "hi").unwrap();
        let nav = NavState::new(tmp.path()).unwrap();
        (tmp, App::new(nav, None, false, None))
    }

    #[test]
    fn screen_shows_banner_path_listing_and_hints() {
        let (_tmp, app) = fixture_app();
        let screen = render(&app);
        assert!(screen.contains("FILECRAFT"));
        assert!(screen.contains(&app.nav.cwd.display().to_string()));
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
        assert!(screen.contains("COMMANDS"));
        assert!(screen.contains("KEYS"));
        // The move line sits just below the first pager page; one row down
        // brings it into view without skipping it.
        app.handle_key(KeyInput::Char('j'));
        let screen = render(&app);
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
        assert_eq!(sanitize("bell\u{7}tab\tnl\n"), "bell\u{FFFD}tab\u{FFFD}nl\u{FFFD}");
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
