//! The Filecraft state machine.
//!
//! [`App::handle_key`] consumes abstract [`KeyInput`]s and returns
//! [`Effect`]s; the terminal event loop in `main.rs` translates real key
//! events in and interprets effects out. The app itself never touches the
//! terminal, so every interaction - including move/rename confirmation -
//! is deterministically testable.

use std::path::{Path, PathBuf};

use crate::agent::{self, Agent, AgentRequest};
use crate::bearings::{self, Glyphs, Ladder};
use crate::command::{self, Command};
use crate::editor;
use crate::fsops::{self, FsError};
use crate::markdown::{self, DocLine};
use crate::nav::NavState;
use crate::pager::{self, Pager};
use crate::picker::{self, FolderPicker};
use crate::preview::{self, PreviewData, ViewSource};
use crate::trash::{self, Trasher};

/// Abstract key input, decoupled from any terminal backend.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyInput {
    Char(char),
    Enter,
    Esc,
    Backspace,
    Up,
    Down,
    Left,
    Right,
    PageUp,
    PageDown,
    Home,
    End,
    CtrlC,
}

/// What the event loop must do after a key was handled.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Effect {
    None,
    /// Leave the TUI and exit cleanly.
    Quit,
    /// Suspend the TUI, run `argv` with inherited stdio (an editor), wait,
    /// then restore the screen.
    RunInteractive {
        argv: Vec<String>,
    },
    /// Spawn `argv` detached (macOS `open`); the TUI stays up.
    SpawnDetached {
        argv: Vec<String>,
    },
}

/// Which input surface currently owns the keyboard.
#[derive(Debug, Clone, PartialEq, Eq)]
// `Pager` owns its document and layout cache; boxing it would complicate the
// public state-machine API solely to reduce the size of this short-lived enum.
#[allow(clippy::large_enum_variant)]
pub enum Mode {
    Browse,
    Command { input: String },
    Filter { input: String },
    ConfirmOp,
    FolderPicker(FolderPicker),
    Pager(Pager),
}

/// A move, rename, or trash waiting for explicit confirmation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PendingOp {
    Move {
        src: PathBuf,
        dst: PathBuf,
    },
    Rename {
        src: PathBuf,
        dst: PathBuf,
    },
    /// Move `src` to the system Trash. `name` is what the prompt says,
    /// because the listing already says which directory it is in.
    Trash {
        src: PathBuf,
        name: String,
    },
}

impl PendingOp {
    /// One-line description shown in the confirmation prompt; always shows
    /// the canonical destination.
    pub fn describe(&self) -> String {
        match self {
            PendingOp::Move { src, dst } => {
                format!("move '{}' -> '{}'", src.display(), dst.display())
            }
            PendingOp::Rename { src, dst } => format!(
                "rename '{}' -> '{}'",
                src.file_name().unwrap_or_default().to_string_lossy(),
                dst.file_name().unwrap_or_default().to_string_lossy()
            ),
            PendingOp::Trash { name, .. } => format!("trash '{name}'"),
        }
    }
}

/// Severity of a BBS message line. Levels also carry a textual prefix in
/// the UI so color is never the only signal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Level {
    Info,
    Ok,
    Error,
}

impl Level {
    /// The level's textual dual, in the character set the screen is
    /// drawing with. All three are exactly five columns wide, so the log
    /// body stays flush whatever levels are in the ring - the message
    /// strip and the `M` pager share this one table.
    pub fn prefix(self, glyphs: &Glyphs) -> String {
        match self {
            Level::Info => format!("  {}  ", glyphs.dot),
            Level::Ok => " ok: ".to_string(),
            Level::Error => " err:".to_string(),
        }
    }
}

/// One line in the BBS message log.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Message {
    pub level: Level,
    pub text: String,
}

const MAX_MESSAGES: usize = 100;

/// Full application state.
pub struct App {
    pub nav: NavState,
    pub mode: Mode,
    pub pending: Option<PendingOp>,
    pub messages: Vec<Message>,
    /// `$EDITOR` captured at startup (None when unset).
    pub editor_env: Option<String>,
    /// Whether `nvim` was found on `$PATH` at startup.
    pub nvim_on_path: bool,
    /// Home directory captured at startup, for `~` and bare `cd`.
    pub home: Option<PathBuf>,
    /// Rows available to the file list; the UI updates this every frame so
    /// PageUp/PageDown match what is on screen.
    pub viewport_rows: usize,
    /// Columns available inside the border; the UI updates this every
    /// frame so the ladder's digit keys address exactly the rungs drawn.
    pub viewport_cols: usize,
    /// Drawing characters in force, so key handling and rendering fit the
    /// ladder to identical widths.
    pub glyphs: Glyphs,
    /// Where `delete` sends an entry. The system Trash in the shipped
    /// binary; a directory under a fixture in tests.
    pub trasher: Box<dyn Trasher>,
}

impl App {
    pub fn new(
        nav: NavState,
        editor_env: Option<String>,
        nvim_on_path: bool,
        home: Option<PathBuf>,
    ) -> Self {
        let mut app = App {
            nav,
            mode: Mode::Browse,
            pending: None,
            messages: Vec::new(),
            editor_env,
            nvim_on_path,
            home,
            viewport_rows: 20,
            viewport_cols: 80,
            glyphs: Glyphs::UNICODE,
            trasher: trash::system(),
        };
        app.push_msg(
            Level::Info,
            "welcome to filecraft - press ? for help, : for commands".to_string(),
        );
        app
    }

    pub fn push_msg(&mut self, level: Level, text: String) {
        self.messages.push(Message { level, text });
        if self.messages.len() > MAX_MESSAGES {
            let excess = self.messages.len() - MAX_MESSAGES;
            self.messages.drain(..excess);
        }
    }

    fn err(&mut self, text: String) -> Effect {
        self.push_msg(Level::Error, text);
        Effect::None
    }

    /// Route a key according to the current mode.
    pub fn handle_key(&mut self, key: KeyInput) -> Effect {
        if key == KeyInput::CtrlC {
            return Effect::Quit;
        }
        match &self.mode {
            Mode::Browse => self.handle_browse_key(key),
            Mode::Command { .. } => self.handle_command_key(key),
            Mode::Filter { .. } => self.handle_filter_key(key),
            Mode::ConfirmOp => self.handle_confirm_key(key),
            Mode::FolderPicker(_) => self.handle_picker_key(key),
            Mode::Pager(_) => self.handle_pager_key(key),
        }
    }

    fn handle_browse_key(&mut self, key: KeyInput) -> Effect {
        match key {
            KeyInput::Char('j') | KeyInput::Down => {
                self.nav.move_cursor(1);
                Effect::None
            }
            KeyInput::Char('k') | KeyInput::Up => {
                self.nav.move_cursor(-1);
                Effect::None
            }
            KeyInput::PageDown => {
                self.nav.move_cursor(self.viewport_rows as isize);
                Effect::None
            }
            KeyInput::PageUp => {
                self.nav.move_cursor(-(self.viewport_rows as isize));
                Effect::None
            }
            KeyInput::Char('g') | KeyInput::Home => {
                self.nav.cursor_to_start();
                Effect::None
            }
            KeyInput::Char('G') | KeyInput::End => {
                self.nav.cursor_to_end();
                Effect::None
            }
            KeyInput::Enter => self.activate_selection(),
            KeyInput::Backspace | KeyInput::Left | KeyInput::Char('h') => self.go_up(),
            KeyInput::Right | KeyInput::Char('l') => self.open_selected(),
            // Digits address the ancestor ladder; `0` is always the anchor.
            KeyInput::Char(c) if c.is_ascii_digit() => self.jump_to_rung(c as u8 - b'0'),
            // The one browse key that arms an operation. It changes
            // nothing by itself: it raises the same y/n prompt `:delete`
            // does, and only `y` moves anything.
            KeyInput::Char('d') => self.cmd_trash(),
            KeyInput::Char('M') => self.show_messages(),
            KeyInput::Char('/') => {
                self.mode = Mode::Filter {
                    input: self.nav.filter.clone(),
                };
                Effect::None
            }
            KeyInput::Char(':') => {
                self.mode = Mode::Command {
                    input: String::new(),
                };
                Effect::None
            }
            KeyInput::Char('?') => self.show_help(),
            KeyInput::Char('.') => {
                if let Err(e) = self.nav.toggle_hidden() {
                    return self.err(e.to_string());
                }
                let state = if self.nav.show_hidden {
                    "shown"
                } else {
                    "hidden"
                };
                self.push_msg(Level::Info, format!("dotfiles {state}"));
                Effect::None
            }
            KeyInput::Char('r') => {
                if let Err(e) = self.nav.refresh() {
                    return self.err(e.to_string());
                }
                self.push_msg(Level::Info, "refreshed".to_string());
                Effect::None
            }
            KeyInput::Char('q') => Effect::Quit,
            // Esc backs out exactly one level; quitting is `q` or Ctrl-C.
            KeyInput::Esc => self.back_out(),
            _ => Effect::None,
        }
    }

    /// One step out of whatever the user is inside. In browse mode the
    /// only thing to leave is an active filter; with none, Esc does
    /// nothing at all - it never quits.
    fn back_out(&mut self) -> Effect {
        if self.nav.filter.is_empty() {
            return Effect::None;
        }
        self.nav.set_filter(String::new());
        self.push_msg(Level::Info, "filter cleared".to_string());
        Effect::None
    }

    fn handle_command_key(&mut self, key: KeyInput) -> Effect {
        let Mode::Command { input } = &mut self.mode else {
            return Effect::None;
        };
        match key {
            KeyInput::Char(c) => {
                input.push(c);
                Effect::None
            }
            KeyInput::Backspace => {
                input.pop();
                Effect::None
            }
            KeyInput::Esc => {
                self.mode = Mode::Browse;
                Effect::None
            }
            KeyInput::Enter => {
                let line = input.clone();
                self.mode = Mode::Browse;
                if line.trim().is_empty() {
                    Effect::None
                } else {
                    self.execute_line(&line)
                }
            }
            _ => Effect::None,
        }
    }

    fn handle_filter_key(&mut self, key: KeyInput) -> Effect {
        let Mode::Filter { input } = &mut self.mode else {
            return Effect::None;
        };
        match key {
            KeyInput::Char(c) => {
                input.push(c);
                let filter = input.clone();
                self.nav.set_filter(filter);
                Effect::None
            }
            KeyInput::Backspace => {
                input.pop();
                let filter = input.clone();
                self.nav.set_filter(filter);
                Effect::None
            }
            KeyInput::Esc => {
                self.nav.set_filter(String::new());
                self.mode = Mode::Browse;
                self.push_msg(Level::Info, "filter cleared".to_string());
                Effect::None
            }
            KeyInput::Enter => {
                self.mode = Mode::Browse;
                Effect::None
            }
            _ => Effect::None,
        }
    }

    fn handle_confirm_key(&mut self, key: KeyInput) -> Effect {
        match key {
            KeyInput::Char('y') | KeyInput::Char('Y') | KeyInput::Enter => self.perform_pending(),
            // `q` cancels here, as it does in the reader and the folder
            // picker: the back-out key never means "go ahead".
            KeyInput::Char('n') | KeyInput::Char('N') | KeyInput::Char('q') | KeyInput::Esc => {
                let description = self
                    .pending
                    .take()
                    .map(|op| op.describe())
                    .unwrap_or_default();
                self.mode = Mode::Browse;
                self.push_msg(Level::Info, format!("cancelled: {description}"));
                Effect::None
            }
            _ => {
                self.push_msg(
                    Level::Info,
                    "press y to confirm, or n / q / Esc to cancel".to_string(),
                );
                Effect::None
            }
        }
    }

    /// Mirror the terminal's geometry into the state key handling and
    /// the reader compute against, and re-establish the reader's one
    /// invariant against it: the offset never points past the last page.
    ///
    /// Every surface that drives the app - the event loop, the golden
    /// frame tests - goes through here, so a resize can never leave
    /// `top_line`, the position footer, and `n`/`N` reading a stale
    /// offset while the screen shows the real last page.
    pub fn set_viewport(&mut self, rows: usize, cols: usize) {
        self.viewport_rows = rows.max(1);
        self.viewport_cols = cols;
        let (width, view, glyphs) = (self.pager_cols(), self.pager_rows(), self.glyphs);
        if let Mode::Pager(pager) = &mut self.mode {
            pager.clamp(width, view, &glyphs);
        }
    }

    /// Columns of text the reader has, mirrored from the terminal the
    /// same way the ladder's width is: the frame it draws sits inside the
    /// listing area, so scrolling and drawing agree on what a row is.
    pub fn pager_cols(&self) -> usize {
        self.viewport_cols.saturating_sub(pager::FRAME_COLS).max(1)
    }

    /// Rows of text the reader has.
    pub fn pager_rows(&self) -> usize {
        self.viewport_rows.saturating_sub(pager::FRAME_ROWS).max(1)
    }

