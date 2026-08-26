//! Filecraft entry point: argument handling, TTY detection, the terminal
//! event loop, and interpretation of [`Effect`]s (running editors,
//! spawning macOS `open`). Everything decision-shaped lives in the
//! library; this file is deliberately thin.

use std::io::{self, IsTerminal, Write};
use std::path::PathBuf;
use std::process::ExitCode;

use ratatui::crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};

use filecraft::app::{App, Effect, KeyInput, Level};
use filecraft::cli::{self, CliAction};
use filecraft::editor;
use filecraft::nav::NavState;
use filecraft::ui::{self, Theme};

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let cli = match cli::parse_args(&args) {
        Ok(CliAction::Help) => {
            print!("{}", cli::USAGE);
            return ExitCode::SUCCESS;
        }
        Ok(CliAction::Version) => {
            println!("filecraft {}", env!("CARGO_PKG_VERSION"));
            return ExitCode::SUCCESS;
        }
        Ok(CliAction::Run(cli)) => cli,
        Err(message) => {
            eprintln!("filecraft: {message}");
            eprintln!("try 'filecraft --help'");
            return ExitCode::from(2);
        }
    };

    let interactive = !cli.force_list && io::stdin().is_terminal() && io::stdout().is_terminal();

    if !interactive {
        if !cli.force_list {
            eprintln!(
                "filecraft: no TTY detected; printing a static listing \
                 (run in a real terminal for the interactive screen)"
            );
        }
        return match ui::render_static_listing(&cli.directory) {
            Ok(listing) => {
                let mut stdout = io::stdout().lock();
                if stdout.write_all(listing.as_bytes()).is_err() {
                    return ExitCode::FAILURE;
                }
                ExitCode::SUCCESS
            }
            Err(error) => {
                eprintln!("filecraft: {error}");
                ExitCode::FAILURE
            }
        };
    }

    match run_tui(cli.directory) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("filecraft: {error}");
            ExitCode::FAILURE
        }
    }
}

fn run_tui(directory: PathBuf) -> io::Result<()> {
    let nav = NavState::new(&directory).map_err(|e| io::Error::other(e.to_string()))?;
    let editor_env = std::env::var("EDITOR").ok();
    let path_env = std::env::var("PATH").ok();
    let nvim_on_path = editor::find_in_path("nvim", path_env.as_deref()).is_some();
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let mut app = App::new(nav, editor_env, nvim_on_path, home);
    let no_color = std::env::var("NO_COLOR").ok();
    let theme = Theme::from_no_color_env(no_color.as_deref());

    let mut terminal = ratatui::init();
    let result = event_loop(&mut terminal, &mut app, &theme);
    ratatui::restore();
    result
}

fn event_loop(
    terminal: &mut ratatui::DefaultTerminal,
    app: &mut App,
    theme: &Theme,
) -> io::Result<()> {
    loop {
        let size = terminal.size()?;
        app.viewport_rows = size.height.saturating_sub(ui::CHROME_ROWS).max(1) as usize;
        terminal.draw(|frame| ui::draw(frame, app, theme))?;

        match event::read()? {
            Event::Key(key) if key.kind != KeyEventKind::Release => {
                let Some(input) = map_key(key.code, key.modifiers) else {
                    continue;
                };
                match app.handle_key(input) {
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
            app.push_msg(Level::Ok, format!("{} closed", argv[0]));
        }
        Ok(exit) => {
            app.push_msg(Level::Info, format!("{} exited with {exit}", argv[0]));
        }
        Err(error) => {
            app.push_msg(
                Level::Error,
                format!("failed to run '{}': {error}", argv[0]),
            );
        }
    }
    // The editor may have created or changed files.
    if let Err(error) = app.nav.refresh() {
        app.push_msg(Level::Error, error.to_string());
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
            app.push_msg(
                Level::Error,
                format!("failed to run '{}': {error}", argv[0]),
            );
        }
    }
}
