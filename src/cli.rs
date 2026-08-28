//! Command-line argument parsing for the `filecraft` binary.
//!
//! Interactive vs static-listing is decided later from TTY detection;
//! this module only interprets argv. Nothing here is passed to a shell.

use std::path::PathBuf;

/// Parsed runtime options for a normal invocation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CliArgs {
    /// Directory to open (default: `.`).
    pub directory: PathBuf,
    /// Force the static listing even on a TTY (`--list`).
    pub force_list: bool,
}

/// What `filecraft` should do after parsing argv.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CliAction {
    /// Run against a directory (interactive TUI or static listing).
    Run(CliArgs),
    /// Print [`USAGE`] and exit 0.
    Help,
    /// Print [`UPDATE_USAGE`] and exit 0.
    HelpUpdate,
    /// Print the version and exit 0.
    Version,
    /// Self-update (`filecraft update` / `filecraft update --check`).
    Update {
        /// Report whether an update is available without installing.
        check: bool,
    },
}

pub const USAGE: &str = "\
filecraft - keyboard-first, BBS-style terminal file navigator

USAGE:
  filecraft [OPTIONS] [DIRECTORY]
  filecraft update [--check]

OPTIONS:
  -l, --list       print a static listing and exit (no TUI)
  -h, --help       show this help
  -V, --version    show version

COMMANDS:
  update           install the latest filecraft
  update --check   report whether an update is available

No DIRECTORY opens the current working directory. A folder named
update is opened as `filecraft ./update`.

Interactive mode needs a real TTY. Without one, filecraft prints a
static listing of DIRECTORY (default: the current directory) instead.
Set NO_COLOR to disable colors; selection and markers stay visible.
Set FILECRAFT_ASCII to draw the screen in printable ASCII only.
";

pub const UPDATE_USAGE: &str = "\
filecraft update - install the latest filecraft

USAGE:
  filecraft update [--check]

  --check    check for an update without installing

A local git clone is pulled with `git pull --ff-only` and reinstalled
with `cargo install --path <clone> --locked --force`. A global cargo
install is refreshed with:
  cargo install --git https://github.com/hsuanchenlin/filecraft.git --locked --force

Requires `cargo` (and `git` for a clone). Network, missing tools, and
permission errors are reported and do not crash.
";

/// Parse argv *after* the program name.
pub fn parse_args(args: &[String]) -> Result<CliAction, String> {
    if args.first().map(String::as_str) == Some("update") {
        return parse_update_args(&args[1..]);
    }
    let mut directory: Option<PathBuf> = None;
    let mut force_list = false;
    for arg in args {
        match arg.as_str() {
            "-h" | "--help" => return Ok(CliAction::Help),
            "-V" | "--version" => return Ok(CliAction::Version),
            "-l" | "--list" => force_list = true,
            flag if flag.starts_with('-') => {
                return Err(format!("unknown option '{flag}'"));
            }
            path => {
                if directory.is_some() {
                    return Err("expected at most one DIRECTORY argument".to_string());
                }
                directory = Some(PathBuf::from(path));
            }
        }
    }
    Ok(CliAction::Run(CliArgs {
        directory: directory.unwrap_or_else(|| PathBuf::from(".")),
        force_list,
    }))
}

fn parse_update_args(args: &[String]) -> Result<CliAction, String> {
    let mut check = false;
    for arg in args {
        match arg.as_str() {
            "-h" | "--help" => return Ok(CliAction::HelpUpdate),
            "-V" | "--version" => return Ok(CliAction::Version),
            "--check" => check = true,
            flag if flag.starts_with('-') => {
                return Err(format!("unknown option '{flag}'"));
            }
            extra => {
                return Err(format!("unexpected argument '{extra}'"));
            }
        }
    }
    Ok(CliAction::Update { check })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(words: &[&str]) -> Vec<String> {
        words.iter().map(|s| (*s).to_string()).collect()
    }

    #[test]
    fn defaults_to_current_directory() {
        assert_eq!(
            parse_args(&[]).unwrap(),
            CliAction::Run(CliArgs {
                directory: PathBuf::from("."),
                force_list: false,
            })
        );
    }

    #[test]
    fn positional_directory_opens_that_path() {
        assert_eq!(
            parse_args(&args(&["/tmp/notes"])).unwrap(),
            CliAction::Run(CliArgs {
                directory: PathBuf::from("/tmp/notes"),
                force_list: false,
            })
        );
        assert_eq!(
            parse_args(&args(&["./update"])).unwrap(),
            CliAction::Run(CliArgs {
                directory: PathBuf::from("./update"),
                force_list: false,
            })
        );
    }

    #[test]
    fn directory_and_list_flag() {
        assert_eq!(
            parse_args(&args(&["--list", "/tmp/a b"])).unwrap(),
            CliAction::Run(CliArgs {
                directory: PathBuf::from("/tmp/a b"),
                force_list: true,
            })
        );
        assert_eq!(
            parse_args(&args(&["-l", "docs"])).unwrap(),
            CliAction::Run(CliArgs {
                directory: PathBuf::from("docs"),
                force_list: true,
            })
        );
        assert_eq!(
            parse_args(&args(&["--list", "update"])).unwrap(),
            CliAction::Run(CliArgs {
                directory: PathBuf::from("update"),
                force_list: true,
            })
        );
    }

    #[test]
    fn update_and_check_flags() {
        assert_eq!(
            parse_args(&args(&["update"])).unwrap(),
            CliAction::Update { check: false }
        );
        assert_eq!(
            parse_args(&args(&["update", "--check"])).unwrap(),
            CliAction::Update { check: true }
        );
        assert_eq!(
            parse_args(&args(&["update", "--help"])).unwrap(),
            CliAction::HelpUpdate
        );
        assert_eq!(
            parse_args(&args(&["update", "-h"])).unwrap(),
            CliAction::HelpUpdate
        );
        assert_eq!(
            parse_args(&args(&["update", "--version"])).unwrap(),
            CliAction::Version
        );
    }

    #[test]
    fn update_rejects_unknown_flags_and_extra_args() {
        let err = parse_args(&args(&["update", "--force"])).unwrap_err();
        assert!(err.contains("unknown option"));
        let err = parse_args(&args(&["update", "--check", "extra"])).unwrap_err();
        assert!(err.contains("unexpected argument"));
        let err = parse_args(&args(&["--check"])).unwrap_err();
        assert!(err.contains("unknown option"));
    }

    #[test]
    fn help_and_version_short_circuit() {
        assert_eq!(parse_args(&args(&["--help"])).unwrap(), CliAction::Help);
        assert_eq!(parse_args(&args(&["-h"])).unwrap(), CliAction::Help);
        assert_eq!(
            parse_args(&args(&["--version"])).unwrap(),
            CliAction::Version
        );
        assert_eq!(parse_args(&args(&["-V"])).unwrap(), CliAction::Version);
    }

    #[test]
    fn unknown_flag_is_an_error() {
        let err = parse_args(&args(&["--force"])).unwrap_err();
        assert!(err.contains("unknown option"));
    }

    #[test]
    fn two_directories_is_an_error() {
        let err = parse_args(&args(&["a", "b"])).unwrap_err();
        assert!(err.contains("at most one"));
    }

    #[test]
    fn usage_mentions_tty_requirement() {
        assert!(USAGE.contains("real TTY"));
        assert!(USAGE.contains("--list"));
        assert!(USAGE.contains("update [--check]"));
        assert!(USAGE.contains("current working directory"));
        assert!(UPDATE_USAGE.contains("--check"));
        assert!(UPDATE_USAGE.contains("cargo install --git"));
    }
}