    fn handle_pager_key(&mut self, key: KeyInput) -> Effect {
        if matches!(&self.mode, Mode::Pager(p) if p.find.is_some()) {
            return self.handle_find_key(key);
        }
        let (width, view, glyphs) = (self.pager_cols(), self.pager_rows(), self.glyphs);
        let page = view as isize;
        let half = (view as isize / 2).max(1);
        let mut close = false;
        let mut missed: Option<(Level, String)> = None;
        {
            let Mode::Pager(pager) = &mut self.mode else {
                return Effect::None;
            };
            match key {
                KeyInput::Char('j') | KeyInput::Down => {
                    pager.scroll_by(1, width, view, &glyphs);
                }
                KeyInput::Char('k') | KeyInput::Up => {
                    pager.scroll_by(-1, width, view, &glyphs);
                }
                KeyInput::Char('d') => pager.scroll_by(half, width, view, &glyphs),
                KeyInput::Char('u') => pager.scroll_by(-half, width, view, &glyphs),
                KeyInput::Char('f') | KeyInput::PageDown => {
                    pager.scroll_by(page, width, view, &glyphs)
                }
                KeyInput::Char('b') | KeyInput::PageUp => {
                    pager.scroll_by(-page, width, view, &glyphs)
                }
                KeyInput::Char('g') | KeyInput::Home => pager.scroll = 0,
                KeyInput::Char('G') | KeyInput::End => pager.scroll_to_end(width, view, &glyphs),
                KeyInput::Char('/') => pager.find = Some(String::new()),
                KeyInput::Char('n') | KeyInput::Char('N') => {
                    let forward = key == KeyInput::Char('n');
                    if pager.query.is_empty() {
                        missed = Some((Level::Info, "no search yet - press / to find".to_string()));
                    } else if !pager.step_match(forward, width, view, &glyphs) {
                        missed = Some((Level::Error, format!("no match for '{}'", pager.query)));
                    }
                }
                KeyInput::Char('q')
                | KeyInput::Char('h')
                | KeyInput::Left
                | KeyInput::Esc
                | KeyInput::Enter => close = true,
                _ => {}
            }
        }
        if close {
            // The listing is untouched underneath, so closing lands on
            // exactly the row the reader was opened from.
            self.mode = Mode::Browse;
        }
        if let Some((level, text)) = missed {
            self.push_msg(level, text);
        }
        Effect::None
    }

    /// The `/` prompt inside the reader. Esc leaves the search, not the
    /// reader - backing out is always exactly one level.
    fn handle_find_key(&mut self, key: KeyInput) -> Effect {
        let (width, view, glyphs) = (self.pager_cols(), self.pager_rows(), self.glyphs);
        let mut missed: Option<String> = None;
        {
            let Mode::Pager(pager) = &mut self.mode else {
                return Effect::None;
            };
            let Some(input) = pager.find.as_mut() else {
                return Effect::None;
            };
            match key {
                KeyInput::Char(c) => input.push(c),
                KeyInput::Backspace => {
                    input.pop();
                }
                KeyInput::Esc => pager.find = None,
                KeyInput::Enter => {
                    let query = std::mem::take(input);
                    pager.find = None;
                    pager.query = query;
                    if pager.query.is_empty() {
                        // An empty query clears the highlight, nothing else.
                    } else if !pager.seek_match(width, view, &glyphs) {
                        missed = Some(format!("no match for '{}'", pager.query));
                    }
                }
                _ => {}
            }
        }
        if let Some(text) = missed {
            return self.err(text);
        }
        Effect::None
    }

    /// Enter on the current selection: directories are entered, files are
    /// handed to the editor. Never automatic - always this explicit key.
    fn activate_selection(&mut self) -> Effect {
        let Some(entry) = self.nav.selected().cloned() else {
            return self.err("nothing selected".to_string());
        };
        if entry.is_parent {
            return self.go_up();
        }
        if entry.is_enterable() {
            return self.enter_selected_dir();
        }
        if entry.is_file_like() {
            return self.cmd_edit();
        }
        match entry.kind {
            crate::nav::EntryKind::SymlinkBroken => {
                self.err(format!("broken symlink: '{}' points nowhere", entry.name))
            }
            _ => self.err(format!("cannot open special file '{}'", entry.name)),
        }
    }

    /// `l` / Right: descend into a directory, or open a text file in the
    /// read-only reader. Both halves are read-only - this key never
    /// launches an editor and never touches the file.
    fn open_selected(&mut self) -> Effect {
        let Some(entry) = self.nav.selected().cloned() else {
            return self.err("nothing selected".to_string());
        };
        if entry.is_parent {
            return self.go_up();
        }
        if entry.is_enterable() {
            return self.enter_selected_dir();
        }
        if entry.is_file_like() {
            return self.open_pager_for_file();
        }
        match entry.kind {
            crate::nav::EntryKind::SymlinkBroken => {
                self.err(format!("broken symlink: '{}' points nowhere", entry.name))
            }
            _ => self.err(format!("cannot read special file '{}'", entry.name)),
        }
    }

    /// Open the selected regular file in the reader. Markdown gets its
    /// structure drawn; anything else readable is shown as it is; a
    /// binary is refused in words rather than painted on the screen.
    fn open_pager_for_file(&mut self) -> Effect {
        let (name, path) = match self.selected_operand() {
            Ok(v) => v,
            Err(e) => return self.err(format!("read: {e}")),
        };
        let source = match preview::read_view(&path) {
            Ok(source) => source,
            Err(e) => return self.err(format!("read: {e}")),
        };
        let ViewSource::Text { text, truncated } = source else {
            return self.err(format!(
                "cannot read '{name}' as text - it is binary; try ':' then open"
            ));
        };
        let mut doc = if text.is_empty() {
            vec![DocLine::meta("(empty file)")]
        } else if markdown::is_markdown(&path) {
            markdown::parse_markdown(&text)
        } else {
            markdown::parse_plain(&text)
        };
        if truncated {
            doc.push(DocLine::meta(format!(
                "(truncated at {} lines / {} KiB)",
                preview::MAX_VIEW_LINES,
                preview::MAX_VIEW_BYTES / 1024
            )));
        }
        self.mode = Mode::Pager(Pager::document(name, doc));
        Effect::None
    }

    fn enter_selected_dir(&mut self) -> Effect {
        let Some(entry) = self.nav.selected().cloned() else {
            return self.err("nothing selected".to_string());
        };
        if entry.is_parent {
            return self.go_up();
        }
        if !entry.is_enterable() {
            return self.err(format!("'{}' is not a directory", entry.name));
        }
        let path = self.nav.cwd.join(&entry.name);
        let canonical = match std::fs::canonicalize(&path) {
            Ok(c) => c,
            Err(e) => return self.err(fsops::io_error(&path, &e).to_string()),
        };
        match self.nav.change_dir(canonical, None) {
            Ok(()) => Effect::None,
            Err(e) => self.err(e.to_string()),
        }
    }

    /// The ancestor ladder as it is drawn right now.
    ///
    /// Rendering and the digit keys share this one computation, so every
    /// digit on screen is a key that works and no key addresses a rung
    /// the elision hid.
    pub fn ladder(&self) -> Ladder {
        self.ladder_in(self.viewport_cols, &self.glyphs)
    }

    /// The ladder as it is drawn in `cols` columns. The renderer passes
    /// the real width of the row; [`App::ladder`] passes the width the
    /// event loop last reported, and they are the same number.
    pub fn ladder_in(&self, cols: usize, glyphs: &Glyphs) -> Ladder {
        let summary = self.ladder_summary_with(glyphs);
        let layout = bearings::ladder_row(cols, bearings::display_width(&summary));
        bearings::ladder(
            &self.nav.cwd,
            self.home.as_deref(),
            layout.chain_budget,
            glyphs,
        )
    }

    /// The ladder's textual dual: depth and size in words, never implied
    /// by the shape of the chain alone.
    pub fn ladder_summary(&self) -> String {
        self.ladder_summary_with(&self.glyphs)
    }

    /// [`App::ladder_summary`] in a given character set.
    pub fn ladder_summary_with(&self, glyphs: &Glyphs) -> String {
        let depth = bearings::depth_of(&self.nav.cwd, self.home.as_deref());
        let items = self.nav.entries.iter().filter(|e| !e.is_parent).count();
        let unit = if items == 1 { "item" } else { "items" };
        format!("depth {depth} {} {items} {unit}", glyphs.dot)
    }

    /// Jump to a visible ancestor. Pure navigation: it goes through
    /// `NavState::change_dir` exactly as `cd` does, and selects the child
    /// it came through the way going up does.
    fn jump_to_rung(&mut self, digit: u8) -> Effect {
        let ladder = self.ladder();
        let Some(rung) = ladder.rung(digit) else {
            return self.err(format!("no ancestor '{digit}' on the ladder"));
        };
        let (target, label) = (rung.path.clone(), rung.label.clone());
        if target == self.nav.cwd {
            self.push_msg(Level::Info, format!("already at {label}"));
            return Effect::None;
        }
        let select = self
            .nav
            .cwd
            .strip_prefix(&target)
            .ok()
            .and_then(|rest| rest.components().next())
            .map(|c| c.as_os_str().to_string_lossy().into_owned());
        match self.nav.change_dir(target, select.as_deref()) {
            Ok(()) => {
                let cwd = self.nav.cwd.display().to_string();
                self.push_msg(Level::Ok, format!("cwd: {cwd}"));
                Effect::None
            }
            Err(e) => self.err(e.to_string()),
        }
    }

    /// Open the message ring in the existing pager. The log keeps a
    /// hundred lines but the strip shows three; this is how the other
    /// ninety-seven are reachable.
    fn show_messages(&mut self) -> Effect {
        let lines: Vec<String> = if self.messages.is_empty() {
            vec!["(no messages yet)".to_string()]
        } else {
            self.messages
                .iter()
                .map(|message| format!("{} {}", message.level.prefix(&self.glyphs), message.text))
                .collect()
        };
        self.mode = Mode::Pager(Pager::plain(
            format!("messages ({} of {MAX_MESSAGES})", self.messages.len()),
            lines,
        ));
        Effect::None
    }

    fn go_up(&mut self) -> Effect {
        match self.nav.go_up() {
            Ok(true) => Effect::None,
            Ok(false) => {
                self.push_msg(Level::Info, "already at the filesystem root".to_string());
                Effect::None
            }
            Err(e) => self.err(e.to_string()),
        }
    }

    /// Parse and run one BBS command line. Public for the prompt and for
    /// tests.
    pub fn execute_line(&mut self, line: &str) -> Effect {
        match command::parse(line) {
            Ok(cmd) => self.execute(cmd),
            Err(e) => self.err(e.to_string()),
        }
    }

    fn execute(&mut self, cmd: Command) -> Effect {
        match cmd {
            Command::Cd { path } => self.cmd_cd(path),
            Command::Move { destination: None } => self.open_move_picker(),
            Command::Move {
                destination: Some(destination),
            } => self.cmd_move(&destination),
            Command::Rename { name } => self.cmd_rename(&name),
            Command::Trash => self.cmd_trash(),
            Command::Open => self.cmd_open(),
            Command::Edit => self.cmd_edit(),
            Command::Preview => self.cmd_preview(),
            Command::Help => self.show_help(),
            Command::Quit => Effect::Quit,
            Command::Agent { args } => self.cmd_agent(args),
        }
    }

    fn cmd_cd(&mut self, path: Option<String>) -> Effect {
        let target = match path {
            Some(p) => p,
            None => match &self.home {
                Some(home) => home.display().to_string(),
                None => return self.err("cd: home directory unknown".to_string()),
            },
        };
        let dir = match fsops::canonical_dir(&self.nav.cwd, &target, self.home.as_deref()) {
            Ok(d) => d,
            Err(e) => return self.err(format!("cd: {e}")),
        };
        match self.nav.change_dir(dir, None) {
            Ok(()) => {
                let cwd = self.nav.cwd.display().to_string();
                self.push_msg(Level::Ok, format!("cwd: {cwd}"));
                Effect::None
            }
            Err(e) => self.err(format!("cd: {e}")),
        }
    }

    /// Resolve the selection for an operation; the synthetic `..` row is
    /// never a valid target.
    fn selected_operand(&self) -> Result<(String, PathBuf), String> {
        let Some(entry) = self.nav.selected() else {
            return Err("nothing selected".to_string());
        };
        if entry.is_parent {
            return Err("cannot operate on '..' - select a real entry".to_string());
        }
        Ok((entry.name.clone(), self.nav.cwd.join(&entry.name)))
    }

    /// `:move` with no path: pick a destination folder, then confirm.
    fn open_move_picker(&mut self) -> Effect {
        let (name, src) = match self.selected_operand() {
            Ok(v) => v,
            Err(e) => return self.err(format!("move: {e}")),
        };
        match FolderPicker::open(&self.nav.cwd, name, src, self.nav.show_hidden) {
            Ok(picker) => {
                self.mode = Mode::FolderPicker(picker);
                Effect::None
            }
            Err(e) => self.err(format!("move: {e}")),
        }
    }

    /// Rows of folders the picker has, mirrored from the listing area
    /// the same way the reader's rows are: the dest header and borders
    /// sit inside that area, so paging and drawing agree on a row.
    pub fn picker_rows(&self) -> usize {
        self.viewport_rows.saturating_sub(picker::FRAME_ROWS).max(1)
    }

