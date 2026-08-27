//! The Filecraft state machine.
//!
//! [`App::handle_key`] consumes abstract [`KeyInput`]s and returns
//! [`Effect`]s; the terminal event loop in `main.rs` translates real key
//! events in and interprets effects out. The app itself never touches the
//! terminal, so every interaction - including move/rename confirmation -
//! is deterministically testable.

use std::path::PathBuf;

use crate::agent::{self, Agent, AgentRequest};
use crate::command::{self, Command};
use crate::editor;
use crate::fsops::{self, FsError};
use crate::nav::NavState;
use crate::preview::{self, PreviewData};

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

/// A scrollable full-screen text pane (help, built-in preview, agent
/// explanation).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pager {
    pub title: String,
    pub lines: Vec<String>,
    pub scroll: usize,
}

/// Which input surface currently owns the keyboard.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Mode {
    Browse,
    Command { input: String },
    Filter { input: String },
    ConfirmOp,
    Pager(Pager),
}

/// A move or rename waiting for explicit confirmation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PendingOp {
    Move { src: PathBuf, dst: PathBuf },
    Rename { src: PathBuf, dst: PathBuf },
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
            KeyInput::Backspace | KeyInput::Left | KeyInput::Char('h') | KeyInput::Char('l') => {
                self.go_up()
            }
            KeyInput::Right => self.enter_selected_dir(),
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
            KeyInput::Char('q') | KeyInput::Esc => Effect::Quit,
            _ => Effect::None,
        }
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
            KeyInput::Char('n') | KeyInput::Char('N') | KeyInput::Esc => {
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
                self.push_msg(Level::Info, "press y to confirm or n to cancel".to_string());
                Effect::None
            }
        }
    }

    fn handle_pager_key(&mut self, key: KeyInput) -> Effect {
        let page = self.viewport_rows.max(1);
        let Mode::Pager(pager) = &mut self.mode else {
            return Effect::None;
        };
        let max_scroll = pager.lines.len().saturating_sub(1);
        match key {
            KeyInput::Char('j') | KeyInput::Down => {
                pager.scroll = (pager.scroll + 1).min(max_scroll);
            }
            KeyInput::Char('k') | KeyInput::Up => {
                pager.scroll = pager.scroll.saturating_sub(1);
            }
            KeyInput::PageDown => pager.scroll = (pager.scroll + page).min(max_scroll),
            KeyInput::PageUp => pager.scroll = pager.scroll.saturating_sub(page),
            KeyInput::Char('g') | KeyInput::Home => pager.scroll = 0,
            KeyInput::Char('G') | KeyInput::End => pager.scroll = max_scroll,
            KeyInput::Char('q') | KeyInput::Esc | KeyInput::Enter => {
                self.mode = Mode::Browse;
            }
            _ => {}
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
            Command::Move { destination } => self.cmd_move(&destination),
            Command::Rename { name } => self.cmd_rename(&name),
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

    fn perform_pending(&mut self) -> Effect {
        self.mode = Mode::Browse;
        let Some(op) = self.pending.take() else {
            return self.err("nothing to confirm".to_string());
        };
        let (src, dst, verb) = match &op {
            PendingOp::Move { src, dst } => (src.clone(), dst.clone(), "moved"),
            PendingOp::Rename { src, dst } => (src.clone(), dst.clone(), "renamed"),
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
                self.mode = Mode::Pager(Pager {
                    title: format!("preview: {title}"),
                    lines,
                    scroll: 0,
                });
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
        self.mode = Mode::Pager(Pager {
            title: "agent (not configured)".to_string(),
            lines: reply.lines,
            scroll: 0,
        });
        Effect::None
    }

    fn show_help(&mut self) -> Effect {
        self.mode = Mode::Pager(Pager {
            title: "help".to_string(),
            lines: help_lines(),
            scroll: 0,
        });
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
        "  Backspace, h, l      go to parent directory",
        "  Left                 go to parent directory",
        "  Right                enter selected directory",
        "  /                    filter the listing (Esc clears)",
        "  :                    command prompt",
        "  .                    show/hide dotfiles",
        "  r                    refresh listing",
        "  ?                    this help",
        "  q, Esc, Ctrl-C       quit",
        "",
        "COMMANDS (at the : prompt)",
        "  cd [path]            change directory (~ ok; quote spaces)",
        "  move <destination>   move selected entry (asks y/n first)",
        "  rename <new-name>    rename selected entry (asks y/n first)",
        "  open                 open selected entry with macOS 'open'",
        "  edit                 edit selected file in $EDITOR (or nvim)",
        "  preview              read-only preview (nvim -R, or built-in)",
        "  agent [...]          future AI seam - disabled in v0",
        "  help                 this help",
        "  quit                 leave filecraft",
        "",
        "SAFETY",
        "  - moves and renames never overwrite and always ask first",
        "  - there is no delete command in v0",
        "  - commands are parsed directly; nothing touches a shell",
        "  - everything stays on this machine: no network, no telemetry",
        "",
        "MARKERS   name/ directory   name@ symlink   name@! broken symlink",
        "",
        "press q or Esc to close this help",
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

    fn select(app: &mut App, name: &str) {
        let visible = app.nav.visible();
        let pos = visible
            .iter()
            .position(|&i| app.nav.entries[i].name == name)
            .unwrap_or_else(|| panic!("entry '{name}' not visible"));
        app.nav.cursor = pos;
    }

    fn last_msg(app: &App) -> &Message {
        app.messages.last().expect("expected a message")
    }

    #[test]
    fn quit_keys() {
        let tmp = tempfile::tempdir().unwrap();
        let mut app = app_in(&tmp);
        assert_eq!(app.handle_key(KeyInput::Char('q')), Effect::Quit);
        assert_eq!(app.handle_key(KeyInput::Esc), Effect::Quit);
        assert_eq!(app.handle_key(KeyInput::CtrlC), Effect::Quit);
        assert_eq!(app.execute_line("quit"), Effect::Quit);
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

        for line in ["move x", "rename x", "edit", "preview", "open"] {
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
        for key in [
            KeyInput::Backspace,
            KeyInput::Char('h'),
            KeyInput::Char('l'),
            KeyInput::Left,
        ] {
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
    fn preview_uses_builtin_when_nvim_unavailable() {
        let tmp = tempfile::tempdir().unwrap();
        fs::write(tmp.path().join("doc.txt"), "content here").unwrap();
        let mut app = app_in(&tmp); // nvim_on_path: false
        select(&mut app, "doc.txt");
        assert_eq!(app.execute_line("preview"), Effect::None);
        let Mode::Pager(pager) = &app.mode else {
            panic!("expected built-in pager");
        };
        assert!(pager.lines.join("\n").contains("content here"));
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
        assert!(pager.lines.join("\n").contains("binary file"));
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
        assert!(p.lines.iter().any(|l| l.contains("move <destination>")));

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
        assert!(pager.lines.join("\n").contains("not sent anywhere"));
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
