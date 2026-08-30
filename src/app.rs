//! The Filecraft state machine.
//!
//! [`App::handle_key`] consumes abstract [`KeyInput`]s and returns
//! [`Effect`]s; the terminal event loop in `main.rs` translates real key
//! events in and interprets effects out. The app itself never touches the
//! terminal, so every interaction - including the move, rename, and
//! trash confirmations - is deterministically testable.

use std::path::{Path, PathBuf};
use std::time::{Instant, SystemTime};

use crate::agent::{self, Agent, AgentRequest};
use crate::bearings::{self, Glyphs, Ladder};
use crate::command::{self, Command};
use crate::config;
use crate::editor;
use crate::fsops::{self, FsError};
use crate::i18n::{Lang, Op};
use crate::joblog::{self, LogPane};
use crate::markdown::{self, DocLine};
use crate::multiselect::{self, FileSelector, Toggled};
use crate::nav::NavState;
use crate::pager::{self, Pager};
use crate::picker::{self, FolderPicker};
use crate::preview::{self, PreviewData, ViewSource};
use crate::stream;
use crate::summarize::{self, Job, JobSpec, Outcome, Provider, Runner};
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
    Command {
        input: String,
    },
    Filter {
        input: String,
    },
    ConfirmOp,
    /// `q` / Ctrl-C with a summary still running. Only `y` leaves.
    ConfirmQuit,
    FolderPicker(FolderPicker),
    /// Picking the files an AI summary will cover.
    FileSelector(FileSelector),
    /// Picking which AI CLI runs it, over the files already chosen.
    ProviderMenu {
        files: Vec<PathBuf>,
    },
    Pager(Pager),
    /// Watching a summary run's own output. Read-only, like every other
    /// pane: closing it closes a view, never the run.
    JobLog(LogPane),
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
    pub fn describe(&self, lang: Lang) -> String {
        match self {
            PendingOp::Move { src, dst } => {
                lang.describe_move(&src.display().to_string(), &dst.display().to_string())
            }
            PendingOp::Rename { src, dst } => lang.describe_rename(
                &src.file_name().unwrap_or_default().to_string_lossy(),
                &dst.file_name().unwrap_or_default().to_string_lossy(),
            ),
            PendingOp::Trash { name, .. } => lang.describe_trash(name),
        }
    }

    /// Whether Enter may stand in for `y`.
    ///
    /// It may not for a trash: `d` in browse is a page-scroll in the
    /// reader and Enter in browse activates the selection, so the two
    /// keys are reachable in a row from muscle memory. An operation that
    /// takes an entry out of its directory is answered with the letter
    /// and nothing else. A move or a rename keeps the older contract -
    /// both are reversible in place, and both are typed on purpose.
    pub fn needs_explicit_yes(&self) -> bool {
        matches!(self, PendingOp::Trash { .. })
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

/// A path as the message log names it: the file name alone, because the
/// selector header already says where the summary will land.
fn display_name(path: &Path) -> String {
    path.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string())
}

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
    /// The language every phrase on screen is said in.
    ///
    /// One source of truth: the renderer reads it from here rather than
    /// resolving it again, so `:lang` changes the whole screen at the
    /// next frame and nothing can be left speaking the old language.
    pub lang: Lang,
    /// Where a language change is remembered, when there is anywhere to
    /// remember it. `None` means this session only, and `:lang` says so
    /// rather than silently forgetting.
    pub config_path: Option<PathBuf>,
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
    /// How an AI summary is run. A child process in the shipped binary;
    /// a scripted stand-in in tests, so no provider is ever needed.
    pub runner: Box<dyn Runner>,
    /// The one summary that may be in flight. There is at most one, so
    /// the status row, the quit confirmation, and the message it finally
    /// logs all refer to the same unambiguous thing.
    pub job: Option<ActiveJob>,
    /// The most recent summary run's log. Kept after the run ends - and
    /// after a run that could not even start - so `L` can still be
    /// pressed to read what the provider actually said.
    pub run_log: Option<RunLog>,
}

/// A summary run's own output, and enough about the run to make sense of
/// it. Outlives the [`ActiveJob`]: a finished run is exactly the one you
/// want to read afterwards.
#[derive(Debug, Clone)]
pub struct RunLog {
    pub provider: Provider,
    /// The Markdown file the run was asked to write.
    pub output: PathBuf,
    /// Everything the provider printed, filled in as it printed it.
    pub stream: stream::Handle,
}

impl RunLog {
    /// Whether this is the run currently going, rather than the last one
    /// that went.
    pub fn running(&self) -> bool {
        self.stream.running()
    }
}

/// A summary run in flight, plus the spec that describes it in words.
pub struct ActiveJob {
    pub spec: JobSpec,
    handle: Box<dyn Job>,
}

impl ActiveJob {
    /// Adopt an already-started run. The state machine builds these
    /// through [`App::start_summary`]; this is also how a test or another
    /// front end hands in a job of its own.
    pub fn new(spec: JobSpec, handle: Box<dyn Job>) -> Self {
        ActiveJob { spec, handle }
    }

    /// The live status the screen shows while this runs.
    pub fn status_line(&self, lang: Lang) -> String {
        self.spec.status_line(lang)
    }
}

