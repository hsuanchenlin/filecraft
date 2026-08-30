//! Filecraft entry point: argument handling, TTY detection, the terminal
//! event loop, and interpretation of [`Effect`]s (running editors,
//! spawning macOS `open`). Everything decision-shaped lives in the
//! library; this file is deliberately thin.

use std::io::{self, IsTerminal, Write};
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use ratatui::crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};

use filecraft::app::{App, Effect, KeyInput, Level};
use filecraft::cli::{self, CliAction};
use filecraft::columns::ColumnSet;
use filecraft::config;
use filecraft::editor;
use filecraft::i18n::{self, Lang};
use filecraft::nav::NavState;
use filecraft::ui::{self, Theme};

/// Everything the user's own settings decide, read once at startup.
struct Settings {
    lang: Lang,
    columns: ColumnSet,
    /// Where a change is written back, when there is anywhere to write.
    /// `None` is a session that can change a setting but not remember
    /// it, and the commands say so rather than pretending they saved.
    path: Option<PathBuf>,
}

/// Read the language and the listing shape the user asked for.
///
/// The one place the environment and the config file are touched:
/// [`i18n::resolve`] decides the language and [`config::read_columns`]
/// the listing, and both are handed strings so the decisions themselves
/// stay testable without a home directory.
fn resolve_settings(home: Option<&PathBuf>) -> Settings {
    let path = config::path(
        std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .as_deref(),
        home.map(PathBuf::as_path),
    );
    let configured = path.as_deref().and_then(config::load);
    let env = std::env::var("FILECRAFT_LANG").ok();
    let lc_all = std::env::var("LC_ALL").ok();
    let lc_messages = std::env::var("LC_MESSAGES").ok();
    let lang_env = std::env::var("LANG").ok();
    let (lang, _source) = i18n::resolve(&i18n::Request {
        env: env.as_deref(),
        config: configured.as_deref().and_then(config::read_language),
        lc_all: lc_all.as_deref(),
        lc_messages: lc_messages.as_deref(),
        lang: lang_env.as_deref(),
    });
    let columns = configured
        .as_deref()
        .map(|text| config::read_columns(text, &ColumnSet::default()))
        .unwrap_or_default();
    Settings {
        lang,
        columns,
        path,
    }
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    // Resolved before argv is even interpreted, because `--help` and a
    // usage error are the first things filecraft can say.
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let settings = resolve_settings(home.as_ref());
    let lang = settings.lang;
    let cli = match cli::parse_args(&args) {
        Ok(CliAction::Help) => {
            print!("{}", lang.cli_usage());
            return ExitCode::SUCCESS;
        }
        Ok(CliAction::HelpUpdate) => {
            print!("{}", lang.cli_update_usage());
            return ExitCode::SUCCESS;
        }
        Ok(CliAction::Version) => {
            println!("filecraft {}", env!("CARGO_PKG_VERSION"));
            return ExitCode::SUCCESS;
        }
        Ok(CliAction::Update { check }) => {
            return match filecraft::update::run(check) {
                Ok(report) => {
                    print!("{report}");
                    ExitCode::SUCCESS
                }
                Err(error) => {
                    eprintln!("filecraft: {error}");
                    ExitCode::FAILURE
                }
            };
        }
        Ok(CliAction::Run(cli)) => cli,
        Err(error) => {
            eprintln!("filecraft: {}", error.message(lang));
            eprintln!("{}", lang.cli_try_help());
            return ExitCode::from(2);
        }
    };

    let interactive = !cli.force_list && io::stdin().is_terminal() && io::stdout().is_terminal();

    if !interactive {
        if !cli.force_list {
            eprintln!("{}", lang.no_tty_warning());
        }
        return match ui::render_static_listing(&cli.directory, lang) {
            Ok(listing) => {
                let mut stdout = io::stdout().lock();
                if stdout.write_all(listing.as_bytes()).is_err() {
                    return ExitCode::FAILURE;
                }
                ExitCode::SUCCESS
            }
            Err(error) => {
                eprintln!("filecraft: {}", error.message(lang));
                ExitCode::FAILURE
            }
        };
    }

    match run_tui(cli.directory, home, settings) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("filecraft: {error}");
            ExitCode::FAILURE
        }
    }
}

fn run_tui(directory: PathBuf, home: Option<PathBuf>, settings: Settings) -> io::Result<()> {
    let lang = settings.lang;
    let nav = NavState::new(&directory).map_err(|e| io::Error::other(e.message(lang)))?;
    let editor_env = std::env::var("EDITOR").ok();
    let path_env = std::env::var("PATH").ok();
    let nvim_on_path = editor::find_in_path("nvim", path_env.as_deref()).is_some();
    let mut app = App::new(nav, editor_env, nvim_on_path, home, lang);
    app.columns = settings.columns;
    app.config_path = settings.path;
    let no_color = std::env::var("NO_COLOR").ok();
    let ascii = std::env::var("FILECRAFT_ASCII").ok();
    let theme = Theme::from_env(no_color.as_deref(), ascii.as_deref());
    // Key handling fits the ladder to the same width and characters the
    // renderer uses, so every digit on screen is a key that works.
    app.glyphs = theme.glyphs();

    let previous_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        ratatui::restore();
        previous_hook(info);
    }));

    let mut terminal = ratatui::init();
    let result = event_loop(&mut terminal, &mut app, &theme);
    ratatui::restore();
    result
}

