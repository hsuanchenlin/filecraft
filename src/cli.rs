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
    /// Print the version and exit 0.
    Version,
}

pub const USAGE: &str = "\
filecraft - keyboard-first, BBS-style terminal file navigator

USAGE:
  filecraft [OPTIONS] [DIRECTORY]

OPTIONS:
  -l, --list       print a static listing and exit (no TUI)
  -h, --help       show this help
  -V, --version    show version

Interactive mode needs a real TTY. Without one, filecraft prints a
static listing of DIRECTORY (default: the current directory) instead.
Set NO_COLOR to disable colors; selection and markers stay visible.
";

/// Parse argv *after* the program name.
pub fn parse_args(args: &[String]) -> Result<CliAction, String> {
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
    }
}