impl App {
    pub fn new(
        nav: NavState,
        editor_env: Option<String>,
        nvim_on_path: bool,
        home: Option<PathBuf>,
        lang: Lang,
    ) -> Self {
        let mut app = App {
            nav,
            mode: Mode::Browse,
            pending: None,
            messages: Vec::new(),
            editor_env,
            nvim_on_path,
            home,
            lang,
            config_path: None,
            viewport_rows: 20,
            viewport_cols: 80,
            glyphs: Glyphs::UNICODE,
            trasher: trash::system(),
            runner: summarize::process_runner(),
            job: None,
            run_log: None,
        };
        app.push_msg(Level::Info, lang.welcome().to_string());
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
            // Ctrl-C still leaves from anywhere, but a running summary is
            // a child process someone started on purpose: it is killed
            // deliberately or not at all.
            return self.quit_or_confirm();
        }
        match &self.mode {
            Mode::Browse => self.handle_browse_key(key),
            Mode::Command { .. } => self.handle_command_key(key),
            Mode::Filter { .. } => self.handle_filter_key(key),
            Mode::ConfirmOp => self.handle_confirm_key(key),
            Mode::ConfirmQuit => self.handle_confirm_quit_key(key),
            Mode::FolderPicker(_) => self.handle_picker_key(key),
            Mode::FileSelector(_) => self.handle_selector_key(key),
            Mode::ProviderMenu { .. } => self.handle_provider_key(key),
            Mode::Pager(_) | Mode::JobLog(_) => self.handle_pager_key(key),
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
            // The live log of the summary run. Read-only, and it does
            // not touch the run: it opens a view over output already
            // captured.
            KeyInput::Char('L') => self.cmd_log(),
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
                    return self.err(e.message(self.lang));
                }
                let note = if self.nav.show_hidden {
                    self.lang.dotfiles_now_shown()
                } else {
                    self.lang.dotfiles_now_hidden()
                };
                self.push_msg(Level::Info, note.to_string());
                Effect::None
            }
            KeyInput::Char('r') => {
                if let Err(e) = self.nav.refresh() {
                    return self.err(e.message(self.lang));
                }
                let refreshed = self.lang.refreshed().to_string();
                self.push_msg(Level::Info, refreshed);
                Effect::None
            }
            // `S`, like `:summarize`, only opens the file selector.
            // Nothing is read, sent, or run until files are picked and a
            // provider is chosen.
            KeyInput::Char('S') => self.cmd_summarize(),
            KeyInput::Char('q') => self.quit_or_confirm(),
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
        let cleared = self.lang.filter_cleared().to_string();
        self.push_msg(Level::Info, cleared);
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
                let cleared = self.lang.filter_cleared().to_string();
                self.push_msg(Level::Info, cleared);
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
        let enter_confirms = self
            .pending
            .as_ref()
            .is_some_and(|op| !op.needs_explicit_yes());
        match key {
            KeyInput::Char('y') | KeyInput::Char('Y') => self.perform_pending(),
            KeyInput::Enter if enter_confirms => self.perform_pending(),
            // `q` cancels here, as it does in the reader and the folder
            // picker: the back-out key never means "go ahead".
            KeyInput::Char('n') | KeyInput::Char('N') | KeyInput::Char('q') | KeyInput::Esc => {
                let lang = self.lang;
                let description = self
                    .pending
                    .take()
                    .map(|op| op.describe(lang))
                    .unwrap_or_default();
                self.mode = Mode::Browse;
                self.push_msg(Level::Info, lang.cancelled(&description));
                Effect::None
            }
            _ => {
                self.push_msg(Level::Info, self.lang.press_y_or_cancel().to_string());
                Effect::None
            }
        }
    }

    /// `q` and Ctrl-C. With no summary running this is a plain quit; with
    /// one running it raises the prompt instead, because leaving would
    /// kill a child process the user started on purpose.
    fn quit_or_confirm(&mut self) -> Effect {
        let Some(job) = &self.job else {
            return Effect::Quit;
        };
        let status = job.status_line(self.lang);
        if matches!(self.mode, Mode::ConfirmQuit) {
            return Effect::None;
        }
        self.mode = Mode::ConfirmQuit;
        // The prompt row asks the question; the log says which run it is
        // about, which is the part the prompt has no room for.
        self.push_msg(Level::Info, self.lang.confirm_quit_line(&status));
        Effect::None
    }

    fn handle_confirm_quit_key(&mut self, key: KeyInput) -> Effect {
        match key {
            KeyInput::Char('y') | KeyInput::Char('Y') => {
                if let Some(mut job) = self.job.take() {
                    job.handle.terminate();
                }
                Effect::Quit
            }
            // Enter is not an answer here, for the same reason it is not
            // one for a trash: the key that raised the prompt sits one
            // slip away, and terminating a run is not undone by retrying.
            KeyInput::Char('n') | KeyInput::Char('N') | KeyInput::Esc => {
                self.mode = Mode::Browse;
                let note = self.lang.summary_still_running().to_string();
                self.push_msg(Level::Info, note);
                Effect::None
            }
            _ => {
                let note = self.lang.press_y_to_terminate().to_string();
                self.push_msg(Level::Info, note);
                Effect::None
            }
        }
    }

    /// Rows of entries the file selector has, mirrored from the listing
    /// area the same way the reader's and the picker's rows are.
    pub fn selector_rows(&self) -> usize {
        self.viewport_rows
            .saturating_sub(multiselect::FRAME_ROWS)
            .max(1)
    }

    /// `:summarize` / `:summary` / `S` - open the file selector. Nothing
    /// is read or sent here; this only lists folders and documents.
    fn cmd_summarize(&mut self) -> Effect {
        if let Some(job) = &self.job {
            let status = job.status_line(self.lang);
            return self.err(self.lang.already_running(&status));
        }
        match FileSelector::open(&self.nav.cwd, self.nav.show_hidden) {
            Ok(selector) => {
                let opened = self.lang.summarize_opened(&summarize::summarizable_note());
                self.push_msg(Level::Info, opened);
                self.mode = Mode::FileSelector(selector);
                Effect::None
            }
            Err(e) => {
                let text = self.lang.summarize_error(&e.message(self.lang));
                self.err(text)
            }
        }
    }

    fn handle_selector_key(&mut self, key: KeyInput) -> Effect {
        let rows = self.selector_rows();
        let lang = self.lang;
        let mut confirm = false;
        let mut cancel = false;
        let mut err: Option<String> = None;
        let mut info: Option<String> = None;
        {
            let Mode::FileSelector(selector) = &mut self.mode else {
                return Effect::None;
            };
            match key {
                KeyInput::Char('j') | KeyInput::Down => selector.move_cursor(1),
                KeyInput::Char('k') | KeyInput::Up => selector.move_cursor(-1),
                KeyInput::PageDown => selector.move_cursor(rows as isize),
                KeyInput::PageUp => selector.move_cursor(-(rows as isize)),
                KeyInput::Char('g') | KeyInput::Home => selector.cursor_to_start(),
                KeyInput::Char('G') | KeyInput::End => selector.cursor_to_end(),
                KeyInput::Char('l') | KeyInput::Right => {
                    if let Err(e) = selector.enter_focused() {
                        err = Some(lang.summarize_error(&e.message(lang)));
                    }
                }
                KeyInput::Backspace | KeyInput::Left | KeyInput::Char('h') => {
                    match selector.go_up() {
                        Ok(true) => {}
                        Ok(false) => info = Some(lang.at_filesystem_root().to_string()),
                        Err(e) => err = Some(lang.summarize_error(&e.message(lang))),
                    }
                }
                KeyInput::Char(' ') => match selector.toggle_focused() {
                    Ok(Toggled::Added(path)) => {
                        info = Some(lang.file_selected(&display_name(&path), selector.count()));
                    }
                    Ok(Toggled::Removed(path)) => {
                        info = Some(lang.file_unselected(&display_name(&path), selector.count()));
                    }
                    Err(e) => err = Some(lang.summarize_error(&e.message(lang))),
                },
                KeyInput::Enter | KeyInput::Char('c') => confirm = true,
                KeyInput::Esc | KeyInput::Char('q') => cancel = true,
                _ => {}
            }
        }
        if cancel {
            self.mode = Mode::Browse;
            self.push_msg(Level::Info, lang.cancelled_summarize().to_string());
            return Effect::None;
        }
        if confirm {
            return self.confirm_selection();
        }
        if let Some(text) = err {
            return self.err(text);
        }
        if let Some(text) = info {
            self.push_msg(Level::Info, text);
        }
        Effect::None
    }

    /// Enter / `c` in the selector: hand the chosen files to the provider
    /// dialog. An empty selection is refused with the selector still up,
    /// so nothing is lost by pressing Enter early.
    fn confirm_selection(&mut self) -> Effect {
        let files = match &self.mode {
            Mode::FileSelector(selector) => selector.chosen.clone(),
            _ => return Effect::None,
        };
        if files.is_empty() {
            let text = self.lang.summarize_nothing_selected().to_string();
            return self.err(text);
        }
        let line = self.lang.choose_a_provider(files.len());
        self.push_msg(Level::Info, line);
        self.mode = Mode::ProviderMenu { files };
        Effect::None
    }

    fn handle_provider_key(&mut self, key: KeyInput) -> Effect {
        let choice = match key {
            // Enter alone is the default provider - the one choice that
            // needs no reading.
            KeyInput::Enter => summarize::resolve(None),
            KeyInput::Char(c) if c.is_ascii_digit() => match summarize::resolve(Some(c)) {
                Some(provider) => Some(provider),
                None => return self.err(self.lang.no_such_provider(c)),
            },
            KeyInput::Esc | KeyInput::Char('q') => {
                self.mode = Mode::Browse;
                let note = self.lang.cancelled_summarize().to_string();
                self.push_msg(Level::Info, note);
                return Effect::None;
            }
            _ => return Effect::None,
        };
        let Some(provider) = choice else {
            return Effect::None;
        };
        self.start_summary(provider)
    }

    /// Spawn the summary in the background. The screen stays live: the
    /// only thing that changes here is that a job exists.
    fn start_summary(&mut self, provider: Provider) -> Effect {
        let files = match &self.mode {
            Mode::ProviderMenu { files } => files.clone(),
            _ => return Effect::None,
        };
        self.mode = Mode::Browse;
        let Some(first) = files.first().cloned() else {
            let text = self.lang.summarize_no_files().to_string();
            return self.err(text);
        };
        let output = summarize::output_path(&first, &summarize::stamp(SystemTime::now()));
        let Some(spec) = JobSpec::new(provider, files, output) else {
            let text = self.lang.summarize_no_files();
            return self.err(text);
        };
        // The log is the app's, not the job's: the runner fills it while
        // it runs, and it is still here to read once the job is gone.
        let live = stream::Handle::new();
        let started = self.runner.start(&spec, &live);
        self.run_log = Some(RunLog {
            provider,
            output: spec.output.clone(),
            stream: live,
        });
        match started {
            Ok(handle) => {
                let (status, will_write, watch) = (
                    spec.status_line(self.lang),
                    self.lang.will_write(&spec.output.display().to_string()),
                    self.lang.watch_the_provider().to_string(),
                );
                self.push_msg(Level::Ok, status);
                self.push_msg(Level::Info, will_write);
                self.push_msg(Level::Info, watch);
                self.job = Some(ActiveJob { spec, handle });
                Effect::None
            }
            Err(e) => {
                // A run that never started still has a log - one line
                // saying why - and `L` still opens it.
                if let Some(run) = &self.run_log {
                    run.stream.end();
                }
                self.err(self.lang.summarize_error(&e.message(self.lang)))
            }
        }
    }

    /// Whether a summary is in flight, for the status row and the event
    /// loop's tick.
    pub fn job_active(&self) -> bool {
        self.job.is_some()
    }

    /// The live status the status row shows, if anything is running.
    pub fn job_status(&self) -> Option<String> {
        let lang = self.lang;
        self.job.as_ref().map(|job| job.status_line(lang))
    }

    /// Ask the running summary whether it is done, and report it if so.
    /// Called from the event loop, never blocking.
    pub fn poll_job(&mut self) {
        let Some(job) = &mut self.job else {
            return;
        };
        let Some(outcome) = job.handle.poll() else {
            return;
        };
        self.job = None;
        // Whatever the runner did with it, the log is closed when the
        // app collects the outcome: the header stops saying "thinking"
        // about a run that is over.
        if let Some(run) = &self.run_log {
            run.stream.end();
        }
        match outcome {
            Outcome::Written(path) => {
                let written = self.lang.summary_written(&path.display().to_string());
                self.push_msg(Level::Ok, written);
                let Some(parent) = path.parent() else {
                    return;
                };
                let name = path
                    .file_name()
                    .map(|name| name.to_string_lossy().into_owned());
                let moved = parent != self.nav.cwd;
                let result = if moved {
                    self.nav.change_dir(parent.to_path_buf(), name.as_deref())
                } else {
                    self.nav.refresh()
                };
                if let Err(e) = result {
                    let text = e.message(self.lang);
                    self.push_msg(Level::Error, text);
                    return;
                }
                if moved {
                    let note = self.lang.listing_moved_to(&parent.display().to_string());
                    self.push_msg(Level::Info, note);
                }
                if let Some(name) = name {
                    let visible = self.nav.visible();
                    if let Some(pos) = visible
                        .iter()
                        .position(|&i| self.nav.entries[i].name == name)
                    {
                        self.nav.cursor = pos;
                        let note = self.lang.press_l_to_read().to_string();
                        self.push_msg(Level::Info, note);
                    }
                }
            }
            Outcome::Failed(reason) => {
                // A run's own account of what went wrong, in the screen's
                // language - except for the one part that is evidence
                // rather than prose, the provider's own last line, which
                // `Failure::Provider` carries through untranslated.
                let text = self.lang.summarize_error(&reason.message(self.lang));
                self.push_msg(Level::Error, text);
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
        self.sync_job_log();
    }

    /// Read the running summary's log into an open log viewer.
    ///
    /// Called from [`App::set_viewport`], which every surface that draws
    /// goes through once a frame: a log that grew between two frames is
    /// on screen at the next one, without a keypress and without the
    /// event loop knowing anything about panes.
    fn sync_job_log(&mut self) {
        let Some(live) = self.run_log.as_ref().map(|run| run.stream.clone()) else {
            return;
        };
        let (width, view, glyphs, lang) =
            (self.log_cols(), self.log_rows(), self.glyphs, self.lang);
        if let Mode::JobLog(pane) = &mut self.mode {
            pane.sync(&live, Instant::now(), width, view, &glyphs, lang);
        }
    }

    /// Columns of text the log viewer has - the reader's frame plus its
    /// own pinned header.
    pub fn log_cols(&self) -> usize {
        self.viewport_cols.saturating_sub(joblog::FRAME_COLS).max(1)
    }

    /// Rows of log the viewer has.
    pub fn log_rows(&self) -> usize {
        self.viewport_rows.saturating_sub(joblog::FRAME_ROWS).max(1)
    }

    /// The geometry of whichever full-screen pane is open, so one set of
    /// scroll keys serves the reader and the log viewer alike.
    fn pane_geometry(&self) -> (usize, usize) {
        match &self.mode {
            Mode::JobLog(_) => (self.log_cols(), self.log_rows()),
            _ => (self.pager_cols(), self.pager_rows()),
        }
    }

    /// The [`Pager`] inside whichever pane is open.
    fn pane(&mut self) -> Option<&mut Pager> {
        match &mut self.mode {
            Mode::Pager(pager) => Some(pager),
            Mode::JobLog(pane) => Some(&mut pane.pager),
            _ => None,
        }
    }

    /// `L` / `:log` / `:job` - open the summary run's own output.
    ///
    /// Read-only and detached: the run is not touched, not waited on,
    /// and not stopped when the pane is closed. A finished run's log is
    /// still here, which is what makes this the place to find the
    /// session a provider announced.
    fn cmd_log(&mut self) -> Effect {
        let Some(run) = &self.run_log else {
            let text = self.lang.log_never_ran().to_string();
            return self.err(text);
        };
        let (provider, live) = (run.provider, run.stream.clone());
        let mut pane = LogPane::new(provider, self.lang);
        let (width, view, glyphs, lang) =
            (self.log_cols(), self.log_rows(), self.glyphs, self.lang);
        pane.sync(&live, Instant::now(), width, view, &glyphs, lang);
        self.mode = Mode::JobLog(pane);
        Effect::None
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

    /// The scroll, search, and back-out keys of a full-screen pane.
    ///
    /// One implementation for the file reader and the log viewer: they
    /// are the same pane over different documents, and a key that meant
    /// two things in the two of them would be the bug this avoids.
    fn handle_pager_key(&mut self, key: KeyInput) -> Effect {
        if self.pane().is_some_and(|pager| pager.find.is_some()) {
            return self.handle_find_key(key);
        }
        let (width, view) = self.pane_geometry();
        let (glyphs, lang) = (self.glyphs, self.lang);
        let page = view as isize;
        let half = (view as isize / 2).max(1);
        let mut close = false;
        let mut missed: Option<(Level, String)> = None;
        {
            let Some(pager) = self.pane() else {
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
                        missed = Some((Level::Info, lang.no_search_yet().to_string()));
                    } else if !pager.step_match(forward, width, view, &glyphs) {
                        missed = Some((Level::Error, lang.no_match_for(&pager.query)));
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
            // exactly the row the reader was opened from. A summary run
            // watched through the log viewer keeps running: the pane
            // owns nothing the run needs.
            self.mode = Mode::Browse;
        } else if let Mode::JobLog(pane) = &mut self.mode {
            // Following the newest output *is* being at the bottom, so
            // it is re-read from where the scroll left the view rather
            // than toggled by a key of its own.
            pane.refollow(width, view, &glyphs);
        }
        if let Some((level, text)) = missed {
            self.push_msg(level, text);
        }
        Effect::None
    }

    /// The `/` prompt inside the reader. Esc leaves the search, not the
    /// reader - backing out is always exactly one level.
    fn handle_find_key(&mut self, key: KeyInput) -> Effect {
        let (width, view) = self.pane_geometry();
        let (glyphs, lang) = (self.glyphs, self.lang);
        let mut missed: Option<String> = None;
        {
            let Some(pager) = self.pane() else {
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
                        missed = Some(lang.no_match_for(&pager.query));
                    }
                }
                _ => {}
            }
        }
        if let Mode::JobLog(pane) = &mut self.mode {
            // A committed search moves the view exactly as a scroll key
            // does, so the follow is re-read here too: otherwise the next
            // frame pulls the log back to the bottom and the match the
            // search just found is never seen.
            pane.refollow(width, view, &glyphs);
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
            let text = self.lang.nothing_selected().to_string();
            return self.err(text);
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
            crate::nav::EntryKind::SymlinkBroken => self.err(self.lang.broken_symlink(&entry.name)),
            _ => self.err(self.lang.cannot_open_special(&entry.name)),
        }
    }

    /// `l` / Right: descend into a directory, or open a text file in the
    /// read-only reader. Both halves are read-only - this key never
    /// launches an editor and never touches the file.
    fn open_selected(&mut self) -> Effect {
        let Some(entry) = self.nav.selected().cloned() else {
            let text = self.lang.nothing_selected().to_string();
            return self.err(text);
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
            crate::nav::EntryKind::SymlinkBroken => self.err(self.lang.broken_symlink(&entry.name)),
            _ => self.err(self.lang.cannot_read_special(&entry.name)),
        }
    }

    /// Open the selected regular file in the reader. Markdown gets its
    /// structure drawn; anything else readable is shown as it is; a
    /// binary is refused in words rather than painted on the screen.
    fn open_pager_for_file(&mut self) -> Effect {
        let lang = self.lang;
        let (name, path) = match self.selected_operand() {
            Ok(v) => v,
            Err(e) => return self.err(self.lang.op_says(Op::Read, &e)),
        };
        let source = match preview::read_view(&path) {
            Ok(source) => source,
            Err(e) => return self.err(lang.op_says(Op::Read, &e.message(lang))),
        };
        let ViewSource::Text { text, truncated } = source else {
            return self.err(lang.not_text(&name));
        };
        let mut doc = if text.is_empty() {
            vec![DocLine::meta(lang.empty_file())]
        } else if markdown::is_markdown(&path) {
            markdown::parse_markdown(&text)
        } else {
            markdown::parse_plain(&text)
        };
        if truncated {
            doc.push(DocLine::meta(lang.truncated(
                preview::MAX_VIEW_LINES,
                preview::MAX_VIEW_BYTES / 1024,
            )));
        }
        self.mode = Mode::Pager(Pager::document(name, doc));
        Effect::None
    }

    fn enter_selected_dir(&mut self) -> Effect {
        let Some(entry) = self.nav.selected().cloned() else {
            let text = self.lang.nothing_selected().to_string();
            return self.err(text);
        };
        if entry.is_parent {
            return self.go_up();
        }
        if !entry.is_enterable() {
            return self.err(self.lang.not_a_directory(&entry.name));
        }
        let path = self.nav.cwd.join(&entry.name);
        let canonical = match std::fs::canonicalize(&path) {
            Ok(c) => c,
            Err(e) => return self.err(fsops::io_error(&path, &e).message(self.lang)),
        };
        match self.nav.change_dir(canonical, None) {
            Ok(()) => Effect::None,
            Err(e) => self.err(e.message(self.lang)),
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
        self.lang.ladder_summary(depth, items, glyphs.dot)
    }

    /// Jump to a visible ancestor. Pure navigation: it goes through
    /// `NavState::change_dir` exactly as `cd` does, and selects the child
    /// it came through the way going up does.
    fn jump_to_rung(&mut self, digit: u8) -> Effect {
        let ladder = self.ladder();
        let Some(rung) = ladder.rung(digit) else {
            return self.err(self.lang.no_such_rung(digit));
        };
        let (target, label) = (rung.path.clone(), rung.label.clone());
        if target == self.nav.cwd {
            let note = self.lang.already_at(&label);
            self.push_msg(Level::Info, note);
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
                let cwd = self.lang.cwd_line(&self.nav.cwd.display().to_string());
                self.push_msg(Level::Ok, cwd);
                Effect::None
            }
            Err(e) => self.err(e.message(self.lang)),
        }
    }

    /// Open the message ring in the existing pager. The log keeps a
    /// hundred lines but the strip shows three; this is how the other
    /// ninety-seven are reachable.
    fn show_messages(&mut self) -> Effect {
        let lines: Vec<String> = if self.messages.is_empty() {
            vec![self.lang.no_messages_yet().to_string()]
        } else {
            self.messages
                .iter()
                .map(|message| format!("{} {}", message.level.prefix(&self.glyphs), message.text))
                .collect()
        };
        let title = self.lang.messages_title(self.messages.len(), MAX_MESSAGES);
        self.mode = Mode::Pager(Pager::plain(title, lines));
        Effect::None
    }

    fn go_up(&mut self) -> Effect {
        match self.nav.go_up() {
            Ok(true) => Effect::None,
            Ok(false) => {
                let note = self.lang.at_filesystem_root().to_string();
                self.push_msg(Level::Info, note);
                Effect::None
            }
            Err(e) => self.err(e.message(self.lang)),
        }
    }

    /// Parse and run one BBS command line. Public for the prompt and for
    /// tests.
    pub fn execute_line(&mut self, line: &str) -> Effect {
        match command::parse(line) {
            Ok(cmd) => self.execute(cmd),
            Err(e) => self.err(e.message(self.lang)),
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
            Command::Summarize => self.cmd_summarize(),
            Command::Log => self.cmd_log(),
            Command::Language { code } => self.cmd_language(code.as_deref()),
            Command::Help => self.show_help(),
            Command::Quit => self.quit_or_confirm(),
            Command::Agent { args } => self.cmd_agent(args),
        }
    }

    fn cmd_cd(&mut self, path: Option<String>) -> Effect {
        let target = match path {
            Some(p) => p,
            None => match &self.home {
                Some(home) => home.display().to_string(),
                None => {
                    let text = self.lang.home_unknown().to_string();
                    return self.err(text);
                }
            },
        };
        let dir = match fsops::canonical_dir(&self.nav.cwd, &target, self.home.as_deref()) {
            Ok(d) => d,
            Err(e) => return self.err(self.lang.op_says(Op::Cd, &e.message(self.lang))),
        };
        match self.nav.change_dir(dir, None) {
            Ok(()) => {
                let cwd = self.lang.cwd_line(&self.nav.cwd.display().to_string());
                self.push_msg(Level::Ok, cwd);
                Effect::None
            }
            Err(e) => self.err(self.lang.op_says(Op::Cd, &e.message(self.lang))),
        }
    }

    /// Resolve the selection for an operation; the synthetic `..` row is
    /// never a valid target.
    fn selected_operand(&self) -> Result<(String, PathBuf), String> {
        let Some(entry) = self.nav.selected() else {
            return Err(self.lang.nothing_selected().to_string());
        };
        if entry.is_parent {
            return Err(self.lang.cannot_operate_on_parent().to_string());
        }
        Ok((entry.name.clone(), self.nav.cwd.join(&entry.name)))
    }

    /// `:move` with no path: pick a destination folder, then confirm.
    fn open_move_picker(&mut self) -> Effect {
        let (name, src) = match self.selected_operand() {
            Ok(v) => v,
            Err(e) => return self.err(self.lang.op_says(Op::Move, &e)),
        };
        match FolderPicker::open(&self.nav.cwd, name, src, self.nav.show_hidden) {
            Ok(picker) => {
                self.mode = Mode::FolderPicker(picker);
                Effect::None
            }
            Err(e) => self.err(self.lang.op_says(Op::Move, &e.message(self.lang))),
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
        let lang = self.lang;
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
                        err = Some(lang.op_says(Op::Move, &e.message(lang)));
                    }
                }
                KeyInput::Backspace | KeyInput::Left | KeyInput::Char('h') => {
                    match picker.go_up() {
                        Ok(true) => {}
                        Ok(false) => {
                            info = Some(lang.at_filesystem_root().to_string());
                        }
                        Err(e) => err = Some(lang.op_says(Op::Move, &e.message(lang))),
                    }
                }
                KeyInput::Enter | KeyInput::Char('m') => select = true,
                KeyInput::Esc | KeyInput::Char('q') => cancel = true,
                _ => {}
            }
        }
        if cancel {
            self.mode = Mode::Browse;
            self.push_msg(Level::Info, lang.cancelled_folder_picker().to_string());
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
            Err(e) => return self.err(self.lang.op_says(Op::Move, &e)),
        };
        let dst = match fsops::canonical_move_target(
            &self.nav.cwd,
            destination,
            &name,
            self.home.as_deref(),
        ) {
            Ok(d) => d,
            Err(e) => return self.err(self.lang.op_says(Op::Move, &e.message(self.lang))),
        };
        if src == dst {
            let text = self.lang.move_same_place().to_string();
            return self.err(text);
        }
        if std::fs::symlink_metadata(&dst).is_ok() && !fsops::same_file(&src, &dst) {
            let text = self
                .lang
                .op_says(Op::Move, &FsError::AlreadyExists(dst).message(self.lang));
            return self.err(text);
        }
        let src_is_dir = std::fs::symlink_metadata(&src)
            .map(|m| m.is_dir())
            .unwrap_or(false);
        if src_is_dir && dst.starts_with(&src) {
            let text = self.lang.move_into_itself().to_string();
            return self.err(text);
        }
        let op = PendingOp::Move { src, dst };
        let line = self.lang.confirm_line(&op.describe(self.lang));
        self.push_msg(Level::Info, line);
        self.pending = Some(op);
        self.mode = Mode::ConfirmOp;
        Effect::None
    }

    fn cmd_rename(&mut self, new_name: &str) -> Effect {
        let (name, src) = match self.selected_operand() {
            Ok(v) => v,
            Err(e) => return self.err(self.lang.op_says(Op::Rename, &e)),
        };
        if let Err(e) = fsops::validate_new_name(new_name) {
            return self.err(self.lang.op_says(Op::Rename, &e.message(self.lang)));
        }
        if new_name == name {
            let text = self.lang.rename_same_name().to_string();
            return self.err(text);
        }
        let dst = self.nav.cwd.join(new_name);
        if std::fs::symlink_metadata(&dst).is_ok() && !fsops::same_file(&src, &dst) {
            let text = self
                .lang
                .op_says(Op::Rename, &FsError::AlreadyExists(dst).message(self.lang));
            return self.err(text);
        }
        let op = PendingOp::Rename { src, dst };
        let line = self.lang.confirm_line(&op.describe(self.lang));
        self.push_msg(Level::Info, line);
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
            Err(e) => return self.err(self.lang.op_says(Op::Delete, &e)),
        };
        if let Err(e) = trash::check_trashable(&src) {
            return self.err(self.lang.op_says(Op::Delete, &e.message(self.lang)));
        }
        if let Err(e) = std::fs::symlink_metadata(&src) {
            let text = self
                .lang
                .op_says(Op::Delete, &fsops::io_error(&src, &e).message(self.lang));
            return self.err(text);
        }
        let op = PendingOp::Trash { src, name };
        let line = self.lang.confirm_line(&op.describe(self.lang));
        self.push_msg(Level::Info, line);
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
            return self.err(self.lang.op_says(Op::Delete, &e.message(self.lang)));
        }
        match self.trasher.trash(src) {
            Ok(()) => {
                let where_to = self.trasher.destination(self.lang);
                let done = self.lang.trashed(name, &where_to);
                self.push_msg(Level::Ok, done);
                if let Err(e) = self.nav.refresh() {
                    return self.err(e.message(self.lang));
                }
                Effect::None
            }
            Err(e) => {
                let _ = self.nav.refresh();
                self.err(self.lang.op_says(Op::Delete, &e.message(self.lang)))
            }
        }
    }

    fn perform_pending(&mut self) -> Effect {
        self.mode = Mode::Browse;
        let Some(op) = self.pending.take() else {
            let text = self.lang.nothing_to_confirm().to_string();
            return self.err(text);
        };
        let renaming = matches!(op, PendingOp::Rename { .. });
        let (src, dst) = match &op {
            PendingOp::Move { src, dst } | PendingOp::Rename { src, dst } => {
                (src.clone(), dst.clone())
            }
            PendingOp::Trash { src, name } => {
                let (src, name) = (src.clone(), name.clone());
                return self.perform_trash(&src, &name);
            }
        };
        match fsops::safe_move(&src, &dst) {
            Ok(()) => {
                let (from, to) = (src.display().to_string(), dst.display().to_string());
                let done = if renaming {
                    self.lang.renamed(&from, &to)
                } else {
                    self.lang.moved(&from, &to)
                };
                self.push_msg(Level::Ok, done);
                if let Err(e) = self.nav.refresh() {
                    return self.err(e.message(self.lang));
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
                self.err(e.message(self.lang))
            }
        }
    }

    fn cmd_open(&mut self) -> Effect {
        let (name, path) = match self.selected_operand() {
            Ok(v) => v,
            Err(e) => return self.err(self.lang.op_says(Op::Open, &e)),
        };
        if !cfg!(target_os = "macos") {
            let text = self.lang.open_macos_only().to_string();
            return self.err(text);
        }
        let note = self.lang.opening_with_macos(&name);
        self.push_msg(Level::Ok, note);
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
            Err(e) => return self.err(self.lang.op_says(Op::Edit, &e)),
        };
        let Some(entry) = self.nav.selected() else {
            return self.err(self.lang.op_says(Op::Edit, self.lang.nothing_selected()));
        };
        if !entry.is_file_like() {
            return self.err(self.lang.not_a_regular_file(&name));
        }
        let argv = editor::build_edit_command(self.editor_env.as_deref(), &path);
        let note = self.lang.opening_in_editor(&name, &argv[0]);
        self.push_msg(Level::Ok, note);
        Effect::RunInteractive { argv }
    }

    fn cmd_preview(&mut self) -> Effect {
        let (name, path) = match self.selected_operand() {
            Ok(v) => v,
            Err(e) => return self.err(self.lang.op_says(Op::Preview, &e)),
        };
        let Some(entry) = self.nav.selected() else {
            return self.err(self.lang.op_says(Op::Preview, self.lang.nothing_selected()));
        };
        if entry.is_file_like() && self.nvim_on_path {
            match preview::sniff(&path) {
                Ok(sample) if !sample.is_empty() && preview::is_probably_text(&sample) => {
                    let argv = editor::build_preview_command(&path);
                    let note = self.lang.opening_preview(&name);
                    self.push_msg(Level::Ok, note);
                    return Effect::RunInteractive { argv };
                }
                Ok(_) => {}
                Err(e) => return self.err(self.lang.op_says(Op::Preview, &e.message(self.lang))),
            }
        }
        match preview::build_preview(&path, self.lang) {
            Ok(PreviewData { title, lines }) => {
                let heading = self.lang.preview_title(&title);
                self.mode = Mode::Pager(Pager::plain(heading, lines));
                Effect::None
            }
            Err(e) => self.err(self.lang.op_says(Op::Preview, &e.message(self.lang))),
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
        let note = self.lang.agent_disabled().to_string();
        self.push_msg(Level::Info, note);
        self.mode = Mode::Pager(Pager::plain(self.lang.agent_title(), reply.lines));
        Effect::None
    }

    fn show_help(&mut self) -> Effect {
        self.mode = Mode::Pager(Pager::plain(self.lang.help_title(), help_lines(self.lang)));
        Effect::None
    }

    /// `:lang` / `:language` - the screen's language.
    ///
    /// With no code it reports the current one, so a user who cannot
    /// read the screen still has a way to find out what to type. With a
    /// code it switches every phrase at the next frame and writes the
    /// choice down, and a preference that could not be written down is
    /// said out loud rather than swallowed: the session is still in the
    /// new language, and the user knows the next one will not be.
    fn cmd_language(&mut self, code: Option<&str>) -> Effect {
        let Some(code) = code else {
            let line = self.lang.language_is(self.lang.endonym(), self.lang.code());
            self.push_msg(Level::Info, line);
            return Effect::None;
        };
        let Some(lang) = Lang::parse(code) else {
            let codes = Lang::ALL
                .iter()
                .map(|l| format!("{} ({})", l.code(), l.endonym()))
                .collect::<Vec<_>>()
                .join(", ");
            return self.err(self.lang.unknown_language(code, &codes));
        };
        // The confirmation is already in the new language: it is the
        // first evidence that the switch took.
        self.lang = lang;
        let set = lang.language_set(lang.endonym(), lang.code());
        self.push_msg(Level::Ok, set);
        match &self.config_path {
            Some(path) => {
                let path = path.clone();
                match config::save(&path, lang) {
                    Ok(()) => {
                        let saved = lang.language_saved(&path.display().to_string());
                        self.push_msg(Level::Info, saved);
                    }
                    Err(e) => {
                        let note = lang.language_not_saved(&e.to_string());
                        self.push_msg(Level::Error, note);
                    }
                }
            }
            None => {
                let note = lang.language_not_saved(lang.fs_home_not_found());
                self.push_msg(Level::Error, note);
            }
        }
        Effect::None
    }
}

