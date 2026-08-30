//! Command-line argument parsing for the `filecraft` binary.
//!
//! Interactive vs static-listing is decided later from TTY detection;
//! this module only interprets argv. Nothing here is passed to a shell.

use std::path::PathBuf;

use crate::i18n::{CliError, Lang};

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
    /// Print [`Lang::cli_usage`] and exit 0.
    Help,
    /// Print [`Lang::cli_update_usage`] and exit 0.
    HelpUpdate,
    /// Print the version and exit 0.
    Version,
    /// Self-update (`filecraft update` / `filecraft update --check`).
    Update {
        /// Report whether an update is available without installing.
        check: bool,
    },
}

/// The English usage text, for callers that have no language of their
/// own. The screen's own copy comes from [`Lang::cli_usage`].
pub fn usage() -> &'static str {
    Lang::En.cli_usage()
}

/// [`usage`] for `filecraft update`.
pub fn update_usage() -> &'static str {
    Lang::En.cli_update_usage()
}

/// Parse argv *after* the program name.
pub fn parse_args(args: &[String]) -> Result<CliAction, CliError> {
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
                return Err(CliError::UnknownOption(flag.to_string()));
            }
            path => {
                if directory.is_some() {
                    return Err(CliError::TooManyDirectories);
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

fn parse_update_args(args: &[String]) -> Result<CliAction, CliError> {
    let mut check = false;
    for arg in args {
        match arg.as_str() {
            "-h" | "--help" => return Ok(CliAction::HelpUpdate),
            "-V" | "--version" => return Ok(CliAction::Version),
            "--check" => check = true,
            flag if flag.starts_with('-') => {
                return Err(CliError::UnknownOption(flag.to_string()));
            }
            extra => {
                return Err(CliError::UnexpectedArgument(extra.to_string()));
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
        assert_eq!(
            parse_args(&args(&["update", "--force"])).unwrap_err(),
            CliError::UnknownOption("--force".to_string())
        );
        assert_eq!(
            parse_args(&args(&["update", "--check", "extra"])).unwrap_err(),
            CliError::UnexpectedArgument("extra".to_string())
        );
        assert_eq!(
            parse_args(&args(&["--check"])).unwrap_err(),
            CliError::UnknownOption("--check".to_string())
        );
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
        assert_eq!(
            parse_args(&args(&["--force"])).unwrap_err(),
            CliError::UnknownOption("--force".to_string())
        );
    }

    #[test]
    fn two_directories_is_an_error() {
        assert_eq!(
            parse_args(&args(&["a", "b"])).unwrap_err(),
            CliError::TooManyDirectories
        );
    }

    #[test]
    fn usage_mentions_tty_requirement() {
        assert!(usage().contains("real TTY"));
        assert!(usage().contains("--list"));
        assert!(usage().contains("update [--check]"));
        assert!(usage().contains("current working directory"));
        assert!(update_usage().contains("--check"));
        assert!(update_usage().contains("cargo install --git"));
    }
}