    fn handle_picker_key(&mut self, key: KeyInput) -> Effect {
        let rows = self.picker_rows();
        let mut select = false;
        let mut cancel = false;
        let mut err: Option<String> = None;
        let mut info: Option<String> = None;
        {
            let Mode::FolderPicker(picker) = &mut self.mode else {
                return Effect::None;
            };
            match key {
                KeyInput::Char('j') | KeyInput::Down => picker.move_cursor(1),
                KeyInput::Char('k') | KeyInput::Up => picker.move_cursor(-1),
                KeyInput::PageDown => picker.move_cursor(rows as isize),
                KeyInput::PageUp => picker.move_cursor(-(rows as isize)),
                KeyInput::Char('g') | KeyInput::Home => picker.cursor_to_start(),
                KeyInput::Char('G') | KeyInput::End => picker.cursor_to_end(),
                KeyInput::Char('l') | KeyInput::Right => {
                    if let Err(e) = picker.enter_focused() {
                        err = Some(format!("move: {e}"));
                    }
                }
                KeyInput::Backspace | KeyInput::Left | KeyInput::Char('h') => {
                    match picker.go_up() {
                        Ok(true) => {}
                        Ok(false) => {
                            info = Some("already at the filesystem root".to_string());
                        }
                        Err(e) => err = Some(format!("move: {e}")),
                    }
                }
                KeyInput::Enter | KeyInput::Char('m') => select = true,
                KeyInput::Esc | KeyInput::Char('q') => cancel = true,
                _ => {}
            }
        }
        if cancel {
            self.mode = Mode::Browse;
            self.push_msg(Level::Info, "cancelled: folder picker".to_string());
            return Effect::None;
        }
        if select {
            return self.confirm_picker_destination();
        }
        if let Some(text) = err {
            return self.err(text);
        }
        if let Some(text) = info {
            self.push_msg(Level::Info, text);
        }
        Effect::None
    }

    fn confirm_picker_destination(&mut self) -> Effect {
        let dest = match &self.mode {
            Mode::FolderPicker(picker) => picker.destination().to_string_lossy().into_owned(),
            _ => return Effect::None,
        };
        self.cmd_move(&dest)
    }

    fn cmd_move(&mut self, destination: &str) -> Effect {
        let (name, src) = match self.selected_operand() {
            Ok(v) => v,
            Err(e) => return self.err(format!("move: {e}")),
        };
        let dst = match fsops::canonical_move_target(
            &self.nav.cwd,
            destination,
            &name,
            self.home.as_deref(),
        ) {
            Ok(d) => d,
            Err(e) => return self.err(format!("move: {e}")),
        };
        if src == dst {
            return self.err("move: source and destination are the same".to_string());
        }
        if std::fs::symlink_metadata(&dst).is_ok() && !fsops::same_file(&src, &dst) {
            return self.err(format!("move: {}", FsError::AlreadyExists(dst)));
        }
        let src_is_dir = std::fs::symlink_metadata(&src)
            .map(|m| m.is_dir())
            .unwrap_or(false);
        if src_is_dir && dst.starts_with(&src) {
            return self.err("move: cannot move a directory into itself".to_string());
        }
        let op = PendingOp::Move { src, dst };
        self.push_msg(Level::Info, format!("confirm: {} (y/n)", op.describe()));
        self.pending = Some(op);
        self.mode = Mode::ConfirmOp;
        Effect::None
    }

    fn cmd_rename(&mut self, new_name: &str) -> Effect {
        let (name, src) = match self.selected_operand() {
            Ok(v) => v,
            Err(e) => return self.err(format!("rename: {e}")),
        };
        if let Err(e) = fsops::validate_new_name(new_name) {
            return self.err(format!("rename: {e}"));
        }
        if new_name == name {
            return self.err("rename: that is already the current name".to_string());
        }
        let dst = self.nav.cwd.join(new_name);
        if std::fs::symlink_metadata(&dst).is_ok() && !fsops::same_file(&src, &dst) {
            return self.err(format!("rename: {}", FsError::AlreadyExists(dst)));
        }
        let op = PendingOp::Rename { src, dst };
        self.push_msg(Level::Info, format!("confirm: {} (y/n)", op.describe()));
        self.pending = Some(op);
        self.mode = Mode::ConfirmOp;
        Effect::None
    }

    /// `:delete` / `:trash` / `d` - arm a recoverable move to the system
    /// Trash. Nothing leaves the directory until the prompt is answered
    /// with `y`.
    fn cmd_trash(&mut self) -> Effect {
        let (name, src) = match self.selected_operand() {
            Ok(v) => v,
            Err(e) => return self.err(format!("delete: {e}")),
        };
        if let Err(e) = trash::check_trashable(&src) {
            return self.err(format!("delete: {e}"));
        }
        if let Err(e) = std::fs::symlink_metadata(&src) {
            return self.err(format!("delete: {}", fsops::io_error(&src, &e)));
        }
        let op = PendingOp::Trash { src, name };
        self.push_msg(Level::Info, format!("confirm: {} (y/n)", op.describe()));
        self.pending = Some(op);
        self.mode = Mode::ConfirmOp;
        Effect::None
    }

    /// Run the armed trash, then re-read the listing. Split out of
    /// [`App::perform_pending`] because it is the one confirmed operation
    /// that is not a rename underneath.
    fn perform_trash(&mut self, src: &Path, name: &str) -> Effect {
        // Re-checked at the moment of execution, not only when armed:
        // between the two the listing may have been refreshed.
        if let Err(e) = trash::check_trashable(src) {
            return self.err(format!("delete: {e}"));
        }
        match self.trasher.trash(src) {
            Ok(()) => {
                let where_to = self.trasher.destination();
                self.push_msg(
                    Level::Ok,
                    format!("trashed '{name}' -> {where_to} (recoverable in Finder)"),
                );
                if let Err(e) = self.nav.refresh() {
                    return self.err(e.to_string());
                }
                Effect::None
            }
            Err(e) => {
                let _ = self.nav.refresh();
                self.err(format!("delete: {e}"))
            }
        }
    }

    fn perform_pending(&mut self) -> Effect {
        self.mode = Mode::Browse;
        let Some(op) = self.pending.take() else {
            return self.err("nothing to confirm".to_string());
        };
        let (src, dst, verb) = match &op {
            PendingOp::Move { src, dst } => (src.clone(), dst.clone(), "moved"),
            PendingOp::Rename { src, dst } => (src.clone(), dst.clone(), "renamed"),
            PendingOp::Trash { src, name } => {
                let (src, name) = (src.clone(), name.clone());
                return self.perform_trash(&src, &name);
            }
        };
        match fsops::safe_move(&src, &dst) {
            Ok(()) => {
                self.push_msg(
                    Level::Ok,
                    format!("{verb} '{}' -> '{}'", src.display(), dst.display()),
                );
                if let Err(e) = self.nav.refresh() {
                    return self.err(e.to_string());
                }
                // Keep the selection on the result when it landed in the
                // current directory.
                if dst.parent() == Some(self.nav.cwd.as_path()) {
                    if let Some(target_name) = dst.file_name() {
                        let target_name = target_name.to_string_lossy().into_owned();
                        let visible = self.nav.visible();
                        if let Some(pos) = visible
                            .iter()
                            .position(|&i| self.nav.entries[i].name == target_name)
                        {
                            self.nav.cursor = pos;
                        }
                    }
                }
                Effect::None
            }
            Err(e) => {
                let _ = self.nav.refresh();
                self.err(e.to_string())
            }
        }
    }

    fn cmd_open(&mut self) -> Effect {
        let (name, path) = match self.selected_operand() {
            Ok(v) => v,
            Err(e) => return self.err(format!("open: {e}")),
        };
        if !cfg!(target_os = "macos") {
            return self.err("open: only supported on macOS (uses /usr/bin/open)".to_string());
        }
        self.push_msg(Level::Ok, format!("open: handing '{name}' to macOS open"));
        Effect::SpawnDetached {
            argv: vec![
                "/usr/bin/open".to_string(),
                "--".to_string(),
                path.to_string_lossy().into_owned(),
            ],
        }
    }

    fn cmd_edit(&mut self) -> Effect {
        let (name, path) = match self.selected_operand() {
            Ok(v) => v,
            Err(e) => return self.err(format!("edit: {e}")),
        };
        let Some(entry) = self.nav.selected() else {
            return self.err("edit: nothing selected".to_string());
        };
        if !entry.is_file_like() {
            return self.err(format!("edit: '{name}' is not a regular file"));
        }
        let argv = editor::build_edit_command(self.editor_env.as_deref(), &path);
        self.push_msg(Level::Ok, format!("edit: opening '{name}' in {}", argv[0]));
        Effect::RunInteractive { argv }
    }

    fn cmd_preview(&mut self) -> Effect {
        let (name, path) = match self.selected_operand() {
            Ok(v) => v,
            Err(e) => return self.err(format!("preview: {e}")),
        };
        let Some(entry) = self.nav.selected() else {
            return self.err("preview: nothing selected".to_string());
        };
        if entry.is_file_like() && self.nvim_on_path {
            match preview::sniff(&path) {
                Ok(sample) if !sample.is_empty() && preview::is_probably_text(&sample) => {
                    let argv = editor::build_preview_command(&path);
                    self.push_msg(
                        Level::Ok,
                        format!("preview: opening '{name}' read-only in nvim"),
                    );
                    return Effect::RunInteractive { argv };
                }
                Ok(_) => {}
                Err(e) => return self.err(format!("preview: {e}")),
            }
        }
        match preview::build_preview(&path) {
            Ok(PreviewData { title, lines }) => {
                self.mode = Mode::Pager(Pager::plain(format!("preview: {title}"), lines));
                Effect::None
            }
            Err(e) => self.err(format!("preview: {e}")),
        }
    }

    fn cmd_agent(&mut self, args: Vec<String>) -> Effect {
        let seam = agent::default_agent();
        let request = AgentRequest {
            args,
            cwd: self.nav.cwd.clone(),
            selection: self
                .nav
                .selected()
                .filter(|e| !e.is_parent)
                .map(|e| self.nav.cwd.join(&e.name)),
        };
        debug_assert!(!seam.is_enabled(), "v0 must never ship an enabled agent");
        let reply = seam.handle(&request);
        self.push_msg(Level::Info, "agent: not configured in v0".to_string());
        self.mode = Mode::Pager(Pager::plain("agent (not configured)", reply.lines));
        Effect::None
    }

    fn show_help(&mut self) -> Effect {
        self.mode = Mode::Pager(Pager::plain("help", help_lines()));
        Effect::None
    }
}