/// The full help text, shared by the `?` key and the `help` command.
///
/// A thin forward to [`Lang::help_lines`]: the words live with every
/// other phrase, and this is the name the rest of the crate knows.
pub fn help_lines(lang: Lang) -> Vec<String> {
    lang.help_lines()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::i18n::Lang;
    use crate::summarize::Failure;
    use std::fs;
    use std::sync::Mutex;

    fn app_in(tmp: &tempfile::TempDir) -> App {
        let nav = NavState::new(tmp.path()).unwrap();
        App::new(nav, None, false, None, Lang::En)
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

    /// A summary run that never spawns anything: it reports whatever the
    /// test told it to, and records whether it was terminated.
    #[derive(Clone, Default)]
    struct ScriptedJob {
        outcome: Option<Outcome>,
        terminated: std::sync::Arc<std::sync::atomic::AtomicBool>,
    }

    impl Job for ScriptedJob {
        fn poll(&mut self) -> Option<Outcome> {
            self.outcome.take()
        }
        fn terminate(&mut self) {
            self.terminated
                .store(true, std::sync::atomic::Ordering::SeqCst);
        }
    }

    /// The [`Runner`] seam, scripted. It records every spec it was asked
    /// to start, so the argv, the prompt, and the output path a real run
    /// would have used are all assertable without an AI CLI installed.
    ///
    /// It also keeps the log handle it was handed, which is how a test
    /// makes a run "print" something: the app cannot tell a line written
    /// here from a line a child process wrote.
    #[derive(Clone, Default)]
    struct FakeRunner {
        started: std::sync::Arc<Mutex<Vec<JobSpec>>>,
        terminated: std::sync::Arc<std::sync::atomic::AtomicBool>,
        streams: std::sync::Arc<Mutex<Vec<stream::Handle>>>,
        outcome: Option<Outcome>,
        fail: Option<Failure>,
    }

    impl Runner for FakeRunner {
        fn start(&self, spec: &JobSpec, live: &stream::Handle) -> Result<Box<dyn Job>, Failure> {
            if let Some(reason) = &self.fail {
                return Err(reason.clone());
            }
            self.started.lock().unwrap().push(spec.clone());
            self.streams.lock().unwrap().push(live.clone());
            Ok(Box::new(ScriptedJob {
                outcome: self.outcome.clone(),
                terminated: std::sync::Arc::clone(&self.terminated),
            }))
        }
    }

    impl FakeRunner {
        /// The log of the run it most recently started.
        fn live(&self) -> stream::Handle {
            self.streams
                .lock()
                .unwrap()
                .last()
                .cloned()
                .expect("no run was started")
        }
    }

    /// An app whose summary runner is scripted, so `:summarize` runs end
    /// to end without an AI CLI on `$PATH` and without a network.
    fn app_with_runner(tmp: &tempfile::TempDir, runner: FakeRunner) -> (App, FakeRunner) {
        let mut app = app_in(tmp);
        app.runner = Box::new(runner.clone());
        (app, runner)
    }

    fn docs_fixture() -> tempfile::TempDir {
        let tmp = tempfile::tempdir().unwrap();
        fs::create_dir(tmp.path().join("deep")).unwrap();
        fs::write(tmp.path().join("report.pdf"), "%PDF-1.4").unwrap();
        fs::write(tmp.path().join("notes.md"), "# notes").unwrap();
        fs::write(tmp.path().join("log.txt"), "log").unwrap();
        fs::write(tmp.path().join("photo.png"), "png").unwrap();
        fs::write(tmp.path().join("deep/inner.markdown"), "# inner").unwrap();
        tmp
    }

    /// Focus a row in the open file selector by name.
    fn focus_row(app: &mut App, name: &str) {
        let Mode::FileSelector(selector) = &mut app.mode else {
            panic!("expected the file selector");
        };
        selector.cursor = selector
            .entries
            .iter()
            .position(|e| e.name == name)
            .unwrap_or_else(|| panic!("row '{name}' not in the selector"));
    }

    fn selector(app: &App) -> &FileSelector {
        let Mode::FileSelector(selector) = &app.mode else {
            panic!("expected the file selector");
        };
        selector
    }

    #[test]
    fn summarize_opens_the_selector_from_the_key_and_both_commands() {
        let tmp = docs_fixture();
        let mut app = app_in(&tmp);
        assert_eq!(app.handle_key(KeyInput::Char('S')), Effect::None);
        assert!(matches!(app.mode, Mode::FileSelector(_)));

        for line in ["summarize", "summary"] {
            let mut app = app_in(&tmp);
            assert_eq!(app.execute_line(line), Effect::None);
            assert!(
                matches!(app.mode, Mode::FileSelector(_)),
                "'{line}' must open the selector"
            );
        }
    }

    #[test]
    fn the_selector_lists_documents_and_folders_only() {
        let tmp = docs_fixture();
        let mut app = app_in(&tmp);
        app.handle_key(KeyInput::Char('S'));
        let listed: Vec<String> = selector(&app)
            .entries
            .iter()
            .map(|e| e.name.clone())
            .collect();
        for shown in ["report.pdf", "notes.md", "log.txt", "deep"] {
            assert!(
                listed.contains(&shown.to_string()),
                "{shown} should be listed"
            );
        }
        assert!(!listed.contains(&"photo.png".to_string()));
    }

    #[test]
    fn space_marks_files_across_directories_and_the_count_is_shown() {
        let tmp = docs_fixture();
        let mut app = app_in(&tmp);
        app.handle_key(KeyInput::Char('S'));

        focus_row(&mut app, "notes.md");
        app.handle_key(KeyInput::Char(' '));
        assert_eq!(selector(&app).count(), 1);
        assert!(last_msg(&app).text.contains("selected 'notes.md'"));
        assert_eq!(selector(&app).header_line(Lang::En), "selected: 1 file");

        focus_row(&mut app, "report.pdf");
        app.handle_key(KeyInput::Char(' '));
        assert_eq!(selector(&app).header_line(Lang::En), "selected: 2 files");

        // Down into a folder, and the selection comes along.
        focus_row(&mut app, "deep");
        app.handle_key(KeyInput::Char('l'));
        focus_row(&mut app, "inner.markdown");
        app.handle_key(KeyInput::Char(' '));
        assert_eq!(selector(&app).count(), 3);

        // Space again on the same row takes it back off.
        app.handle_key(KeyInput::Char(' '));
        assert_eq!(selector(&app).count(), 2);
        assert!(last_msg(&app).text.contains("unselected 'inner.markdown'"));
    }

    #[test]
    fn space_on_a_folder_selects_nothing_and_says_which_files_qualify() {
        let tmp = docs_fixture();
        let mut app = app_in(&tmp);
        app.handle_key(KeyInput::Char('S'));
        focus_row(&mut app, "deep");
        app.handle_key(KeyInput::Char(' '));
        assert_eq!(selector(&app).count(), 0);
        assert_eq!(last_msg(&app).level, Level::Error);
        assert!(last_msg(&app).text.contains(".pdf"));
    }

    #[test]
    fn confirming_an_empty_selection_is_refused_with_the_selector_still_up() {
        let tmp = docs_fixture();
        let mut app = app_in(&tmp);
        app.handle_key(KeyInput::Char('S'));
        assert_eq!(app.handle_key(KeyInput::Enter), Effect::None);
        assert!(matches!(app.mode, Mode::FileSelector(_)));
        assert_eq!(last_msg(&app).level, Level::Error);
        assert!(last_msg(&app).text.contains("no files selected"));
    }

    #[test]
    fn esc_cancels_the_selector_and_starts_nothing() {
        let tmp = docs_fixture();
        let (mut app, runner) = app_with_runner(&tmp, FakeRunner::default());
        app.handle_key(KeyInput::Char('S'));
        focus_row(&mut app, "notes.md");
        app.handle_key(KeyInput::Char(' '));
        assert_eq!(app.handle_key(KeyInput::Esc), Effect::None);
        assert_eq!(app.mode, Mode::Browse);
        assert!(app.job.is_none());
        assert!(runner.started.lock().unwrap().is_empty());
    }

    /// Walk the whole flow with the keys a user presses: `S`, Space,
    /// confirm, then a provider.
    fn run_summary(app: &mut App, files: &[&str], provider_key: KeyInput) {
        app.handle_key(KeyInput::Char('S'));
        for name in files {
            focus_row(app, name);
            app.handle_key(KeyInput::Char(' '));
        }
        app.handle_key(KeyInput::Enter);
        assert!(matches!(app.mode, Mode::ProviderMenu { .. }));
        app.handle_key(provider_key);
    }

    #[test]
    fn enter_at_the_provider_dialog_runs_the_default_provider() {
        let tmp = docs_fixture();
        let (mut app, runner) = app_with_runner(&tmp, FakeRunner::default());
        run_summary(&mut app, &["report.pdf", "notes.md"], KeyInput::Enter);

        let started = runner.started.lock().unwrap();
        assert_eq!(started.len(), 1);
        let spec = &started[0];
        assert_eq!(spec.provider, Provider::Ag);
        assert_eq!(spec.argv()[0], "agy");
        assert_eq!(spec.files.len(), 2);
        assert_eq!(app.mode, Mode::Browse);
        assert!(app.job_active());
    }

    #[test]
    fn a_digit_at_the_provider_dialog_runs_that_provider() {
        let tmp = docs_fixture();
        for (key, expected) in [
            ('1', Provider::Ag),
            ('2', Provider::Cc),
            ('3', Provider::Co),
            ('4', Provider::Gk),
            ('5', Provider::Ki),
        ] {
            let (mut app, runner) = app_with_runner(&tmp, FakeRunner::default());
            run_summary(&mut app, &["notes.md"], KeyInput::Char(key));
            let started = runner.started.lock().unwrap();
            assert_eq!(started.len(), 1, "'{key}' must start exactly one run");
            assert_eq!(started[0].provider, expected);
        }
    }

    #[test]
    fn an_unlisted_digit_starts_nothing_and_leaves_the_dialog_up() {
        let tmp = docs_fixture();
        let (mut app, runner) = app_with_runner(&tmp, FakeRunner::default());
        app.handle_key(KeyInput::Char('S'));
        focus_row(&mut app, "notes.md");
        app.handle_key(KeyInput::Char(' '));
        app.handle_key(KeyInput::Enter);
        app.handle_key(KeyInput::Char('9'));
        assert!(matches!(app.mode, Mode::ProviderMenu { .. }));
        assert!(runner.started.lock().unwrap().is_empty());
        assert_eq!(last_msg(&app).level, Level::Error);
    }

    #[test]
    fn the_summary_lands_beside_the_first_selected_file() {
        let tmp = docs_fixture();
        let (mut app, runner) = app_with_runner(&tmp, FakeRunner::default());
        // Picked in the deep folder first, so that is where it goes.
        app.handle_key(KeyInput::Char('S'));
        focus_row(&mut app, "deep");
        app.handle_key(KeyInput::Char('l'));
        focus_row(&mut app, "inner.markdown");
        app.handle_key(KeyInput::Char(' '));
        app.handle_key(KeyInput::Char('h'));
        focus_row(&mut app, "notes.md");
        app.handle_key(KeyInput::Char(' '));
        app.handle_key(KeyInput::Enter);
        app.handle_key(KeyInput::Enter);

        let started = runner.started.lock().unwrap();
        let spec = &started[0];
        let deep = tmp.path().canonicalize().unwrap().join("deep");
        assert_eq!(spec.output, deep.join("inner-summary.md"));
        assert_eq!(spec.cwd, deep);
    }

    #[test]
    fn a_finished_run_logs_the_summary_and_selects_it_to_be_read() {
        let tmp = docs_fixture();
        let written = tmp.path().canonicalize().unwrap().join("notes-summary.md");
        fs::write(&written, "# summary").unwrap();
        let (mut app, _runner) = app_with_runner(
            &tmp,
            FakeRunner {
                outcome: Some(Outcome::Written(written.clone())),
                ..FakeRunner::default()
            },
        );
        run_summary(&mut app, &["notes.md"], KeyInput::Enter);
        assert!(app.job_active());

        app.poll_job();
        assert!(!app.job_active());
        let text: Vec<String> = app.messages.iter().map(|m| m.text.clone()).collect();
        assert!(
            text.iter()
                .any(|t| t == &format!("summary written to {}", written.display())),
            "expected an ok line naming the summary, got {text:?}"
        );
        assert_eq!(
            app.nav.selected().unwrap().name,
            "notes-summary.md",
            "the summary should be under the cursor, ready for l"
        );
    }

    #[test]
    fn a_cross_directory_summary_moves_the_listing_and_focuses_the_file() {
        let tmp = docs_fixture();
        let deep = tmp.path().canonicalize().unwrap().join("deep");
        let written = deep.join("inner-summary.md");
        fs::write(&written, "# summary").unwrap();
        let (mut app, _runner) = app_with_runner(
            &tmp,
            FakeRunner {
                outcome: Some(Outcome::Written(written.clone())),
                ..FakeRunner::default()
            },
        );

        app.handle_key(KeyInput::Char('S'));
        focus_row(&mut app, "deep");
        app.handle_key(KeyInput::Char('l'));
        focus_row(&mut app, "inner.markdown");
        app.handle_key(KeyInput::Char(' '));
        app.handle_key(KeyInput::Char('h'));
        app.handle_key(KeyInput::Enter);
        app.handle_key(KeyInput::Enter);
        app.poll_job();

        assert_eq!(app.nav.cwd, deep);
        assert_eq!(app.nav.selected().unwrap().name, "inner-summary.md");
        assert!(app.messages.iter().any(|message| {
            message.text == format!("listing moved to {}", app.nav.cwd.display())
        }));
        assert_eq!(last_msg(&app).text, "press l to read it");
    }

    #[test]
    fn a_failed_run_is_reported_and_clears_the_job() {
        let tmp = docs_fixture();
        let (mut app, _runner) = app_with_runner(
            &tmp,
            FakeRunner {
                outcome: Some(Outcome::Failed(Failure::Provider(
                    "agy: not logged in".to_string(),
                ))),
                ..FakeRunner::default()
            },
        );
        run_summary(&mut app, &["notes.md"], KeyInput::Enter);
        app.poll_job();
        assert!(!app.job_active());
        assert_eq!(last_msg(&app).level, Level::Error);
        assert!(last_msg(&app).text.contains("not logged in"));
    }

    #[test]
    fn a_provider_that_will_not_start_is_an_error_not_a_job() {
        let tmp = docs_fixture();
        let (mut app, _runner) = app_with_runner(
            &tmp,
            FakeRunner {
                fail: Some(Failure::Spawn {
                    program: "agy".to_string(),
                    detail: "No such file or directory".to_string(),
                }),
                ..FakeRunner::default()
            },
        );
        run_summary(&mut app, &["notes.md"], KeyInput::Enter);
        assert!(!app.job_active());
        assert_eq!(app.mode, Mode::Browse);
        assert_eq!(last_msg(&app).level, Level::Error);
        assert!(last_msg(&app).text.contains("could not run 'agy'"));
    }

    #[test]
    fn only_one_summary_runs_at_a_time() {
        let tmp = docs_fixture();
        let (mut app, runner) = app_with_runner(&tmp, FakeRunner::default());
        run_summary(&mut app, &["notes.md"], KeyInput::Enter);
        assert_eq!(app.handle_key(KeyInput::Char('S')), Effect::None);
        assert_eq!(app.mode, Mode::Browse);
        assert_eq!(last_msg(&app).level, Level::Error);
        assert!(last_msg(&app).text.contains("already running"));
        assert_eq!(runner.started.lock().unwrap().len(), 1);
    }

    #[test]
    fn the_status_line_names_the_run_while_it_is_going() {
        let tmp = docs_fixture();
        let (mut app, _runner) = app_with_runner(&tmp, FakeRunner::default());
        assert_eq!(app.job_status(), None);
        run_summary(&mut app, &["report.pdf", "notes.md"], KeyInput::Enter);
        assert_eq!(
            app.job_status(),
            Some("[AI: summarizing 2 files with agy]".to_string())
        );
    }

    #[test]
    fn quitting_with_a_summary_running_asks_first_and_only_y_leaves() {
        let tmp = docs_fixture();
        for quit_key in [KeyInput::Char('q'), KeyInput::CtrlC] {
            let (mut app, runner) = app_with_runner(&tmp, FakeRunner::default());
            run_summary(&mut app, &["notes.md"], KeyInput::Enter);

            assert_eq!(app.handle_key(quit_key), Effect::None, "{quit_key:?}");
            assert_eq!(app.mode, Mode::ConfirmQuit);
            assert!(
                last_msg(&app).text.contains("quit and terminate"),
                "the log must name the run: {}",
                last_msg(&app).text
            );
            assert!(!runner.terminated.load(std::sync::atomic::Ordering::SeqCst));

            // Enter is not an answer, and neither is any other stray key.
            assert_eq!(app.handle_key(KeyInput::Enter), Effect::None);
            assert_eq!(app.mode, Mode::ConfirmQuit);
            assert!(app.job_active());

            // `n` keeps the run and the session.
            assert_eq!(app.handle_key(KeyInput::Char('n')), Effect::None);
            assert_eq!(app.mode, Mode::Browse);
            assert!(app.job_active());
            assert!(!runner.terminated.load(std::sync::atomic::Ordering::SeqCst));

            // `y` terminates the child and leaves.
            app.handle_key(quit_key);
            assert_eq!(app.handle_key(KeyInput::Char('y')), Effect::Quit);
            assert!(!app.job_active());
            assert!(runner.terminated.load(std::sync::atomic::Ordering::SeqCst));
        }
    }

    #[test]
    fn esc_at_the_quit_prompt_keeps_the_run_going() {
        let tmp = docs_fixture();
        let (mut app, runner) = app_with_runner(&tmp, FakeRunner::default());
        run_summary(&mut app, &["notes.md"], KeyInput::Enter);
        app.handle_key(KeyInput::Char('q'));
        assert_eq!(app.handle_key(KeyInput::Esc), Effect::None);
        assert_eq!(app.mode, Mode::Browse);
        assert!(app.job_active());
        assert!(!runner.terminated.load(std::sync::atomic::Ordering::SeqCst));
    }

    #[test]
    fn quit_is_immediate_with_nothing_running() {
        let tmp = docs_fixture();
        let (mut app, _runner) = app_with_runner(&tmp, FakeRunner::default());
        assert_eq!(app.handle_key(KeyInput::Char('q')), Effect::Quit);
        assert_eq!(app.handle_key(KeyInput::CtrlC), Effect::Quit);
        assert_eq!(app.execute_line("quit"), Effect::Quit);
    }

    #[test]
    fn the_quit_command_also_asks_while_a_summary_runs() {
        let tmp = docs_fixture();
        let (mut app, _runner) = app_with_runner(&tmp, FakeRunner::default());
        run_summary(&mut app, &["notes.md"], KeyInput::Enter);
        assert_eq!(app.execute_line("quit"), Effect::None);
        assert_eq!(app.mode, Mode::ConfirmQuit);
    }

    /// A run that fails is where a half-finished localization shows: the
    /// screen says why in the user's language, and the one part that is
    /// evidence rather than prose - the provider's own last line - comes
    /// through exactly as the provider said it.
    #[test]
    fn a_failed_run_is_reported_in_the_screens_language() {
        let tmp = docs_fixture();
        let (mut app, _runner) = app_with_runner(
            &tmp,
            FakeRunner {
                outcome: Some(Outcome::Failed(Failure::NoOutput)),
                ..FakeRunner::default()
            },
        );
        app.lang = Lang::ZhTw;
        run_summary(&mut app, &["notes.md"], KeyInput::Enter);
        app.poll_job();
        assert_eq!(last_msg(&app).level, Level::Error);
        assert_eq!(last_msg(&app).text, "摘要: AI 模型沒有輸出任何內容");

        let (mut app, _runner) = app_with_runner(
            &tmp,
            FakeRunner {
                outcome: Some(Outcome::Failed(Failure::Provider(
                    "agy: not logged in".to_string(),
                ))),
                ..FakeRunner::default()
            },
        );
        app.lang = Lang::ZhTw;
        run_summary(&mut app, &["notes.md"], KeyInput::Enter);
        app.poll_job();
        assert_eq!(last_msg(&app).text, "摘要: agy: not logged in");
    }

    #[test]
    fn a_run_that_could_not_start_says_why_in_the_screens_language() {
        let tmp = docs_fixture();
        let (mut app, _runner) = app_with_runner(
            &tmp,
            FakeRunner {
                fail: Some(Failure::Spawn {
                    program: "agy".to_string(),
                    detail: "No such file or directory".to_string(),
                }),
                ..FakeRunner::default()
            },
        );
        app.lang = Lang::ZhTw;
        run_summary(&mut app, &["notes.md"], KeyInput::Enter);
        assert_eq!(last_msg(&app).level, Level::Error);
        assert_eq!(
            last_msg(&app).text,
            "摘要: 無法執行 'agy'：No such file or directory"
        );
        // The same failure, in English, is what the Markdown note in the
        // reserved file would carry.
        assert_eq!(
            crate::summarize::failure_note(&Failure::Spawn {
                program: "agy".to_string(),
                detail: "No such file or directory".to_string(),
            }),
            "# Summary failed\n\ncould not run 'agy': No such file or directory\n"
        );
    }

    /// `:lang` with no code: a user who cannot read the screen still
    /// has a way to find out what to type.
    #[test]
    fn lang_with_no_code_reports_the_current_language() {
        let tmp = tempfile::tempdir().unwrap();
        let mut app = app_in(&tmp);
        assert_eq!(app.execute_line("lang"), Effect::None);
        assert_eq!(app.lang, Lang::En);
        let text = &last_msg(&app).text;
        assert!(text.contains("English"), "{text}");
        assert!(text.contains("(en)"), "{text}");
    }

    #[test]
    fn lang_switches_the_whole_screen_and_says_so_in_the_new_language() {
        let tmp = tempfile::tempdir().unwrap();
        let mut app = app_in(&tmp);
        assert_eq!(app.execute_line("lang zh"), Effect::None);
        assert_eq!(app.lang, Lang::ZhTw);
        // The confirmation is the first evidence that the switch took,
        // so it is already in the language that was asked for.
        let confirmed = app
            .messages
            .iter()
            .any(|m| m.text.contains("語言已設定為 繁體中文 (zh-TW)"));
        assert!(confirmed, "{:?}", app.messages);
        // And so is everything the screen goes on to say.
        app.execute_line("help");
        let Mode::Pager(pager) = &app.mode else {
            panic!("expected the help reader");
        };
        assert_eq!(pager.title, "說明");
        assert!(pager.text().contains("鍵盤優先"));
    }

    #[test]
    fn language_is_the_alias_and_every_spelling_of_the_code_is_taken() {
        for code in ["zh", "zh-TW", "zh_TW", "ZH-tw", "zh-Hant"] {
            let tmp = tempfile::tempdir().unwrap();
            let mut app = app_in(&tmp);
            app.execute_line(&format!("language {code}"));
            assert_eq!(app.lang, Lang::ZhTw, "'{code}' should select zh-TW");
        }
        let tmp = tempfile::tempdir().unwrap();
        let mut app = app_in(&tmp);
        app.lang = Lang::ZhTw;
        app.execute_line("lang en");
        assert_eq!(app.lang, Lang::En);
    }

    #[test]
    fn an_unknown_language_changes_nothing_and_lists_the_ones_there_are() {
        let tmp = tempfile::tempdir().unwrap();
        let mut app = app_in(&tmp);
        assert_eq!(app.execute_line("lang klingon"), Effect::None);
        assert_eq!(app.lang, Lang::En, "an unknown code must change nothing");
        let text = &last_msg(&app).text;
        assert_eq!(last_msg(&app).level, Level::Error);
        assert!(text.contains("klingon"), "{text}");
        for lang in Lang::ALL {
            assert!(text.contains(lang.code()), "{text}");
            assert!(text.contains(lang.endonym()), "{text}");
        }
    }

    #[test]
    fn a_language_change_is_written_down_for_next_time() {
        let tmp = tempfile::tempdir().unwrap();
        let config = tmp.path().join("cfg").join("config.toml");
        let mut app = app_in(&tmp);
        app.config_path = Some(config.clone());

        app.execute_line("lang zh-TW");
        let text = std::fs::read_to_string(&config).expect("the preference was not saved");
        assert_eq!(crate::config::read_language(&text), Some("zh-TW"));
        assert!(
            app.messages.iter().any(|m| m.text.contains("已儲存至")),
            "{:?}",
            app.messages
        );

        // Switching back rewrites the same file rather than appending.
        app.execute_line("lang en");
        let text = std::fs::read_to_string(&config).unwrap();
        assert_eq!(crate::config::read_language(&text), Some("en"));
        assert_eq!(text.matches("language =").count(), 1, "{text}");
    }

    /// A preference that could not be written down is said out loud: the
    /// session is in the new language, and the user knows the next one
    /// will not be.
    #[test]
    fn a_language_change_that_cannot_be_saved_says_so_rather_than_pretending() {
        let tmp = tempfile::tempdir().unwrap();
        let mut app = app_in(&tmp);
        assert_eq!(app.config_path, None);
        app.execute_line("lang zh");
        assert_eq!(app.lang, Lang::ZhTw, "the session still switched");
        assert_eq!(last_msg(&app).level, Level::Error);
        assert!(last_msg(&app).text.contains("僅套用於本次執行"));
    }

    #[test]
    fn lang_takes_at_most_one_code() {
        let tmp = tempfile::tempdir().unwrap();
        let mut app = app_in(&tmp);
        app.execute_line("lang en zh");
        assert_eq!(app.lang, Lang::En);
        assert_eq!(last_msg(&app).level, Level::Error);
        assert!(last_msg(&app).text.starts_with("usage: lang"));
    }

    /// Switching language is a screen change and nothing else: it moves
    /// no file, and it does not touch a run in flight.
    #[test]
    fn switching_language_touches_nothing_but_the_screen() {
        let tmp = docs_fixture();
        let can = tempfile::tempdir().unwrap();
        let before = snapshot(tmp.path());
        let mut app = app_with_can(&tmp, &can);
        let (cwd, cursor) = (app.nav.cwd.clone(), app.nav.cursor);
        app.execute_line("lang zh");
        app.execute_line("lang en");
        assert_eq!(snapshot(tmp.path()), before);
        assert!(can_contents(&can).is_empty());
        assert_eq!(app.nav.cwd, cwd);
        assert_eq!(app.nav.cursor, cursor);
        assert_eq!(app.mode, Mode::Browse);
        assert!(app.pending.is_none());
        assert!(app.job.is_none(), "no key or command here starts a run");
    }

    /// Everything the message log says after a switch is in the new
    /// language: the log is a record, so lines written before it stay as
    /// they were said.
    #[test]
    fn a_switch_changes_what_is_said_next_not_what_was_already_said() {
        let tmp = docs_fixture();
        let mut app = app_in(&tmp);
        app.handle_key(KeyInput::Char('.'));
        assert!(last_msg(&app).text.contains("dotfiles"));
        app.execute_line("lang zh");
        app.handle_key(KeyInput::Char('.'));
        assert!(
            last_msg(&app).text.contains("隱藏檔"),
            "{:?}",
            last_msg(&app)
        );
        assert!(
            app.messages.iter().any(|m| m.text.contains("dotfiles")),
            "the log must keep what it already said"
        );
    }

    #[test]
    fn browse_refresh_errors_use_the_screen_language() {
        for key in [KeyInput::Char('.'), KeyInput::Char('r')] {
            let tmp = tempfile::tempdir().unwrap();
            let mut app = app_in(&tmp);
            app.lang = Lang::ZhTw;
            let file = tmp.path().join("not-a-directory");
            fs::write(&file, "plain file").unwrap();
            app.nav.cwd = file;

            app.handle_key(key);

            assert_eq!(last_msg(&app).level, Level::Error);
            assert!(last_msg(&app).text.contains("不是目錄"), "{:?}", last_msg(&app));
        }
    }

    #[test]
    fn the_help_documents_every_key_the_summarizer_adds() {
        let help = help_lines(Lang::En).join("\n");
        for needed in [
            "S ",
            "Space",
            "Enter, c",
            "1 - 5",
            "summarize, summary",
            "terminate the summary and quit",
        ] {
            assert!(help.contains(needed), "help must document '{needed}'");
        }
    }

    /// The open log viewer.
    fn log_pane(app: &App) -> &LogPane {
        let Mode::JobLog(pane) = &app.mode else {
            panic!("expected the log viewer, got {:?}", app.mode);
        };
        pane
    }

    /// The rows the open log viewer would draw, gutters and all.
    fn log_lines(app: &App) -> Vec<String> {
        log_pane(app)
            .pager
            .rows(app.log_cols(), &app.glyphs)
            .iter()
            .map(|row| row.text().trim_end().to_string())
            .collect()
    }

    /// One frame of the event loop: the geometry is mirrored in, which is
    /// also when an open log viewer re-reads the run.
    fn frame(app: &mut App) {
        let (rows, cols) = (app.viewport_rows, app.viewport_cols);
        app.set_viewport(rows, cols);
    }

    #[test]
    fn the_log_viewer_says_there_is_nothing_to_watch_before_a_run() {
        let tmp = docs_fixture();
        let mut app = app_in(&tmp);
        assert_eq!(app.handle_key(KeyInput::Char('L')), Effect::None);
        assert_eq!(app.mode, Mode::Browse);
        assert_eq!(last_msg(&app).level, Level::Error);
        assert!(last_msg(&app).text.contains("no AI summary has run yet"));
    }

    #[test]
    fn the_key_and_both_commands_open_the_running_providers_output() {
        let tmp = docs_fixture();
        for open_it in [":L", "log", "job"] {
            let (mut app, runner) = app_with_runner(&tmp, FakeRunner::default());
            run_summary(&mut app, &["notes.md"], KeyInput::Enter);
            runner
                .live()
                .append(stream::Origin::Out, "reading notes.md\n");
            if let Some(line) = open_it.strip_prefix(':') {
                app.handle_key(KeyInput::Char(line.chars().next().unwrap()));
            } else {
                app.execute_line(open_it);
            }
            assert_eq!(
                log_lines(&app),
                vec!["    1 | reading notes.md".to_string()],
                "'{open_it}' did not open the log"
            );
            assert!(app.job_active(), "'{open_it}' disturbed the run");
        }
    }

    /// The point of the whole pane: output printed while it is open
    /// reaches the screen on the next frame, with no key pressed.
    #[test]
    fn output_printed_while_the_pane_is_open_arrives_without_a_keypress() {
        let tmp = docs_fixture();
        let (mut app, runner) = app_with_runner(&tmp, FakeRunner::default());
        run_summary(&mut app, &["notes.md"], KeyInput::Enter);
        app.handle_key(KeyInput::Char('L'));
        assert_eq!(log_lines(&app), vec!["(no output yet)".to_string()]);
        assert_eq!(log_pane(&app).activity(), stream::Activity::Waiting);

        let live = runner.live();
        live.append(stream::Origin::Out, "thinking about it\n");
        live.append(stream::Origin::Err, "session id: 01a04eef-d4a6\n");
        frame(&mut app);
        assert_eq!(
            log_lines(&app),
            vec![
                "    1 | thinking about it".to_string(),
                "    2 ! session id: 01a04eef-d4a6".to_string(),
            ]
        );
        assert_eq!(log_pane(&app).activity(), stream::Activity::Streaming);
        assert_eq!(log_pane(&app).session(), Some("01a04eef-d4a6"));
    }

    /// The header is what makes the session reachable outside Filecraft:
    /// it names the provider's own reopen command, never one Filecraft
    /// would run itself.
    #[test]
    fn the_header_names_the_session_and_how_to_reopen_it() {
        let tmp = docs_fixture();
        let (mut app, runner) = app_with_runner(&tmp, FakeRunner::default());
        // `3` is codex, whose banner really does announce a session.
        run_summary(&mut app, &["notes.md"], KeyInput::Char('3'));
        runner
            .live()
            .append(stream::Origin::Err, "session id: 01a04eef-d4a6\n");
        app.handle_key(KeyInput::Char('L'));
        let [top, bottom] = log_pane(&app).header(&app.glyphs, Lang::En);
        assert!(top.starts_with("codex "), "{top:?}");
        assert_eq!(
            bottom,
            "session 01a04eef-d4a6 · resume: codex resume 01a04eef-d4a6"
        );
    }

    #[test]
    fn the_log_viewer_scrolls_with_the_readers_own_keys() {
        let tmp = docs_fixture();
        let (mut app, runner) = app_with_runner(&tmp, FakeRunner::default());
        app.viewport_rows = 14;
        app.viewport_cols = 60;
        run_summary(&mut app, &["notes.md"], KeyInput::Enter);
        let live = runner.live();
        for i in 1..=80 {
            live.append(stream::Origin::Out, &format!("line {i}\n"));
        }
        app.handle_key(KeyInput::Char('L'));

        // It opens at the newest output, following it.
        let view = app.log_rows();
        assert!(log_pane(&app).follow);
        assert_eq!(log_pane(&app).pager.scroll, 80 - view);

        // Scrolling up stops the follow and holds the place.
        app.handle_key(KeyInput::Char('k'));
        assert!(!log_pane(&app).follow);
        assert_eq!(log_pane(&app).pager.scroll, 80 - view - 1);
        app.handle_key(KeyInput::Char('u'));
        assert_eq!(
            log_pane(&app).pager.scroll,
            80 - view - 1 - (view as isize / 2).max(1) as usize
        );
        app.handle_key(KeyInput::Char('g'));
        assert_eq!(log_pane(&app).pager.scroll, 0);

        // New output leaves a reader who scrolled up exactly where it is.
        live.append(stream::Origin::Out, "line 81\n");
        frame(&mut app);
        assert_eq!(log_pane(&app).pager.scroll, 0);

        // `G` goes to the bottom, which is what following is.
        app.handle_key(KeyInput::Char('G'));
        assert!(log_pane(&app).follow);
        live.append(stream::Origin::Out, "line 82\n");
        frame(&mut app);
        assert_eq!(log_pane(&app).pager.scroll, 82 - view);
    }

    /// A committed search jumps the view, and the next frame has to leave
    /// it there. Following is being at the bottom, so a search that
    /// landed somewhere else is not following any more - otherwise the
    /// match is pulled off the screen before it is ever drawn.
    #[test]
    fn a_search_in_the_log_viewer_survives_the_next_frame() {
        let tmp = docs_fixture();
        let (mut app, runner) = app_with_runner(&tmp, FakeRunner::default());
        app.viewport_rows = 14;
        app.viewport_cols = 60;
        run_summary(&mut app, &["notes.md"], KeyInput::Enter);
        let live = runner.live();
        live.append(stream::Origin::Err, "session id: 01a04eef-d4a6\n");
        for i in 2..=80 {
            live.append(stream::Origin::Out, &format!("line {i}\n"));
        }
        app.handle_key(KeyInput::Char('L'));
        assert!(log_pane(&app).follow);

        for key in "/session".chars() {
            app.handle_key(KeyInput::Char(key));
        }
        assert_eq!(app.handle_key(KeyInput::Enter), Effect::None);
        assert_eq!(log_pane(&app).pager.scroll, 0);
        assert!(!log_pane(&app).follow, "the search left the view following");

        frame(&mut app);
        assert_eq!(log_pane(&app).pager.scroll, 0);
        assert!(
            log_lines(&app)[0].contains("session id"),
            "{:?}",
            log_lines(&app)
        );

        // And the run is still streaming into a pane that is no longer
        // pulled down by it.
        live.append(stream::Origin::Out, "line 81\n");
        frame(&mut app);
        assert_eq!(log_pane(&app).pager.scroll, 0);
    }

    #[test]
    fn every_back_out_key_leaves_the_run_alone() {
        let tmp = docs_fixture();
        for key in [
            KeyInput::Char('h'),
            KeyInput::Char('q'),
            KeyInput::Esc,
            KeyInput::Enter,
            KeyInput::Left,
        ] {
            let (mut app, runner) = app_with_runner(&tmp, FakeRunner::default());
            run_summary(&mut app, &["notes.md"], KeyInput::Enter);
            app.handle_key(KeyInput::Char('L'));
            assert_eq!(app.handle_key(key), Effect::None, "{key:?}");
            assert_eq!(app.mode, Mode::Browse, "{key:?} did not close the pane");
            assert!(app.job_active(), "{key:?} ended the run");
            assert!(
                !runner.terminated.load(std::sync::atomic::Ordering::SeqCst),
                "{key:?} terminated the provider"
            );
            // And it is still there to reopen.
            app.handle_key(KeyInput::Char('L'));
            assert!(matches!(app.mode, Mode::JobLog(_)), "{key:?}");
        }
    }

    /// A finished run is exactly the one worth reading afterwards, so its
    /// log outlives it - and says it has finished.
    #[test]
    fn the_log_outlives_the_run_that_wrote_it() {
        let tmp = docs_fixture();
        let output = tmp.path().join("notes-summary.md");
        let (mut app, runner) = app_with_runner(
            &tmp,
            FakeRunner {
                outcome: Some(Outcome::Written(output)),
                ..FakeRunner::default()
            },
        );
        run_summary(&mut app, &["notes.md"], KeyInput::Enter);
        runner
            .live()
            .append(stream::Origin::Out, "wrote the summary\n");
        app.poll_job();
        assert!(!app.job_active());

        app.handle_key(KeyInput::Char('L'));
        assert_eq!(log_pane(&app).activity(), stream::Activity::Ended);
        assert_eq!(
            log_lines(&app),
            vec!["    1 | wrote the summary".to_string()]
        );
    }

    #[test]
    fn a_run_that_could_not_start_still_has_a_log_saying_why() {
        let tmp = docs_fixture();
        let (mut app, _) = app_with_runner(
            &tmp,
            FakeRunner {
                fail: Some(Failure::Spawn {
                    program: "agy".to_string(),
                    detail: "No such file".to_string(),
                }),
                ..FakeRunner::default()
            },
        );
        run_summary(&mut app, &["notes.md"], KeyInput::Enter);
        assert!(!app.job_active());
        app.handle_key(KeyInput::Char('L'));
        assert_eq!(log_pane(&app).activity(), stream::Activity::Ended);
    }

    /// The log viewer's twin of the reader's rule: it is a view, and a
    /// view changes nothing - not the tree, not the run.
    #[test]
    fn no_log_viewer_key_ever_mutates_the_filesystem_or_the_run() {
        let tmp = docs_fixture();
        let before = snapshot(tmp.path());
        let can = tempfile::tempdir().unwrap();
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
            let runner = FakeRunner::default();
            let mut app = app_with_can(&tmp, &can);
            app.runner = Box::new(runner.clone());
            run_summary(&mut app, &["notes.md"], KeyInput::Enter);
            runner.live().append(stream::Origin::Out, "working\n");
            app.handle_key(KeyInput::Char('L'));
            assert!(matches!(app.mode, Mode::JobLog(_)));

            let effect = app.handle_key(key);
            assert_eq!(effect, Effect::None, "{key:?} produced an effect");
            assert!(app.pending.is_none(), "{key:?} armed an operation");
            assert!(app.job_active(), "{key:?} ended the run");
            assert!(
                !runner.terminated.load(std::sync::atomic::Ordering::SeqCst),
                "{key:?} terminated the provider"
            );
            assert_eq!(
                runner.started.lock().unwrap().len(),
                1,
                "{key:?} started a second run"
            );
            assert_eq!(snapshot(tmp.path()), before, "{key:?} changed the tree");
            assert!(can_contents(&can).is_empty(), "{key:?} trashed something");
        }
    }

    #[test]
    fn help_documents_the_log_viewer_and_both_its_commands() {
        let help = help_lines(Lang::En).join("\n");
        for needed in [
            "L ",
            "KEYS (log viewer",
            "log, job",
            "the run keeps going",
            "stdout",
            "stderr",
        ] {
            assert!(help.contains(needed), "help must document '{needed}'");
        }
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
        assert!(pending.describe(Lang::En).contains("note.txt"));
        assert!(pending.describe(Lang::En).contains("archive"));
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
        assert!(picker(&app).dest_line(Lang::En).starts_with("dest: "));
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
        assert!(pending.describe(Lang::En).contains("note.txt"));
        assert!(pending.describe(Lang::En).contains("archive"));
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
        assert!(app
            .pending
            .as_ref()
            .unwrap()
            .describe(Lang::En)
            .contains("docs"));
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
            app.pending.as_ref().unwrap().describe(Lang::En),
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
        assert_eq!(
            app.pending.as_ref().unwrap().describe(Lang::En),
            "trash 'a.txt'"
        );
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
        assert_eq!(
            app.pending.as_ref().unwrap().describe(Lang::En),
            "trash 'a.txt'"
        );
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
    fn enter_never_stands_in_for_y_at_the_delete_prompt() {
        // `d` is half-page-down in the reader and Enter activates a row
        // in browse: the pair is one muscle-memory slip apart, so the
        // trash is answered with the letter and nothing else.
        let tmp = tempfile::tempdir().unwrap();
        let can = tempfile::tempdir().unwrap();
        fs::write(tmp.path().join("a.txt"), "a").unwrap();
        let before = snapshot(tmp.path());
        let mut app = app_with_can(&tmp, &can);
        select(&mut app, "a.txt");

        app.handle_key(KeyInput::Char('d'));
        app.handle_key(KeyInput::Enter);

        assert_eq!(app.mode, Mode::ConfirmOp, "the prompt must stay up");
        assert!(app.pending.is_some(), "the operation must stay armed");
        assert_eq!(snapshot(tmp.path()), before, "Enter trashed the entry");
        assert!(can_contents(&can).is_empty(), "Enter trashed the entry");
        assert!(
            last_msg(&app).text.contains("press y"),
            "{:?}",
            last_msg(&app)
        );

        // And the key that does mean yes still does.
        app.handle_key(KeyInput::Char('y'));
        assert_eq!(can_contents(&can), vec!["a.txt".to_string()]);
    }

    #[test]
    fn enter_still_confirms_a_move_or_rename() {
        let tmp = tempfile::tempdir().unwrap();
        fs::write(tmp.path().join("a.txt"), "a").unwrap();
        fs::create_dir(tmp.path().join("sub")).unwrap();
        let mut app = app_in(&tmp);
        select(&mut app, "a.txt");

        app.execute_line("move sub");
        app.handle_key(KeyInput::Enter);
        assert_eq!(app.mode, Mode::Browse);
        assert!(tmp.path().join("sub/a.txt").exists(), "Enter must confirm");
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
        let help = help_lines(Lang::En).join("\n");
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
        assert!(
            help.contains("not trash"),
            "the help must say Enter does not answer a trash prompt:\n{help}"
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
        let mut app = App::new(
            nav,
            Some("myedit --fast".to_string()),
            false,
            None,
            Lang::En,
        );

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
            Lang::En,
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
        assert!(pager
            .position(width, view, &glyphs, Lang::En)
            .ends_with("100%"));

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
        let help = help_lines(Lang::En).join("\n");
        for key in ["j / k", "d / u", "f / b", "PgDn / PgUp", "g / G", "n / N"] {
            assert!(help.contains(key), "help never mentions {key}");
        }
    }

    #[test]
    fn the_help_documents_the_folder_picker() {
        let help = help_lines(Lang::En).join("\n");
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
        let runner = FakeRunner::default();
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
            app.runner = Box::new(runner.clone());
            select(&mut app, "notes.md");
            app.handle_key(KeyInput::Char('l'));
            assert!(matches!(app.mode, Mode::Pager(_)));
            let effect = app.handle_key(key);
            assert_eq!(effect, Effect::None, "{key:?} produced an effect");
            assert!(app.pending.is_none(), "{key:?} armed an operation");
            assert!(!app.job_active(), "{key:?} started a summary");
            assert_eq!(snapshot(tmp.path()), before, "{key:?} changed the tree");
        }
        assert!(
            runner.started.lock().unwrap().is_empty(),
            "a reader key ran an AI provider"
        );
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
            Lang::En,
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
            Lang::En,
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
        let mut app = App::new(
            NavState::new(&deep).unwrap(),
            None,
            false,
            Some(root),
            Lang::En,
        );
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
        let app = App::new(
            NavState::new(&root).unwrap(),
            None,
            false,
            Some(root),
            Lang::En,
        );
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
        let runner = FakeRunner::default();
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
            app.runner = Box::new(runner.clone());
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
                // `S` opens the file selector, which is as far as any
                // browse key gets: no AI run may start without a
                // selection and a chosen provider.
                assert!(!app.job_active(), "{key:?} started a summary");
            }
            assert_eq!(snapshot(tmp.path()), before, "{key:?} changed the tree");
            assert!(
                can_contents(&can).is_empty(),
                "{key:?} trashed something without a confirmation"
            );
            assert!(
                runner.started.lock().unwrap().is_empty(),
                "{key:?} ran an AI provider without a confirmed selection"
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
        let mut app = App::new(nav, None, true, None, Lang::En);
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
        let mut app = App::new(nav, None, true, None, Lang::En);
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
        let mut app = App::new(nav, None, false, Some(home.clone()), Lang::En);
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