/// How often the loop wakes to check on a running summary. Short enough
/// that a finished run is reported at once, long enough that an idle
/// summary costs nothing worth measuring.
const JOB_TICK: Duration = Duration::from_millis(200);

fn event_loop(
    terminal: &mut ratatui::DefaultTerminal,
    app: &mut App,
    theme: &Theme,
) -> io::Result<()> {
    loop {
        let size = terminal.size()?;
        app.set_viewport(
            size.height.saturating_sub(ui::CHROME_ROWS) as usize,
            size.width.saturating_sub(ui::BORDER_COLS) as usize,
        );
        terminal.draw(|frame| ui::draw(frame, app, theme))?;

        // With a summary running the loop ticks instead of blocking, so
        // its status stays live and its result lands on the screen the
        // moment it arrives - without a keypress to prompt it.
        if app.job_active() && !event::poll(JOB_TICK)? {
            app.poll_job();
            continue;
        }

        match event::read()? {
            Event::Key(key) if key.kind != KeyEventKind::Release => {
                let Some(input) = map_key(key.code, key.modifiers) else {
                    continue;
                };
                let effect = app.handle_key(input);
                app.poll_job();
                match effect {
                    Effect::None => {}
                    Effect::Quit => return Ok(()),
                    Effect::RunInteractive { argv } => {
                        run_interactive(terminal, app, &argv)?;
                    }
                    Effect::SpawnDetached { argv } => spawn_detached(app, &argv),
                }
            }
            // Resize is handled by redrawing; other events are ignored.
            _ => {}
        }
    }
}

fn map_key(code: KeyCode, modifiers: KeyModifiers) -> Option<KeyInput> {
    if modifiers.contains(KeyModifiers::CONTROL) {
        return match code {
            KeyCode::Char('c') => Some(KeyInput::CtrlC),
            _ => None,
        };
    }
    match code {
        KeyCode::Char(c) => Some(KeyInput::Char(c)),
        KeyCode::Enter => Some(KeyInput::Enter),
        KeyCode::Esc => Some(KeyInput::Esc),
        KeyCode::Backspace => Some(KeyInput::Backspace),
        KeyCode::Up => Some(KeyInput::Up),
        KeyCode::Down => Some(KeyInput::Down),
        KeyCode::Left => Some(KeyInput::Left),
        KeyCode::Right => Some(KeyInput::Right),
        KeyCode::PageUp => Some(KeyInput::PageUp),
        KeyCode::PageDown => Some(KeyInput::PageDown),
        KeyCode::Home => Some(KeyInput::Home),
        KeyCode::End => Some(KeyInput::End),
        _ => None,
    }
}

/// Suspend the TUI, run an editor/preview command with inherited stdio,
/// then restore the Filecraft screen exactly as it was.
fn run_interactive(
    terminal: &mut ratatui::DefaultTerminal,
    app: &mut App,
    argv: &[String],
) -> io::Result<()> {
    ratatui::restore();
    let status = std::process::Command::new(&argv[0])
        .args(&argv[1..])
        .status();
    *terminal = ratatui::init();
    terminal.clear()?;

    match status {
        Ok(exit) if exit.success() => {
            let text = app.lang.program_closed(&argv[0]);
            app.push_msg(Level::Ok, text);
        }
        Ok(exit) => {
            let text = app.lang.program_exited(&argv[0], &exit.to_string());
            app.push_msg(Level::Info, text);
        }
        Err(error) => {
            let text = app.lang.failed_to_run(&argv[0], &error.to_string());
            app.push_msg(Level::Error, text);
        }
    }
    // The editor may have created or changed files.
    if let Err(error) = app.nav.refresh() {
        let text = error.message(app.lang);
        app.push_msg(Level::Error, text);
    }
    Ok(())
}

/// Spawn a fire-and-forget command (macOS `open`) without blocking the
/// screen; a helper thread reaps the child so it never lingers as a
/// zombie.
fn spawn_detached(app: &mut App, argv: &[String]) {
    let spawned = std::process::Command::new(&argv[0])
        .args(&argv[1..])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
    match spawned {
        Ok(mut child) => {
            std::thread::spawn(move || {
                let _ = child.wait();
            });
        }
        Err(error) => {
            let text = app.lang.failed_to_run(&argv[0], &error.to_string());
            app.push_msg(Level::Error, text);
        }
    }
}