/// The full help text, shared by the `?` key and the `help` command.
pub fn help_lines() -> Vec<String> {
    [
        "FILECRAFT - keyboard-first BBS file navigator",
        "",
        "KEYS (browse)",
        "  j / k, Down / Up     move focus",
        "  PgUp / PgDn          move focus a page",
        "  g / G                first / last entry",
        "  Enter                enter directory, or edit selected file",
        "  l, Right             enter directory, or read the selected file",
        "  h, Left, Backspace   go to parent directory",
        "  0-9                  jump to that ancestor on the ladder",
        "  d                    move selected entry to the Trash (asks y/n)",
        "  /                    filter the listing (Esc clears)",
        "  :                    command prompt",
        "  .                    show/hide dotfiles",
        "  r                    refresh listing",
        "  M                    message history",
        "  ?                    this help",
        "  Esc                  back out one level (clears a filter)",
        "  q, Ctrl-C            quit",
        "",
        "KEYS (reader - l on a text or Markdown file)",
        "  j / k, Down / Up     scroll one line",
        "  d / u                scroll half a page",
        "  f / b, PgDn / PgUp   scroll a page",
        "  g / G, Home / End    top / bottom",
        "  /                    find in this file (Enter searches)",
        "  n / N                next / previous match",
        "  h, q, Esc            back to the listing, on the same row",
        "",
        "KEYS (confirmation prompt)",
        "  y, Enter             go ahead",
        "  n, q, Esc            cancel - nothing is touched",
        "",
        "KEYS (folder picker - :move with no path)",
        "  j / k, Down / Up     move focus",
        "  PgUp / PgDn          move focus a page",
        "  l, Right             enter the focused folder",
        "  h, Left, Backspace   go to parent directory",
        "  g / G                first / last folder",
        "  Enter, m             choose the focused folder (then y/n)",
        "  q, Esc               cancel, back to the listing",
        "",
        "COMMANDS (at the : prompt)",
        "  cd [path]            change directory (~ ok; quote spaces)",
        "  move [destination]   folder picker, or a path (asks y/n first)",
        "  rename <new-name>    rename selected entry (asks y/n first)",
        "  delete, trash        move selected entry to the Trash (asks y/n)",
        "  open                 open selected entry with macOS 'open'",
        "  edit                 edit selected file in $EDITOR (or nvim)",
        "  preview              read-only preview (nvim -R, or built-in)",
        "  agent [...]          future AI seam - disabled in v0",
        "  help                 this help",
        "  quit                 leave filecraft",
        "",
        "SAFETY",
        "  - the reader is read-only: no key in it can change a file",
        "  - moves and renames never overwrite and always ask first",
        "  - delete is a move to the Trash: recoverable, never an unlink",
        "  - nothing is ever removed permanently, recursively or otherwise",
        "  - commands are parsed directly; nothing touches a shell",
        "  - everything stays on this machine: no network, no telemetry",
        "",
        "MARKERS   name/ directory   name@ symlink   name@! broken symlink",
        "",
        "BEARINGS",
        "  - the ladder row is read-only: digits jump, nothing else acts there",
        "  - the rail column shows where the viewport sits in the listing",
        "  - the status row says the same thing in words, for speech",
        "",
        "press h, q, or Esc to close this help",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn app_in(tmp: &tempfile::TempDir) -> App {
        let nav = NavState::new(tmp.path()).unwrap();
        App::new(nav, None, false, None)
    }

    /// An app whose Trash is a directory under a fixture, so `delete`
    /// runs for real without touching the user's `~/.Trash`.
    fn app_with_can(tmp: &tempfile::TempDir, can: &tempfile::TempDir) -> App {
        let mut app = app_in(tmp);
        app.trasher = Box::new(trash::DirTrasher::new(can.path()));
        app
    }

    /// What is in a fixture Trash right now.
    fn can_contents(can: &tempfile::TempDir) -> Vec<String> {
        trash::DirTrasher::new(can.path()).contents()
    }

    fn select(app: &mut App, name: &str) {
        let visible = app.nav.visible();
        let pos = visible
            .iter()
            .position(|&i| app.nav.entries[i].name == name)
            .unwrap_or_else(|| panic!("entry '{name}' not visible"));
        app.nav.cursor = pos;
    }

    /// Every path under `dir` with its size and mtime - the evidence that
    /// a key changed nothing.
    fn snapshot(dir: &std::path::Path) -> Vec<String> {
        let mut out = Vec::new();
        let mut stack = vec![dir.to_path_buf()];
        while let Some(next) = stack.pop() {
            let Ok(read) = fs::read_dir(&next) else {
                continue;
            };
            for entry in read.flatten() {
                let meta = entry.metadata().unwrap();
                if meta.is_dir() {
                    stack.push(entry.path());
                }
                out.push(format!(
                    "{} {} {:?}",
                    entry.path().display(),
                    meta.len(),
                    meta.modified().ok()
                ));
            }
        }
        out.sort();
        out
    }

    fn last_msg(app: &App) -> &Message {
        app.messages.last().expect("expected a message")
    }

    #[test]
    fn quit_keys() {
        let tmp = tempfile::tempdir().unwrap();
        let mut app = app_in(&tmp);
        assert_eq!(app.handle_key(KeyInput::Char('q')), Effect::Quit);
        assert_eq!(app.handle_key(KeyInput::CtrlC), Effect::Quit);
        assert_eq!(app.execute_line("quit"), Effect::Quit);
    }

    #[test]
    fn esc_backs_out_one_level_and_never_quits() {
        let tmp = tempfile::tempdir().unwrap();
        fs::write(tmp.path().join("apple.md"), "a").unwrap();
        fs::write(tmp.path().join("banana.md"), "b").unwrap();
        let mut app = app_in(&tmp);

        // With nothing to back out of, Esc does nothing at all.
        assert_eq!(app.handle_key(KeyInput::Esc), Effect::None);
        assert_eq!(app.mode, Mode::Browse);

        // With a filter kept from filter mode, Esc clears exactly that.
        app.handle_key(KeyInput::Char('/'));
        for c in "app".chars() {
            app.handle_key(KeyInput::Char(c));
        }
        app.handle_key(KeyInput::Enter);
        assert_eq!(app.nav.filter, "app");
        assert_eq!(app.handle_key(KeyInput::Esc), Effect::None);
        assert!(app.nav.filter.is_empty());
        assert!(last_msg(&app).text.contains("filter cleared"));

        // And it is still not a quit key.
        assert_eq!(app.handle_key(KeyInput::Esc), Effect::None);
    }

    #[test]
    fn ctrl_c_quits_from_any_mode() {
        let tmp = tempfile::tempdir().unwrap();
        let mut app = app_in(&tmp);
        app.mode = Mode::Command {
            input: "typing".into(),
        };
        assert_eq!(app.handle_key(KeyInput::CtrlC), Effect::Quit);
    }

    #[test]
    fn move_flow_requires_confirmation() {
        let tmp = tempfile::tempdir().unwrap();
        fs::write(tmp.path().join("note.txt"), "n").unwrap();
        fs::create_dir(tmp.path().join("archive")).unwrap();
        let mut app = app_in(&tmp);
        select(&mut app, "note.txt");

        app.execute_line("move archive");
        assert_eq!(app.mode, Mode::ConfirmOp);
        let pending = app.pending.clone().unwrap();
        assert!(pending.describe().contains("note.txt"));
        assert!(pending.describe().contains("archive"));
        // Not moved yet.
        assert!(tmp.path().join("note.txt").exists());

        app.handle_key(KeyInput::Char('y'));
        assert!(!tmp.path().join("note.txt").exists());
        assert!(tmp.path().join("archive/note.txt").exists());
        assert_eq!(last_msg(&app).level, Level::Ok);
        assert_eq!(app.mode, Mode::Browse);
        assert!(app.pending.is_none());
    }

    #[test]
    fn move_flow_cancel_leaves_file() {
        let tmp = tempfile::tempdir().unwrap();
        fs::write(tmp.path().join("note.txt"), "n").unwrap();
        fs::create_dir(tmp.path().join("archive")).unwrap();
        let mut app = app_in(&tmp);
        select(&mut app, "note.txt");

        app.execute_line("move archive");
        app.handle_key(KeyInput::Char('n'));
        assert!(tmp.path().join("note.txt").exists());
        assert!(!tmp.path().join("archive/note.txt").exists());
        assert!(last_msg(&app).text.contains("cancelled"));
        assert!(app.pending.is_none());
    }

    fn picker(app: &App) -> &FolderPicker {
        match &app.mode {
            Mode::FolderPicker(p) => p,
            other => panic!("expected folder picker, got {other:?}"),
        }
    }

    #[test]
    fn move_without_destination_opens_picker() {
        let tmp = tempfile::tempdir().unwrap();
        fs::write(tmp.path().join("note.txt"), "n").unwrap();
        fs::create_dir(tmp.path().join("archive")).unwrap();
        let mut app = app_in(&tmp);
        select(&mut app, "note.txt");
        let cwd = app.nav.cwd.clone();
        let row = app.nav.cursor;

        app.execute_line("move");
        assert!(matches!(app.mode, Mode::FolderPicker(_)));
        assert!(picker(&app).destination() == cwd);
        assert!(picker(&app).entries.iter().any(|e| e.name == "archive"));
        assert!(tmp.path().join("note.txt").exists());
        assert_eq!(app.nav.cwd, cwd);
        assert_eq!(app.nav.cursor, row);
        assert!(app.pending.is_none());
    }

    #[test]
    fn picker_j_k_move_focus_and_header_tracks_destination() {
        let tmp = tempfile::tempdir().unwrap();
        fs::write(tmp.path().join("note.txt"), "n").unwrap();
        fs::create_dir(tmp.path().join("archive")).unwrap();
        fs::create_dir(tmp.path().join("docs")).unwrap();
        let mut app = app_in(&tmp);
        select(&mut app, "note.txt");
        app.execute_line("move");

        let start = picker(&app).cursor;
        app.handle_key(KeyInput::Char('j'));
        assert_eq!(picker(&app).cursor, start + 1);
        app.handle_key(KeyInput::Char('k'));
        assert_eq!(picker(&app).cursor, start);
        app.handle_key(KeyInput::Char('G'));
        assert_eq!(picker(&app).cursor, picker(&app).entries.len() - 1);
        app.handle_key(KeyInput::Char('g'));
        assert_eq!(picker(&app).cursor, 0);
        assert!(picker(&app).dest_line().starts_with("dest: "));
    }

    #[test]
    fn picker_l_descends_and_h_returns_without_changing_the_listing() {
        let tmp = tempfile::tempdir().unwrap();
        fs::write(tmp.path().join("note.txt"), "n").unwrap();
        fs::create_dir_all(tmp.path().join("archive/nested")).unwrap();
        let mut app = app_in(&tmp);
        select(&mut app, "note.txt");
        let listing_cwd = app.nav.cwd.clone();
        app.execute_line("move");

        let archive = picker(&app)
            .entries
            .iter()
            .position(|e| e.name == "archive")
            .unwrap();
        {
            let Mode::FolderPicker(p) = &mut app.mode else {
                panic!();
            };
            p.cursor = archive;
        }
        app.handle_key(KeyInput::Char('l'));
        assert!(picker(&app).cwd.ends_with("archive"));
        assert!(picker(&app).entries.iter().any(|e| e.name == "nested"));
        assert_eq!(app.nav.cwd, listing_cwd, "listing cwd must not follow");

        app.handle_key(KeyInput::Char('h'));
        assert_eq!(picker(&app).cwd, listing_cwd);
        assert_eq!(picker(&app).focused().unwrap().name, "archive");
    }

    #[test]
    fn picker_enter_selects_focused_folder_and_asks_to_confirm() {
        let tmp = tempfile::tempdir().unwrap();
        fs::write(tmp.path().join("note.txt"), "n").unwrap();
        fs::create_dir(tmp.path().join("archive")).unwrap();
        let mut app = app_in(&tmp);
        select(&mut app, "note.txt");
        app.execute_line("move");

        let archive = picker(&app)
            .entries
            .iter()
            .position(|e| e.name == "archive")
            .unwrap();
        {
            let Mode::FolderPicker(p) = &mut app.mode else {
                panic!();
            };
            p.cursor = archive;
        }
        app.handle_key(KeyInput::Enter);
        assert_eq!(app.mode, Mode::ConfirmOp);
        let pending = app.pending.clone().unwrap();
        assert!(pending.describe().contains("note.txt"));
        assert!(pending.describe().contains("archive"));
        assert!(tmp.path().join("note.txt").exists());

        app.handle_key(KeyInput::Char('y'));
        assert!(!tmp.path().join("note.txt").exists());
        assert!(tmp.path().join("archive/note.txt").exists());
        assert_eq!(app.mode, Mode::Browse);
    }

    #[test]
    fn picker_m_also_selects_the_focused_folder() {
        let tmp = tempfile::tempdir().unwrap();
        fs::write(tmp.path().join("note.txt"), "n").unwrap();
        fs::create_dir(tmp.path().join("docs")).unwrap();
        let mut app = app_in(&tmp);
        select(&mut app, "note.txt");
        app.execute_line("move");
        let docs = picker(&app)
            .entries
            .iter()
            .position(|e| e.name == "docs")
            .unwrap();
        {
            let Mode::FolderPicker(p) = &mut app.mode else {
                panic!();
            };
            p.cursor = docs;
        }
        app.handle_key(KeyInput::Char('m'));
        assert_eq!(app.mode, Mode::ConfirmOp);
        assert!(app.pending.as_ref().unwrap().describe().contains("docs"));
        assert!(tmp.path().join("note.txt").exists());
    }

    #[test]
    fn picker_esc_and_q_cancel_without_moving() {
        let tmp = tempfile::tempdir().unwrap();
        fs::write(tmp.path().join("note.txt"), "n").unwrap();
        fs::create_dir(tmp.path().join("archive")).unwrap();
        for key in [KeyInput::Esc, KeyInput::Char('q')] {
            let mut app = app_in(&tmp);
            select(&mut app, "note.txt");
            let row = app.nav.cursor;
            app.execute_line("move");
            assert!(matches!(app.mode, Mode::FolderPicker(_)));
            assert_eq!(app.handle_key(key), Effect::None);
            assert_eq!(app.mode, Mode::Browse, "{key:?}");
            assert!(app.pending.is_none(), "{key:?}");
            assert!(tmp.path().join("note.txt").exists(), "{key:?}");
            assert!(!tmp.path().join("archive/note.txt").exists(), "{key:?}");
            assert_eq!(app.nav.cursor, row, "{key:?}");
            assert!(last_msg(&app).text.contains("cancelled"), "{key:?}");
        }
    }

    /// Every picker key the help and the README advertise besides the
    /// letters: the arrows, Backspace, and paging. They are the same
    /// motions as `j`/`k`/`l`/`h`/`g`/`G`, and none of them can move a
    /// file - only Enter/`m` reaches the confirmation.
    #[test]
    fn picker_arrow_backspace_and_paging_keys_match_the_letter_keys() {
        let tmp = tempfile::tempdir().unwrap();
        fs::write(tmp.path().join("note.txt"), "n").unwrap();
        for i in 0..12 {
            fs::create_dir(tmp.path().join(format!("dir_{i:02}"))).unwrap();
        }
        fs::create_dir(tmp.path().join("dir_00/nested")).unwrap();
        let before = snapshot(tmp.path());
        let mut app = app_in(&tmp);
        app.set_viewport(8, 80);
        select(&mut app, "note.txt");
        app.execute_line("move");

        // Down / Up are j / k.
        let start = picker(&app).cursor;
        app.handle_key(KeyInput::Down);
        assert_eq!(picker(&app).cursor, start + 1);
        app.handle_key(KeyInput::Up);
        assert_eq!(picker(&app).cursor, start);

        // PgDn / PgUp move by exactly the rows the popup draws.
        let rows = app.picker_rows();
        app.handle_key(KeyInput::PageDown);
        assert_eq!(picker(&app).cursor, start + rows);
        app.handle_key(KeyInput::PageUp);
        assert_eq!(picker(&app).cursor, start);

        // Right descends where `l` does; Left and Backspace both come back.
        let listing_cwd = app.nav.cwd.clone();
        let dir_00 = picker(&app)
            .entries
            .iter()
            .position(|e| e.name == "dir_00")
            .unwrap();
        for back in [KeyInput::Left, KeyInput::Backspace] {
            let Mode::FolderPicker(p) = &mut app.mode else {
                panic!("expected folder picker");
            };
            p.cursor = dir_00;
            app.handle_key(KeyInput::Right);
            assert!(picker(&app).cwd.ends_with("dir_00"), "{back:?}");
            assert!(
                picker(&app).entries.iter().any(|e| e.name == "nested"),
                "{back:?}"
            );
            assert_eq!(app.nav.cwd, listing_cwd, "{back:?}");
            app.handle_key(back);
            assert_eq!(picker(&app).cwd, listing_cwd, "{back:?}");
            assert_eq!(picker(&app).focused().unwrap().name, "dir_00", "{back:?}");
        }

        // End / Home are G / g.
        app.handle_key(KeyInput::End);
        assert_eq!(picker(&app).cursor, picker(&app).entries.len() - 1);
        app.handle_key(KeyInput::Home);
        assert_eq!(picker(&app).cursor, 0);

        assert!(matches!(app.mode, Mode::FolderPicker(_)));
        assert!(app.pending.is_none());
        assert_eq!(snapshot(tmp.path()), before, "a picker motion key mutated");
    }

    #[test]
    fn picker_selecting_current_dir_is_the_same_path_error() {
        let tmp = tempfile::tempdir().unwrap();
        fs::write(tmp.path().join("note.txt"), "n").unwrap();
        let mut app = app_in(&tmp);
        select(&mut app, "note.txt");
        app.execute_line("move");
        app.handle_key(KeyInput::Enter);
        assert!(matches!(app.mode, Mode::FolderPicker(_)));
        assert!(app.pending.is_none());
        assert_eq!(last_msg(&app).level, Level::Error);
        assert!(last_msg(&app).text.contains("same"));
        assert!(tmp.path().join("note.txt").exists());
    }

    #[test]
    fn picker_does_not_overwrite_or_move_without_y() {
        let tmp = tempfile::tempdir().unwrap();
        fs::write(tmp.path().join("note.txt"), "n").unwrap();
        fs::create_dir(tmp.path().join("archive")).unwrap();
        fs::write(tmp.path().join("archive/note.txt"), "keep").unwrap();
        let mut app = app_in(&tmp);
        select(&mut app, "note.txt");
        app.execute_line("move");
        let archive = picker(&app)
            .entries
            .iter()
            .position(|e| e.name == "archive")
            .unwrap();
        {
            let Mode::FolderPicker(p) = &mut app.mode else {
                panic!();
            };
            p.cursor = archive;
        }
        app.handle_key(KeyInput::Enter);
        assert!(matches!(app.mode, Mode::FolderPicker(_)));
        assert!(app.pending.is_none());
        assert!(last_msg(&app).text.contains("already exists"));
        assert_eq!(
            fs::read_to_string(tmp.path().join("note.txt")).unwrap(),
            "n"
        );
        assert_eq!(
            fs::read_to_string(tmp.path().join("archive/note.txt")).unwrap(),
            "keep"
        );
    }

    #[test]
    fn delete_asks_before_it_trashes_and_y_moves_the_entry() {
        let tmp = tempfile::tempdir().unwrap();
        let can = tempfile::tempdir().unwrap();
        fs::write(tmp.path().join("notes.md"), "keep me").unwrap();
        let mut app = app_with_can(&tmp, &can);
        select(&mut app, "notes.md");

        app.execute_line("delete");
        assert_eq!(app.mode, Mode::ConfirmOp);
        assert!(tmp.path().join("notes.md").exists(), "armed is not done");
        assert_eq!(
            app.pending.as_ref().unwrap().describe(),
            "trash 'notes.md'",
            "the prompt names the entry the listing names"
        );

        app.handle_key(KeyInput::Char('y'));
        assert_eq!(app.mode, Mode::Browse);
        assert!(app.pending.is_none());
        assert!(!tmp.path().join("notes.md").exists(), "entry must be gone");
        assert_eq!(can_contents(&can), vec!["notes.md".to_string()]);
        assert_eq!(
            fs::read_to_string(can.path().join("notes.md")).unwrap(),
            "keep me",
            "trashing is a move: the bytes must survive"
        );
        assert_eq!(last_msg(&app).level, Level::Ok);
        assert!(
            last_msg(&app).text.contains("recoverable"),
            "{:?}",
            last_msg(&app)
        );
    }

    #[test]
    fn trash_is_the_same_command_under_another_name() {
        let tmp = tempfile::tempdir().unwrap();
        let can = tempfile::tempdir().unwrap();
        fs::write(tmp.path().join("a.txt"), "a").unwrap();
        let mut app = app_with_can(&tmp, &can);
        select(&mut app, "a.txt");

        app.execute_line("trash");
        assert_eq!(app.pending.as_ref().unwrap().describe(), "trash 'a.txt'");
    }

    #[test]
    fn d_arms_the_same_prompt_the_command_does() {
        let tmp = tempfile::tempdir().unwrap();
        let can = tempfile::tempdir().unwrap();
        fs::write(tmp.path().join("a.txt"), "a").unwrap();
        let mut app = app_with_can(&tmp, &can);
        select(&mut app, "a.txt");

        app.handle_key(KeyInput::Char('d'));
        assert_eq!(app.mode, Mode::ConfirmOp);
        assert_eq!(app.pending.as_ref().unwrap().describe(), "trash 'a.txt'");
        assert!(
            tmp.path().join("a.txt").exists(),
            "d alone must touch nothing"
        );
        assert!(can_contents(&can).is_empty());
    }

    #[test]
    fn every_cancel_key_leaves_the_entry_exactly_where_it_was() {
        for key in [
            KeyInput::Char('n'),
            KeyInput::Char('N'),
            KeyInput::Char('q'),
            KeyInput::Esc,
        ] {
            let tmp = tempfile::tempdir().unwrap();
            let can = tempfile::tempdir().unwrap();
            fs::write(tmp.path().join("a.txt"), "a").unwrap();
            let before = snapshot(tmp.path());
            let mut app = app_with_can(&tmp, &can);
            select(&mut app, "a.txt");

            app.handle_key(KeyInput::Char('d'));
            assert_eq!(app.mode, Mode::ConfirmOp, "{key:?}");
            app.handle_key(key);

            assert_eq!(app.mode, Mode::Browse, "{key:?} left the prompt up");
            assert!(app.pending.is_none(), "{key:?}");
            assert_eq!(snapshot(tmp.path()), before, "{key:?} changed the tree");
            assert!(can_contents(&can).is_empty(), "{key:?} trashed the entry");
            assert!(last_msg(&app).text.starts_with("cancelled:"), "{key:?}");
        }
    }

    #[test]
    fn an_unrelated_key_at_the_delete_prompt_neither_trashes_nor_cancels() {
        let tmp = tempfile::tempdir().unwrap();
        let can = tempfile::tempdir().unwrap();
        fs::write(tmp.path().join("a.txt"), "a").unwrap();
        let mut app = app_with_can(&tmp, &can);
        select(&mut app, "a.txt");
        app.handle_key(KeyInput::Char('d'));

        app.handle_key(KeyInput::Char('x'));
        assert_eq!(app.mode, Mode::ConfirmOp);
        assert!(app.pending.is_some());
        assert!(tmp.path().join("a.txt").exists());
        assert!(can_contents(&can).is_empty());
    }

    #[test]
    fn delete_refuses_the_parent_row_in_words() {
        let tmp = tempfile::tempdir().unwrap();
        let can = tempfile::tempdir().unwrap();
        fs::create_dir(tmp.path().join("sub")).unwrap();
        fs::write(tmp.path().join("sub/inner.txt"), "inner").unwrap();
        let mut app = app_with_can(&tmp, &can);
        let sub = app.nav.cwd.join("sub");
        app.nav.change_dir(sub, None).unwrap();
        let before = snapshot(tmp.path());

        for open_it in ["delete", "trash"] {
            select(&mut app, "..");
            app.execute_line(open_it);
            assert_eq!(last_msg(&app).level, Level::Error, "{open_it}");
            assert!(last_msg(&app).text.contains("'..'"), "{:?}", last_msg(&app));
            assert_eq!(app.mode, Mode::Browse, "{open_it} armed a prompt on '..'");
            assert!(app.pending.is_none(), "{open_it}");
        }

        // And the key, which never reaches the command parser.
        select(&mut app, "..");
        app.handle_key(KeyInput::Char('d'));
        assert_eq!(last_msg(&app).level, Level::Error);
        assert!(app.pending.is_none());

        assert_eq!(snapshot(tmp.path()), before);
        assert!(can_contents(&can).is_empty());
    }

    #[test]
    fn delete_trashes_a_directory_whole() {
        let tmp = tempfile::tempdir().unwrap();
        let can = tempfile::tempdir().unwrap();
        fs::create_dir(tmp.path().join("sub")).unwrap();
        fs::write(tmp.path().join("sub/inner.txt"), "inner").unwrap();
        let mut app = app_with_can(&tmp, &can);
        select(&mut app, "sub");

        app.handle_key(KeyInput::Char('d'));
        app.handle_key(KeyInput::Char('y'));

        assert!(!tmp.path().join("sub").exists());
        assert_eq!(
            fs::read_to_string(can.path().join("sub/inner.txt")).unwrap(),
            "inner",
            "a trashed directory keeps its contents - this is a move, not a walk"
        );
    }

    #[test]
    fn delete_reports_an_entry_that_vanished_before_confirmation() {
        let tmp = tempfile::tempdir().unwrap();
        let can = tempfile::tempdir().unwrap();
        fs::write(tmp.path().join("a.txt"), "a").unwrap();
        let mut app = app_with_can(&tmp, &can);
        select(&mut app, "a.txt");
        app.handle_key(KeyInput::Char('d'));

        // Something else removed it while the prompt was up.
        fs::rename(tmp.path().join("a.txt"), can.path().join("a.txt")).unwrap();
        app.handle_key(KeyInput::Char('y'));

        assert_eq!(app.mode, Mode::Browse);
        assert_eq!(last_msg(&app).level, Level::Error);
        assert!(
            last_msg(&app).text.starts_with("delete: "),
            "{:?}",
            last_msg(&app)
        );
    }

    #[test]
    fn delete_on_an_empty_listing_says_so_instead_of_panicking() {
        let tmp = tempfile::tempdir().unwrap();
        let can = tempfile::tempdir().unwrap();
        let mut app = app_with_can(&tmp, &can);

        app.handle_key(KeyInput::Char('d'));
        assert_eq!(app.mode, Mode::Browse);
        assert_eq!(last_msg(&app).level, Level::Error);
        assert!(app.pending.is_none());
    }

    #[test]
    fn the_selection_lands_on_a_real_row_after_the_last_entry_is_trashed() {
        let tmp = tempfile::tempdir().unwrap();
        let can = tempfile::tempdir().unwrap();
        fs::write(tmp.path().join("a.txt"), "a").unwrap();
        fs::write(tmp.path().join("b.txt"), "b").unwrap();
        let mut app = app_with_can(&tmp, &can);
        select(&mut app, "b.txt");

        app.handle_key(KeyInput::Char('d'));
        app.handle_key(KeyInput::Char('y'));

        let selected = app.nav.selected().expect("a row must still be focused");
        assert_eq!(selected.name, "a.txt");
    }

    #[test]
    fn the_help_documents_delete_everywhere_it_is_bound() {
        let help = help_lines().join("\n");
        assert!(help.contains("  d  "), "the browse key is missing:\n{help}");
        assert!(help.contains("delete, trash"), "the commands are missing");
        assert!(
            help.contains("Trash"),
            "the help must say where a deleted entry goes"
        );
        assert!(
            help.contains("n, q, Esc"),
            "the confirmation's cancel keys are missing"
        );
    }

    #[test]
    fn confirm_ignores_other_keys() {
        let tmp = tempfile::tempdir().unwrap();
        fs::write(tmp.path().join("a.txt"), "a").unwrap();
        let mut app = app_in(&tmp);
        select(&mut app, "a.txt");
        app.execute_line("rename b.txt");
        assert_eq!(app.mode, Mode::ConfirmOp);
        app.handle_key(KeyInput::Char('x'));
        assert_eq!(app.mode, Mode::ConfirmOp);
        assert!(app.pending.is_some());
        assert!(tmp.path().join("a.txt").exists());
    }

    #[test]
    fn escape_cancels_confirmation() {
        let tmp = tempfile::tempdir().unwrap();
        fs::write(tmp.path().join("a.txt"), "a").unwrap();
        let mut app = app_in(&tmp);
        select(&mut app, "a.txt");
        app.execute_line("rename b.txt");
        app.handle_key(KeyInput::Esc);
        assert_eq!(app.mode, Mode::Browse);
        assert!(app.pending.is_none());
        assert!(tmp.path().join("a.txt").exists());
    }

    #[test]
    fn rename_flow_confirmed() {
        let tmp = tempfile::tempdir().unwrap();
        fs::write(tmp.path().join("old name.txt"), "x").unwrap();
        let mut app = app_in(&tmp);
        select(&mut app, "old name.txt");

        app.execute_line(r#"rename "new nämé 檔.txt""#);
        assert_eq!(app.mode, Mode::ConfirmOp);
        app.handle_key(KeyInput::Enter);
        assert!(tmp.path().join("new nämé 檔.txt").exists());
        assert!(!tmp.path().join("old name.txt").exists());
        // Selection follows the renamed entry.
        assert_eq!(app.nav.selected().unwrap().name, "new nämé 檔.txt");
    }

    #[test]
    fn rename_rejects_paths_and_dots() {
        let tmp = tempfile::tempdir().unwrap();
        fs::write(tmp.path().join("a.txt"), "a").unwrap();
        let mut app = app_in(&tmp);
        select(&mut app, "a.txt");

        for bad in ["../up", "x/y", "..", "."] {
            app.execute_line(&format!("rename \"{bad}\""));
            assert_eq!(app.mode, Mode::Browse, "rename '{bad}' must not confirm");
            assert_eq!(last_msg(&app).level, Level::Error);
            assert!(app.pending.is_none());
        }
    }

    #[test]
    fn move_refuses_overwrite_before_confirmation() {
        let tmp = tempfile::tempdir().unwrap();
        fs::write(tmp.path().join("a.txt"), "a").unwrap();
        fs::write(tmp.path().join("b.txt"), "b").unwrap();
        let mut app = app_in(&tmp);
        select(&mut app, "a.txt");

        app.execute_line("move b.txt");
        assert_eq!(app.mode, Mode::Browse);
        assert!(app.pending.is_none());
        assert!(last_msg(&app).text.contains("already exists"));
        assert_eq!(fs::read_to_string(tmp.path().join("b.txt")).unwrap(), "b");
    }

    #[test]
    fn move_refuses_directory_into_itself() {
        let tmp = tempfile::tempdir().unwrap();
        fs::create_dir_all(tmp.path().join("outer")).unwrap();
        let mut app = app_in(&tmp);
        select(&mut app, "outer");

        app.execute_line("move outer");
        assert_eq!(app.mode, Mode::Browse);
        assert!(app.pending.is_none());
        assert!(last_msg(&app).text.contains("into itself"));
    }

    #[test]
    fn move_missing_destination_parent_is_an_error() {
        let tmp = tempfile::tempdir().unwrap();
        fs::write(tmp.path().join("a.txt"), "a").unwrap();
        let mut app = app_in(&tmp);
        select(&mut app, "a.txt");
        app.execute_line("move no/such/place/a.txt");
        assert_eq!(last_msg(&app).level, Level::Error);
        assert!(app.pending.is_none());
    }

    #[test]
    fn operations_refuse_parent_row() {
        let tmp = tempfile::tempdir().unwrap();
        fs::create_dir(tmp.path().join("sub")).unwrap();
        let mut app = app_in(&tmp);
        let sub = app.nav.cwd.join("sub");
        app.nav.change_dir(sub, None).unwrap();
        select(&mut app, "..");

        for line in [
            "move", "move x", "rename x", "edit", "preview", "open", "delete", "trash",
        ] {
            app.execute_line(line);
            assert_eq!(last_msg(&app).level, Level::Error, "'{line}' on '..'");
            assert!(last_msg(&app).text.contains("'..'"));
        }
    }

    #[test]
    fn edit_builds_editor_effect_for_files_only() {
        let tmp = tempfile::tempdir().unwrap();
        fs::write(tmp.path().join("doc.md"), "d").unwrap();
        fs::create_dir(tmp.path().join("dir")).unwrap();
        let nav = NavState::new(tmp.path()).unwrap();
        let mut app = App::new(nav, Some("myedit --fast".to_string()), false, None);

        select(&mut app, "doc.md");
        let effect = app.execute_line("edit");
        let Effect::RunInteractive { argv } = effect else {
            panic!("expected RunInteractive, got {effect:?}");
        };
        assert_eq!(argv[0], "myedit");
        assert_eq!(argv[1], "--fast");
        assert!(argv.last().unwrap().ends_with("doc.md"));

        select(&mut app, "dir");
        assert_eq!(app.execute_line("edit"), Effect::None);
        assert!(last_msg(&app).text.contains("not a regular file"));
    }

    #[test]
    fn enter_on_file_edits_it() {
        let tmp = tempfile::tempdir().unwrap();
        fs::write(tmp.path().join("doc.md"), "d").unwrap();
        let mut app = app_in(&tmp);
        select(&mut app, "doc.md");
        let effect = app.handle_key(KeyInput::Enter);
        let Effect::RunInteractive { argv } = effect else {
            panic!("expected editor effect, got {effect:?}");
        };
        assert_eq!(argv[0], "nvim");
    }

    #[test]
    fn enter_on_directory_enters_it() {
        let tmp = tempfile::tempdir().unwrap();
        fs::create_dir(tmp.path().join("sub")).unwrap();
        let mut app = app_in(&tmp);
        select(&mut app, "sub");
        assert_eq!(app.handle_key(KeyInput::Enter), Effect::None);
        assert!(app.nav.cwd.ends_with("sub"));
    }

    #[test]
    fn up_keys_go_to_parent() {
        let tmp = tempfile::tempdir().unwrap();
        fs::create_dir(tmp.path().join("sub")).unwrap();
        for key in [KeyInput::Backspace, KeyInput::Char('h'), KeyInput::Left] {
            let mut app = app_in(&tmp);
            let sub = app.nav.cwd.join("sub");
            app.nav.change_dir(sub, None).unwrap();
            app.handle_key(key);
            assert_eq!(
                app.nav.cwd,
                tmp.path().canonicalize().unwrap(),
                "{key:?} should go up"
            );
        }
    }

    #[test]
    fn l_and_right_descend_into_the_selected_directory() {
        let tmp = tempfile::tempdir().unwrap();
        fs::create_dir(tmp.path().join("sub")).unwrap();
        fs::write(tmp.path().join("note.txt"), "n").unwrap();
        for key in [KeyInput::Char('l'), KeyInput::Right] {
            let mut app = app_in(&tmp);
            select(&mut app, "sub");
            assert_eq!(app.handle_key(key), Effect::None);
            assert!(app.nav.cwd.ends_with("sub"), "{key:?} should descend");

            // On a file it opens the read-only reader: same key, same
            // direction, never an editor and never a change on disk.
            let mut app = app_in(&tmp);
            select(&mut app, "note.txt");
            assert_eq!(app.handle_key(key), Effect::None);
            assert_eq!(app.nav.cwd, tmp.path().canonicalize().unwrap());
            let Mode::Pager(pager) = &app.mode else {
                panic!("{key:?} should open the reader");
            };
            assert_eq!(pager.title, "note.txt");
            assert_eq!(pager.text(), "n");
            assert!(tmp.path().join("note.txt").exists());
        }
    }

    /// The reader as the event loop drives it: the app is told the same
    /// geometry the screen has, so scrolling and drawing agree.
    fn reader(app: &App) -> &Pager {
        let Mode::Pager(pager) = &app.mode else {
            panic!("expected the reader, got {:?}", app.mode);
        };
        pager
    }

    #[test]
    fn l_reads_a_markdown_file_with_its_structure_drawn() {
        let tmp = tempfile::tempdir().unwrap();
        fs::write(
            tmp.path().join("notes.md"),
            "# Title\n\n- one\n\n> quoted\n\n```sh\ncargo test\n```\n",
        )
        .unwrap();
        let mut app = app_in(&tmp);
        select(&mut app, "notes.md");
        assert_eq!(app.handle_key(KeyInput::Char('l')), Effect::None);

        let pager = reader(&app);
        assert_eq!(pager.title, "notes.md");
        let kinds: Vec<markdown::Kind> = pager.doc().iter().map(|l| l.kind).collect();
        assert!(kinds.contains(&markdown::Kind::Heading(1)));
        assert!(kinds.contains(&markdown::Kind::Bullet));
        assert!(kinds.contains(&markdown::Kind::Quote));
        assert!(kinds.contains(&markdown::Kind::Code));
        // The code block is shown as it is, never re-read as Markdown.
        assert!(pager.text().contains("cargo test"));
    }

    #[test]
    fn l_reads_a_plain_text_file_without_inventing_markdown() {
        let tmp = tempfile::tempdir().unwrap();
        fs::write(
            tmp.path().join("plain.txt"),
            "# not a heading\n- not a bullet\n",
        )
        .unwrap();
        let mut app = app_in(&tmp);
        select(&mut app, "plain.txt");
        app.handle_key(KeyInput::Char('l'));
        let pager = reader(&app);
        assert!(pager.doc().iter().all(|l| l.kind == markdown::Kind::Body));
        assert_eq!(pager.text(), "# not a heading\n- not a bullet");
    }

    #[test]
    fn l_refuses_a_binary_file_in_words() {
        let tmp = tempfile::tempdir().unwrap();
        fs::write(tmp.path().join("blob.bin"), [0u8, 159, 146, 150]).unwrap();
        let mut app = app_in(&tmp);
        select(&mut app, "blob.bin");
        assert_eq!(app.handle_key(KeyInput::Char('l')), Effect::None);
        assert_eq!(app.mode, Mode::Browse);
        assert_eq!(last_msg(&app).level, Level::Error);
        assert!(last_msg(&app).text.contains("binary"));
    }

    #[test]
    fn l_reads_an_empty_file_without_an_empty_screen() {
        let tmp = tempfile::tempdir().unwrap();
        fs::write(tmp.path().join("empty.md"), "").unwrap();
        let mut app = app_in(&tmp);
        select(&mut app, "empty.md");
        app.handle_key(KeyInput::Char('l'));
        assert_eq!(reader(&app).text(), "(empty file)");
    }

    #[cfg(unix)]
    #[test]
    fn l_on_a_broken_symlink_says_so_instead_of_opening_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink("/nonexistent/target", tmp.path().join("dangling")).unwrap();
        let mut app = app_in(&tmp);
        select(&mut app, "dangling");
        assert_eq!(app.handle_key(KeyInput::Char('l')), Effect::None);
        assert_eq!(app.mode, Mode::Browse);
        assert!(last_msg(&app).text.contains("broken symlink"));
    }

    #[cfg(unix)]
    #[test]
    fn l_on_a_special_file_says_so_instead_of_opening_nothing() {
        let mut app = App::new(
            NavState::new(std::path::Path::new("/dev")).unwrap(),
            None,
            false,
            None,
        );
        let Some(pos) = app
            .nav
            .visible()
            .iter()
            .position(|&i| app.nav.entries[i].kind == crate::nav::EntryKind::Other)
        else {
            return; // no special file to point at; nothing to assert
        };
        app.nav.cursor = pos;
        assert_eq!(app.handle_key(KeyInput::Char('l')), Effect::None);
        assert_eq!(app.mode, Mode::Browse);
        assert!(last_msg(&app).text.contains("special file"));
    }

    #[test]
    fn reader_scrolling_is_bounded_at_both_ends() {
        let tmp = tempfile::tempdir().unwrap();
        let body: String = (1..=100).map(|i| format!("line {i}\n")).collect();
        fs::write(tmp.path().join("long.txt"), body).unwrap();
        let mut app = app_in(&tmp);
        app.viewport_rows = 12; // 10 rows of text inside the reader frame
        app.viewport_cols = 40;
        select(&mut app, "long.txt");
        app.handle_key(KeyInput::Char('l'));

        for _ in 0..500 {
            app.handle_key(KeyInput::Char('j'));
        }
        let last_page = Pager::max_scroll(100, app.pager_rows());
        assert_eq!(reader(&app).scroll, last_page);

        for _ in 0..500 {
            app.handle_key(KeyInput::Char('k'));
        }
        assert_eq!(reader(&app).scroll, 0);

        // Half and full pages, and the two ends.
        app.handle_key(KeyInput::Char('d'));
        assert_eq!(reader(&app).scroll, app.pager_rows() / 2);
        app.handle_key(KeyInput::Char('u'));
        assert_eq!(reader(&app).scroll, 0);
        app.handle_key(KeyInput::PageDown);
        assert_eq!(reader(&app).scroll, app.pager_rows());
        app.handle_key(KeyInput::PageUp);
        assert_eq!(reader(&app).scroll, 0);
        app.handle_key(KeyInput::Char('G'));
        assert_eq!(reader(&app).scroll, last_page);
        app.handle_key(KeyInput::Char('g'));
        assert_eq!(reader(&app).scroll, 0);
    }

    #[test]
    fn widening_the_terminal_reclamps_the_reader_and_keeps_the_position_honest() {
        // Lines that wrap at 40 columns fit on one row at 200, so the
        // bottom of the document moves up under the offset.
        let tmp = tempfile::tempdir().unwrap();
        let body: String = (1..=100)
            .map(|i| format!("line {i} with enough words to wrap more than once here\n"))
            .collect();
        fs::write(tmp.path().join("wrap.txt"), body).unwrap();
        let mut app = app_in(&tmp);
        app.set_viewport(12, 40);
        select(&mut app, "wrap.txt");
        app.handle_key(KeyInput::Char('l'));
        app.handle_key(KeyInput::Char('G'));
        let narrow_scroll = reader(&app).scroll;
        assert!(narrow_scroll > 100);

        app.set_viewport(12, 200);
        let (width, view, glyphs) = (app.pager_cols(), app.pager_rows(), app.glyphs);
        let pager = reader(&app);
        assert_eq!(pager.scroll, Pager::max_scroll(100, view));
        // The footer reports the last page, not line 1 of the file.
        assert_eq!(pager.top_line(width, &glyphs), 100 - view);
        assert!(pager.position(width, view, &glyphs).ends_with("100%"));

        // A search resumes from the visible position: "line 9" also
        // matches source line 9, and a stale offset would have landed
        // there instead of on the first match below the top of the view.
        app.handle_key(KeyInput::Char('/'));
        for c in "line 9".chars() {
            app.handle_key(KeyInput::Char(c));
        }
        app.handle_key(KeyInput::Enter);
        assert_eq!(reader(&app).top_line(width, &glyphs), 100 - view);
    }

    #[test]
    fn the_help_documents_every_reader_key_that_is_bound() {
        let help = help_lines().join("\n");
        for key in ["j / k", "d / u", "f / b", "PgDn / PgUp", "g / G", "n / N"] {
            assert!(help.contains(key), "help never mentions {key}");
        }
    }

    #[test]
    fn the_help_documents_the_folder_picker() {
        let help = help_lines().join("\n");
        assert!(help.contains("KEYS (folder picker"));
        assert!(help.contains("choose the focused folder"));
        assert!(help.contains("move [destination]"));
        assert!(help.contains("Enter, m"));
        // Every key the picker binds is named in its own block, paging
        // included: the block starts at its heading and ends at the blank
        // line before COMMANDS.
        let block: String = help
            .lines()
            .skip_while(|line| !line.starts_with("KEYS (folder picker"))
            .take_while(|line| !line.is_empty())
            .collect::<Vec<_>>()
            .join("\n");
        for key in [
            "j / k, Down / Up",
            "PgUp / PgDn",
            "l, Right",
            "h, Left, Backspace",
            "g / G",
            "Enter, m",
            "q, Esc",
        ] {
            assert!(
                block.contains(key),
                "picker help never names {key}:\n{block}"
            );
        }
    }

    #[test]
    fn every_reader_exit_key_returns_to_the_same_row() {
        let tmp = tempfile::tempdir().unwrap();
        fs::create_dir(tmp.path().join("sub")).unwrap();
        fs::write(tmp.path().join("a.txt"), "a").unwrap();
        fs::write(tmp.path().join("b.txt"), "b").unwrap();
        for key in [
            KeyInput::Char('h'),
            KeyInput::Char('q'),
            KeyInput::Esc,
            KeyInput::Left,
            KeyInput::Enter,
        ] {
            let mut app = app_in(&tmp);
            select(&mut app, "b.txt");
            let row = app.nav.cursor;
            let cwd = app.nav.cwd.clone();
            app.handle_key(KeyInput::Char('l'));
            assert!(matches!(app.mode, Mode::Pager(_)), "{key:?}");
            assert_eq!(app.handle_key(key), Effect::None);
            assert_eq!(app.mode, Mode::Browse, "{key:?} should close the reader");
            assert_eq!(app.nav.cursor, row, "{key:?} moved the cursor");
            assert_eq!(app.nav.cwd, cwd, "{key:?} moved the directory");
        }
    }

    #[test]
    fn reader_search_finds_types_and_steps() {
        let tmp = tempfile::tempdir().unwrap();
        let body: String = (1..=60).map(|i| format!("line {i}\n")).collect();
        fs::write(tmp.path().join("long.txt"), body).unwrap();
        let mut app = app_in(&tmp);
        app.viewport_rows = 12;
        app.viewport_cols = 40;
        select(&mut app, "long.txt");
        app.handle_key(KeyInput::Char('l'));

        app.handle_key(KeyInput::Char('/'));
        assert_eq!(reader(&app).find.as_deref(), Some(""));
        for c in "LINE 42".chars() {
            app.handle_key(KeyInput::Char(c));
        }
        assert_eq!(reader(&app).find.as_deref(), Some("LINE 42"));
        app.handle_key(KeyInput::Enter);

        let width = app.pager_cols();
        let glyphs = app.glyphs;
        let pager = reader(&app);
        assert!(pager.find.is_none());
        assert_eq!(pager.query, "LINE 42");
        // Case-insensitive: line 42 is source line 41.
        assert_eq!(pager.top_line(width, &glyphs), 41);

        // `n` wraps around to the only other match on that stem.
        app.handle_key(KeyInput::Char('n'));
        assert_eq!(reader(&app).top_line(width, &glyphs), 41);

        // A query nobody can match says so and leaves the view alone.
        app.handle_key(KeyInput::Char('/'));
        for c in "zebra".chars() {
            app.handle_key(KeyInput::Char(c));
        }
        app.handle_key(KeyInput::Enter);
        assert_eq!(last_msg(&app).level, Level::Error);
        assert!(last_msg(&app).text.contains("no match for 'zebra'"));
        assert_eq!(reader(&app).top_line(width, &glyphs), 41);
    }

    #[test]
    fn esc_in_the_find_prompt_leaves_the_search_not_the_reader() {
        let tmp = tempfile::tempdir().unwrap();
        fs::write(tmp.path().join("a.txt"), "alpha\nbeta\n").unwrap();
        let mut app = app_in(&tmp);
        select(&mut app, "a.txt");
        app.handle_key(KeyInput::Char('l'));
        app.handle_key(KeyInput::Char('/'));
        app.handle_key(KeyInput::Char('b'));
        app.handle_key(KeyInput::Backspace);
        assert_eq!(reader(&app).find.as_deref(), Some(""));
        app.handle_key(KeyInput::Esc);
        let pager = reader(&app);
        assert!(pager.find.is_none(), "Esc should close the find prompt");
        assert!(pager.query.is_empty());
        assert!(
            matches!(app.mode, Mode::Pager(_)),
            "Esc should not close the reader"
        );
    }

    #[test]
    fn a_file_bigger_than_the_caps_is_truncated_and_says_so() {
        let tmp = tempfile::tempdir().unwrap();
        let body: String = (0..preview::MAX_VIEW_LINES + 10)
            .map(|i| format!("line {i}\n"))
            .collect();
        fs::write(tmp.path().join("huge.txt"), body).unwrap();
        let mut app = app_in(&tmp);
        select(&mut app, "huge.txt");
        app.handle_key(KeyInput::Char('l'));
        let pager = reader(&app);
        assert_eq!(pager.doc().len(), preview::MAX_VIEW_LINES + 1);
        assert!(pager.text().contains("truncated"));
    }

    #[test]
    fn enter_still_hands_a_file_to_the_editor_not_the_reader() {
        let tmp = tempfile::tempdir().unwrap();
        fs::write(tmp.path().join("a.txt"), "a").unwrap();
        let mut app = app_in(&tmp);
        select(&mut app, "a.txt");
        assert!(matches!(
            app.handle_key(KeyInput::Enter),
            Effect::RunInteractive { .. }
        ));
        assert_eq!(app.mode, Mode::Browse);
    }

    #[test]
    fn no_reader_key_ever_mutates_the_filesystem() {
        // The reader is the one place a file's contents are on screen.
        // Same grammar rule as browse mode, enforced the same way.
        let tmp = tempfile::tempdir().unwrap();
        fs::write(tmp.path().join("notes.md"), "# Title\n\n- one\n> quoted\n").unwrap();
        let before = snapshot(tmp.path());
        let mut keys: Vec<KeyInput> = (0x20u8..0x7f).map(|c| KeyInput::Char(c as char)).collect();
        keys.extend([
            KeyInput::Backspace,
            KeyInput::Up,
            KeyInput::Down,
            KeyInput::Left,
            KeyInput::Right,
            KeyInput::PageUp,
            KeyInput::PageDown,
            KeyInput::Home,
            KeyInput::End,
        ]);
        for key in keys {
            let mut app = app_in(&tmp);
            select(&mut app, "notes.md");
            app.handle_key(KeyInput::Char('l'));
            assert!(matches!(app.mode, Mode::Pager(_)));
            let effect = app.handle_key(key);
            assert_eq!(effect, Effect::None, "{key:?} produced an effect");
            assert!(app.pending.is_none(), "{key:?} armed an operation");
            assert_eq!(snapshot(tmp.path()), before, "{key:?} changed the tree");
        }
    }

    #[test]
    fn n_without_a_search_says_what_to_press() {
        let tmp = tempfile::tempdir().unwrap();
        fs::write(tmp.path().join("a.txt"), "alpha\nbeta\n").unwrap();
        let mut app = app_in(&tmp);
        select(&mut app, "a.txt");
        app.handle_key(KeyInput::Char('l'));
        app.handle_key(KeyInput::Char('n'));
        assert_eq!(last_msg(&app).level, Level::Info);
        assert!(last_msg(&app).text.contains("press / to find"));
        assert!(matches!(app.mode, Mode::Pager(_)));
    }

    #[test]
    fn shift_n_steps_backwards_through_matches() {
        let tmp = tempfile::tempdir().unwrap();
        let body: String = (1..=60).map(|i| format!("mark {i}\n")).collect();
        fs::write(tmp.path().join("m.txt"), body).unwrap();
        let mut app = app_in(&tmp);
        app.viewport_rows = 12;
        app.viewport_cols = 40;
        select(&mut app, "m.txt");
        app.handle_key(KeyInput::Char('l'));
        app.handle_key(KeyInput::Char('/'));
        for c in "mark 3".chars() {
            app.handle_key(KeyInput::Char(c));
        }
        app.handle_key(KeyInput::Enter);
        let (width, glyphs) = (app.pager_cols(), app.glyphs);
        // mark 3, then mark 30..39.
        assert_eq!(reader(&app).top_line(width, &glyphs), 2);
        app.handle_key(KeyInput::Char('n'));
        assert_eq!(reader(&app).top_line(width, &glyphs), 29);
        app.handle_key(KeyInput::Char('N'));
        assert_eq!(reader(&app).top_line(width, &glyphs), 2);
    }

    #[test]
    fn l_on_the_parent_row_still_goes_up() {
        let tmp = tempfile::tempdir().unwrap();
        fs::create_dir(tmp.path().join("sub")).unwrap();
        let mut app = app_in(&tmp);
        let sub = app.nav.cwd.join("sub");
        app.nav.change_dir(sub, None).unwrap();
        select(&mut app, "..");
        app.handle_key(KeyInput::Char('l'));
        assert_eq!(app.nav.cwd, tmp.path().canonicalize().unwrap());
    }

    #[test]
    fn digits_jump_to_visible_ancestors() {
        let tmp = tempfile::tempdir().unwrap();
        let deep = tmp.path().join("a").join("b").join("c");
        fs::create_dir_all(&deep).unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let mut app = App::new(
            NavState::new(&deep).unwrap(),
            None,
            false,
            Some(root.clone()),
        );

        let ladder = app.ladder();
        assert_eq!(ladder.depth, 3);
        assert_eq!(ladder.rung(2).unwrap().path, root.join("a").join("b"));

        // Jumping lands on the ancestor and selects the child it came
        // through, exactly as going up does.
        app.handle_key(KeyInput::Char('2'));
        assert_eq!(app.nav.cwd, root.join("a").join("b"));
        assert_eq!(app.nav.selected().unwrap().name, "c");

        // `0` is always the anchor.
        app.handle_key(KeyInput::Char('0'));
        assert_eq!(app.nav.cwd, root);
        assert_eq!(app.nav.selected().unwrap().name, "a");

        // A digit with no rung reports itself and changes nothing.
        app.handle_key(KeyInput::Char('7'));
        assert_eq!(app.nav.cwd, root);
        assert_eq!(last_msg(&app).level, Level::Error);
        assert!(last_msg(&app).text.contains("no ancestor '7'"));

        // The digit for the current directory is a no-op, not a reload.
        let mut app = App::new(
            NavState::new(&deep).unwrap(),
            None,
            false,
            Some(root.clone()),
        );
        app.nav.set_filter("nothing".to_string());
        app.handle_key(KeyInput::Char('3'));
        assert_eq!(app.nav.cwd, root.join("a").join("b").join("c"));
        assert_eq!(app.nav.filter, "nothing");
        assert!(last_msg(&app).text.contains("already at"));
    }

    #[test]
    fn digit_jump_is_read_only_navigation() {
        let tmp = tempfile::tempdir().unwrap();
        let deep = tmp.path().join("a").join("b");
        fs::create_dir_all(&deep).unwrap();
        fs::write(deep.join("keep.txt"), "k").unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let mut app = App::new(NavState::new(&deep).unwrap(), None, false, Some(root));
        for digit in '0'..='9' {
            app.handle_key(KeyInput::Char(digit));
        }
        assert!(deep.join("keep.txt").exists());
        assert!(app.pending.is_none());
        assert_eq!(app.mode, Mode::Browse);
    }

    #[test]
    fn ladder_summary_states_depth_and_size_in_words() {
        let tmp = tempfile::tempdir().unwrap();
        fs::create_dir(tmp.path().join("sub")).unwrap();
        fs::write(tmp.path().join("one.txt"), "1").unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let app = App::new(NavState::new(&root).unwrap(), None, false, Some(root));
        assert_eq!(app.ladder_summary(), "depth 0 · 2 items");
    }

    #[test]
    fn message_history_is_reachable() {
        let tmp = tempfile::tempdir().unwrap();
        let mut app = app_in(&tmp);
        for i in 0..MAX_MESSAGES {
            app.push_msg(Level::Info, format!("event {i}"));
        }
        app.handle_key(KeyInput::Char('M'));
        let Mode::Pager(pager) = &app.mode else {
            panic!("expected the message pager");
        };
        assert!(pager.title.starts_with("messages ("));
        // The ring holds a hundred lines and all of them are now readable,
        // not just the three the strip shows.
        assert_eq!(pager.lines().len(), MAX_MESSAGES);
        assert!(pager.lines().last().unwrap().contains("event 99"));
        app.handle_key(KeyInput::Char('q'));
        assert_eq!(app.mode, Mode::Browse);
    }

    #[test]
    fn message_levels_share_one_prefix_width() {
        for glyphs in [Glyphs::UNICODE, Glyphs::ASCII] {
            for level in [Level::Info, Level::Ok, Level::Error] {
                assert_eq!(
                    bearings::display_width(&level.prefix(&glyphs)),
                    5,
                    "{level:?} in {glyphs:?}"
                );
            }
        }
    }

    #[test]
    fn message_pager_keeps_the_log_body_flush_across_levels() {
        let tmp = tempfile::tempdir().unwrap();
        let mut app = app_in(&tmp);
        app.messages.clear();
        for level in [Level::Info, Level::Ok, Level::Error] {
            app.push_msg(level, "aligned".to_string());
        }
        app.handle_key(KeyInput::Char('M'));
        let Mode::Pager(pager) = &app.mode else {
            panic!("expected the message pager");
        };
        let columns: Vec<usize> = pager
            .lines()
            .iter()
            .map(|line| bearings::display_width(&line[..line.find("aligned").unwrap()]))
            .collect();
        assert_eq!(columns, vec![6, 6, 6], "{:?}", pager.lines());
    }

    #[test]
    fn message_history_is_readable_when_empty() {
        let tmp = tempfile::tempdir().unwrap();
        let mut app = app_in(&tmp);
        app.messages.clear();
        app.handle_key(KeyInput::Char('M'));
        let Mode::Pager(pager) = &app.mode else {
            panic!("expected the message pager");
        };
        assert_eq!(pager.lines(), vec!["(no messages yet)".to_string()]);
    }

    #[test]
    fn no_browse_key_ever_mutates_the_filesystem() {
        // Grammar rule: motion never mutates, and mutation is always
        // select -> arm -> `y`. `d` is the one browse key allowed to
        // arm; arming still changes nothing. This is the mechanical
        // enforcement of both halves.
        let tmp = tempfile::tempdir().unwrap();
        fs::create_dir(tmp.path().join("sub")).unwrap();
        fs::write(tmp.path().join("sub/inner.txt"), "inner").unwrap();
        fs::write(tmp.path().join("a.txt"), "a").unwrap();
        fs::write(tmp.path().join(".dot"), "d").unwrap();
        let can = tempfile::tempdir().unwrap();

        let before = snapshot(tmp.path());
        let mut keys: Vec<KeyInput> = (0x20u8..0x7f).map(|c| KeyInput::Char(c as char)).collect();
        keys.extend([
            KeyInput::Enter,
            KeyInput::Esc,
            KeyInput::Backspace,
            KeyInput::Up,
            KeyInput::Down,
            KeyInput::Left,
            KeyInput::Right,
            KeyInput::PageUp,
            KeyInput::PageDown,
            KeyInput::Home,
            KeyInput::End,
        ]);
        for key in keys {
            let mut app = app_with_can(&tmp, &can);
            for row in 0..app.nav.visible().len() {
                app.nav.cursor = row;
                app.pending = None;
                if !matches!(app.mode, Mode::Browse) {
                    app.mode = Mode::Browse;
                }
                let effect = app.handle_key(key);
                assert!(
                    !matches!(effect, Effect::SpawnDetached { .. }),
                    "{key:?} spawned a process from browse mode"
                );
                assert!(
                    app.pending.is_none() || key == KeyInput::Char('d'),
                    "{key:?} armed an operation"
                );
            }
            assert_eq!(snapshot(tmp.path()), before, "{key:?} changed the tree");
            assert!(
                can_contents(&can).is_empty(),
                "{key:?} trashed something without a confirmation"
            );
        }
    }

    #[test]
    fn preview_uses_builtin_when_nvim_unavailable() {
        let tmp = tempfile::tempdir().unwrap();
        fs::write(tmp.path().join("doc.txt"), "content here").unwrap();
        let mut app = app_in(&tmp); // nvim_on_path: false
        select(&mut app, "doc.txt");
        assert_eq!(app.execute_line("preview"), Effect::None);
        let Mode::Pager(pager) = &app.mode else {
            panic!("expected built-in pager");
        };
        assert!(pager.lines().join("\n").contains("content here"));
    }

    #[test]
    fn preview_uses_readonly_nvim_for_text_when_available() {
        let tmp = tempfile::tempdir().unwrap();
        fs::write(tmp.path().join("doc.txt"), "text").unwrap();
        let nav = NavState::new(tmp.path()).unwrap();
        let mut app = App::new(nav, None, true, None);
        select(&mut app, "doc.txt");
        let effect = app.execute_line("preview");
        let Effect::RunInteractive { argv } = effect else {
            panic!("expected nvim preview, got {effect:?}");
        };
        assert_eq!(&argv[..4], &["nvim", "-R", "-M", "-n"]);
    }

    #[test]
    fn preview_binary_falls_back_to_builtin_even_with_nvim() {
        let tmp = tempfile::tempdir().unwrap();
        fs::write(tmp.path().join("blob.bin"), [0u8, 1, 2]).unwrap();
        let nav = NavState::new(tmp.path()).unwrap();
        let mut app = App::new(nav, None, true, None);
        select(&mut app, "blob.bin");
        assert_eq!(app.execute_line("preview"), Effect::None);
        let Mode::Pager(pager) = &app.mode else {
            panic!("expected built-in pager for binary file");
        };
        assert!(pager.lines().join("\n").contains("binary file"));
    }

    #[test]
    fn cd_command_changes_directory() {
        let tmp = tempfile::tempdir().unwrap();
        fs::create_dir(tmp.path().join("target dir")).unwrap();
        let mut app = app_in(&tmp);
        app.execute_line(r#"cd "target dir""#);
        assert!(app.nav.cwd.ends_with("target dir"));
        assert_eq!(last_msg(&app).level, Level::Ok);
    }

    #[test]
    fn cd_to_missing_directory_reports_error() {
        let tmp = tempfile::tempdir().unwrap();
        let mut app = app_in(&tmp);
        let before = app.nav.cwd.clone();
        app.execute_line("cd nowhere");
        assert_eq!(app.nav.cwd, before);
        assert_eq!(last_msg(&app).level, Level::Error);
    }

    #[test]
    fn cd_tilde_uses_captured_home() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("home");
        fs::create_dir(&home).unwrap();
        let nav = NavState::new(tmp.path()).unwrap();
        let mut app = App::new(nav, None, false, Some(home.clone()));
        app.execute_line("cd ~");
        assert_eq!(app.nav.cwd, home.canonicalize().unwrap());
        app.execute_line("cd");
        assert_eq!(app.nav.cwd, home.canonicalize().unwrap());
    }

    #[test]
    fn unknown_command_is_reported_not_executed() {
        let tmp = tempfile::tempdir().unwrap();
        let mut app = app_in(&tmp);
        app.execute_line("rm -rf /");
        assert_eq!(last_msg(&app).level, Level::Error);
        assert!(last_msg(&app).text.contains("unknown command"));
    }

    #[test]
    fn shell_metacharacters_never_reach_a_shell() {
        let tmp = tempfile::tempdir().unwrap();
        fs::write(tmp.path().join("a.txt"), "a").unwrap();
        let mut app = app_in(&tmp);
        select(&mut app, "a.txt");
        // A destination full of shell syntax is just a (bad) file name.
        // No `/` so this is a new name in the current directory, not a path.
        app.execute_line("move '$(touch pwned);x'");
        assert_eq!(app.mode, Mode::ConfirmOp);
        let PendingOp::Move { dst, .. } = app.pending.clone().unwrap() else {
            panic!("expected move");
        };
        assert!(dst
            .file_name()
            .unwrap()
            .to_string_lossy()
            .contains("$(touch"));
        app.handle_key(KeyInput::Esc);
        assert!(tmp.path().join("a.txt").exists());
        assert!(!tmp.path().join("pwned").exists());
    }

    #[test]
    fn command_mode_typing_and_escape() {
        let tmp = tempfile::tempdir().unwrap();
        let mut app = app_in(&tmp);
        app.handle_key(KeyInput::Char(':'));
        assert!(matches!(app.mode, Mode::Command { .. }));
        for c in "help".chars() {
            app.handle_key(KeyInput::Char(c));
        }
        app.handle_key(KeyInput::Backspace);
        let Mode::Command { input } = &app.mode else {
            panic!()
        };
        assert_eq!(input, "hel");
        app.handle_key(KeyInput::Esc);
        assert_eq!(app.mode, Mode::Browse);
    }

    #[test]
    fn command_mode_enter_executes() {
        let tmp = tempfile::tempdir().unwrap();
        let mut app = app_in(&tmp);
        app.handle_key(KeyInput::Char(':'));
        for c in "help".chars() {
            app.handle_key(KeyInput::Char(c));
        }
        app.handle_key(KeyInput::Enter);
        assert!(matches!(app.mode, Mode::Pager(_)));
    }

    #[test]
    fn filter_mode_narrows_live_and_esc_clears() {
        let tmp = tempfile::tempdir().unwrap();
        fs::write(tmp.path().join("apple.md"), "").unwrap();
        fs::write(tmp.path().join("banana.md"), "").unwrap();
        let mut app = app_in(&tmp);

        app.handle_key(KeyInput::Char('/'));
        for c in "app".chars() {
            app.handle_key(KeyInput::Char(c));
        }
        assert_eq!(app.nav.filter, "app");
        assert_eq!(app.nav.visible().len(), 2); // ".." + apple.md

        app.handle_key(KeyInput::Enter); // keep filter
        assert_eq!(app.mode, Mode::Browse);
        assert_eq!(app.nav.filter, "app");

        app.handle_key(KeyInput::Char('/'));
        app.handle_key(KeyInput::Esc); // clear filter
        assert!(app.nav.filter.is_empty());
        assert_eq!(app.nav.visible().len(), 3);
    }

    #[test]
    fn help_pager_opens_scrolls_and_closes() {
        let tmp = tempfile::tempdir().unwrap();
        let mut app = app_in(&tmp);
        app.handle_key(KeyInput::Char('?'));
        let Mode::Pager(p) = &app.mode else { panic!() };
        assert_eq!(p.title, "help");
        assert!(p.lines().iter().any(|l| l.contains("move [destination]")));

        app.handle_key(KeyInput::Char('j'));
        app.handle_key(KeyInput::PageDown);
        let Mode::Pager(p) = &app.mode else { panic!() };
        assert!(p.scroll > 0);
        app.handle_key(KeyInput::Char('q'));
        assert_eq!(app.mode, Mode::Browse);
    }

    #[test]
    fn agent_command_is_disabled_and_side_effect_free() {
        let tmp = tempfile::tempdir().unwrap();
        fs::write(tmp.path().join("secret.txt"), "s").unwrap();
        let mut app = app_in(&tmp);
        select(&mut app, "secret.txt");

        let effect = app.execute_line("agent do something scary");
        assert_eq!(effect, Effect::None, "agent must never produce an effect");
        let Mode::Pager(pager) = &app.mode else {
            panic!("expected explanation pager");
        };
        assert!(pager.title.contains("not configured"));
        assert!(pager.lines().join("\n").contains("not sent anywhere"));
        // Nothing changed on disk.
        assert_eq!(
            fs::read_to_string(tmp.path().join("secret.txt")).unwrap(),
            "s"
        );
    }

    #[test]
    fn open_targets_selected_entry_with_macos_open() {
        let tmp = tempfile::tempdir().unwrap();
        fs::write(tmp.path().join("doc.pdf"), "p").unwrap();
        let mut app = app_in(&tmp);
        select(&mut app, "doc.pdf");
        let effect = app.execute_line("open");
        if cfg!(target_os = "macos") {
            let Effect::SpawnDetached { argv } = effect else {
                panic!("expected SpawnDetached, got {effect:?}");
            };
            assert_eq!(argv[0], "/usr/bin/open");
            assert_eq!(argv[1], "--");
            assert!(argv[2].ends_with("doc.pdf"));
        } else {
            assert_eq!(effect, Effect::None);
            assert_eq!(last_msg(&app).level, Level::Error);
        }
    }

    #[test]
    fn message_log_is_capped() {
        let tmp = tempfile::tempdir().unwrap();
        let mut app = app_in(&tmp);
        for i in 0..300 {
            app.push_msg(Level::Info, format!("msg {i}"));
        }
        assert_eq!(app.messages.len(), MAX_MESSAGES);
        assert_eq!(last_msg(&app).text, "msg 299");
    }

    #[test]
    fn empty_directory_operations_do_not_panic() {
        let tmp = tempfile::tempdir().unwrap();
        let mut app = app_in(&tmp);
        // Only ".." is visible; filter it away entirely.
        app.nav.set_filter("zzz-no-match".to_string());
        app.handle_key(KeyInput::Char('j'));
        app.handle_key(KeyInput::Char('k'));
        app.nav.set_filter(String::new());
        assert!(app.nav.selected().is_some());
    }
}
