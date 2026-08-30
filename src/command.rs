//! BBS command-line parsing.
//!
//! Input is tokenized directly by [`tokenize`] and matched against a fixed
//! command table. Nothing is ever passed to a shell: there is no variable
//! expansion, no globbing, no command substitution. Quoting exists only so
//! file names containing spaces can be written on the prompt.

use crate::i18n::{Lang, Usage};

/// A parsed BBS command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    /// `cd [path]` - change directory. No argument means the home directory.
    Cd { path: Option<String> },
    /// `move [destination]` - move the selected entry. No destination
    /// opens the folder picker; a path goes straight to confirmation.
    Move { destination: Option<String> },
    /// `rename <name>` - rename the selected entry (asks for confirmation).
    Rename { name: String },
    /// `delete` / `trash` - move the selected entry to the system Trash
    /// (asks for confirmation). Never an unrecoverable removal.
    Trash,
    /// `open` - open the selected entry with macOS `open`.
    Open,
    /// `edit` - edit the selected file in `$EDITOR` (fallback: `nvim`).
    Edit,
    /// `preview` - read-only preview of the selected entry.
    Preview,
    /// `summarize` / `summary` - open the multi-file selector for an AI
    /// summary. Takes no argument: files are picked in the selector, and
    /// the provider is chosen in the dialog that follows.
    Summarize,
    /// `log` / `job` - open the live log of the AI summary run: what the
    /// provider has printed, and the session it announced. Takes no
    /// argument - there is one run, and it is the one being watched.
    Log,
    /// `lang [code]` / `language [code]` - the screen's language. No
    /// argument reports the current one; a code switches to it and
    /// remembers the choice.
    Language { code: Option<String> },
    /// `help` - show the help screen.
    Help,
    /// `quit` - leave Filecraft.
    Quit,
    /// `agent [...]` - future AI-agent seam; disabled in v0.
    Agent { args: Vec<String> },
}

/// Why a command line failed to parse.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParseError {
    /// The line was empty or only whitespace.
    Empty,
    /// The first word is not a known command.
    Unknown(String),
    /// A known command was given the wrong arguments.
    Usage { command: &'static str, usage: Usage },
    /// A quote was opened but never closed.
    UnterminatedQuote,
    /// A trailing backslash with nothing to escape.
    TrailingEscape,
}

impl ParseError {
    /// Why the line failed, in `lang`. Command names and the word the
    /// user typed are quoted as typed - only the explanation around them
    /// is translated.
    pub fn message(&self, lang: Lang) -> String {
        match self {
            ParseError::Empty => lang.empty_command().to_string(),
            ParseError::Unknown(word) => lang.unknown_command(word),
            ParseError::Usage { command, usage } => lang.usage_line(command, *usage),
            ParseError::UnterminatedQuote => lang.unterminated_quote().to_string(),
            ParseError::TrailingEscape => lang.trailing_escape().to_string(),
        }
    }
}

impl std::fmt::Display for ParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message(Lang::En))
    }
}

/// Split a command line into words.
///
/// Supported syntax, chosen so names with spaces are typeable:
/// - words separated by whitespace
/// - `"..."` and `'...'` quoting (may appear mid-word)
/// - `\x` escapes the next character outside single quotes
///
/// There is deliberately no other syntax: `$`, `*`, `;`, `|`, `>` and
/// friends are ordinary characters.
pub fn tokenize(line: &str) -> Result<Vec<String>, ParseError> {
    let mut words = Vec::new();
    let mut current = String::new();
    let mut in_word = false;
    let mut chars = line.chars();

    #[derive(PartialEq)]
    enum Quote {
        None,
        Single,
        Double,
    }
    let mut quote = Quote::None;

    while let Some(c) = chars.next() {
        match quote {
            Quote::Single => {
                if c == '\'' {
                    quote = Quote::None;
                } else {
                    current.push(c);
                }
            }
            Quote::Double => match c {
                '"' => quote = Quote::None,
                '\\' => match chars.next() {
                    Some(next) => current.push(next),
                    None => return Err(ParseError::TrailingEscape),
                },
                _ => current.push(c),
            },
            Quote::None => match c {
                c if c.is_whitespace() => {
                    if in_word {
                        words.push(std::mem::take(&mut current));
                        in_word = false;
                    }
                }
                '\'' => {
                    quote = Quote::Single;
                    in_word = true;
                }
                '"' => {
                    quote = Quote::Double;
                    in_word = true;
                }
                '\\' => match chars.next() {
                    Some(next) => {
                        current.push(next);
                        in_word = true;
                    }
                    None => return Err(ParseError::TrailingEscape),
                },
                _ => {
                    current.push(c);
                    in_word = true;
                }
            },
        }
    }
    if quote != Quote::None {
        return Err(ParseError::UnterminatedQuote);
    }
    if in_word {
        words.push(current);
    }
    Ok(words)
}

/// Parse one BBS command line.
///
/// Commands taking a path/name argument are strict: at most one argument,
/// quoted if it contains spaces. This avoids silently misreading
/// `move a b` (Filecraft's `move` acts on the *selected* entry, so a second
/// word is almost certainly a mistake). `move` with no argument opens the
/// folder picker instead of requiring a typed path.
pub fn parse(line: &str) -> Result<Command, ParseError> {
    let words = tokenize(line)?;
    let Some((head, args)) = words.split_first() else {
        return Err(ParseError::Empty);
    };

    let head_lower = head.to_lowercase();
    match head_lower.as_str() {
        "cd" => match args {
            [] => Ok(Command::Cd { path: None }),
            [path] => Ok(Command::Cd {
                path: Some(path.clone()),
            }),
            _ => Err(ParseError::Usage {
                command: "cd",
                usage: Usage::Cd,
            }),
        },
        "move" | "mv" => match args {
            [] => Ok(Command::Move { destination: None }),
            [destination] => Ok(Command::Move {
                destination: Some(destination.clone()),
            }),
            _ => Err(ParseError::Usage {
                command: typed(&head_lower, &["move", "mv"]),
                usage: Usage::Move,
            }),
        },
        "rename" => match args {
            [name] => Ok(Command::Rename { name: name.clone() }),
            _ => Err(ParseError::Usage {
                command: "rename",
                usage: Usage::Rename,
            }),
        },
        // Both names mean the same recoverable move-to-Trash. `rm`,
        // `del`, and `rmdir` stay unknown on purpose: they promise POSIX
        // removal, and Filecraft never removes anything.
        "delete" | "trash" => no_args(
            args,
            Command::Trash,
            typed(&head_lower, &["delete", "trash"]),
            Usage::Trash,
        ),
        "open" => no_args(args, Command::Open, "open", Usage::None),
        "edit" => no_args(args, Command::Edit, "edit", Usage::None),
        "preview" => no_args(args, Command::Preview, "preview", Usage::None),
        // No path form on purpose: the files are picked in the selector,
        // so nothing the user types ever reaches the provider's argv.
        "summarize" | "summary" => no_args(
            args,
            Command::Summarize,
            typed(&head_lower, &["summarize", "summary"]),
            Usage::Summarize,
        ),
        // Both names mean the one summary run there can be.
        "log" | "job" => no_args(
            args,
            Command::Log,
            typed(&head_lower, &["log", "job"]),
            Usage::Log,
        ),
        // The screen's language. One argument at most, and it is a
        // language code rather than a path: nothing typed here ever
        // reaches a filesystem call or a child process.
        "lang" | "language" => match args {
            [] => Ok(Command::Language { code: None }),
            [code] => Ok(Command::Language {
                code: Some(code.clone()),
            }),
            _ => Err(ParseError::Usage {
                command: typed(&head_lower, &["lang", "language"]),
                usage: Usage::Language,
            }),
        },
        "help" | "?" => no_args(
            args,
            Command::Help,
            typed(&head_lower, &["help", "?"]),
            Usage::None,
        ),
        "quit" | "q" | "exit" => no_args(
            args,
            Command::Quit,
            typed(&head_lower, &["quit", "q", "exit"]),
            Usage::None,
        ),
        "agent" => Ok(Command::Agent {
            args: args.to_vec(),
        }),
        _ => Err(ParseError::Unknown(head.clone())),
    }
}

/// The spelling the user actually typed, so a usage line never names a
/// command they did not type. Falls back to the canonical name, which is
/// the first alias.
fn typed(head: &str, aliases: &[&'static str]) -> &'static str {
    aliases
        .iter()
        .copied()
        .find(|alias| *alias == head)
        .unwrap_or(aliases[0])
}

fn no_args(
    args: &[String],
    ok: Command,
    command: &'static str,
    usage: Usage,
) -> Result<Command, ParseError> {
    if args.is_empty() {
        Ok(ok)
    } else {
        Err(ParseError::Usage { command, usage })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokenize_plain_words() {
        assert_eq!(tokenize("cd docs").unwrap(), vec!["cd", "docs"]);
    }

    #[test]
    fn tokenize_collapses_whitespace() {
        assert_eq!(tokenize("  cd\t docs  ").unwrap(), vec!["cd", "docs"]);
    }

    #[test]
    fn tokenize_double_quotes_keep_spaces() {
        assert_eq!(
            tokenize(r#"move "My Documents""#).unwrap(),
            vec!["move", "My Documents"]
        );
    }

    #[test]
    fn tokenize_single_quotes_are_literal() {
        assert_eq!(
            tokenize(r#"rename 'a "b" \n c'"#).unwrap(),
            vec!["rename", r#"a "b" \n c"#]
        );
    }

    #[test]
    fn tokenize_backslash_escapes_space() {
        assert_eq!(
            tokenize(r"cd My\ Documents").unwrap(),
            vec!["cd", "My Documents"]
        );
    }

    #[test]
    fn tokenize_mid_word_quotes() {
        assert_eq!(tokenize(r#"cd a"b c"d"#).unwrap(), vec!["cd", "ab cd"]);
    }

    #[test]
    fn tokenize_shell_metacharacters_are_literal() {
        assert_eq!(
            tokenize("cd $HOME;rm|x>*").unwrap(),
            vec!["cd", "$HOME;rm|x>*"]
        );
    }

    #[test]
    fn tokenize_unicode() {
        assert_eq!(
            tokenize("cd 資料夾/新しい").unwrap(),
            vec!["cd", "資料夾/新しい"]
        );
    }

    #[test]
    fn tokenize_unterminated_quote_is_error() {
        assert_eq!(tokenize(r#"cd "oops"#), Err(ParseError::UnterminatedQuote));
        assert_eq!(tokenize("cd 'oops"), Err(ParseError::UnterminatedQuote));
    }

    #[test]
    fn tokenize_trailing_escape_is_error() {
        assert_eq!(tokenize(r"cd oops\"), Err(ParseError::TrailingEscape));
    }

    #[test]
    fn parse_empty_line() {
        assert_eq!(parse(""), Err(ParseError::Empty));
        assert_eq!(parse("   "), Err(ParseError::Empty));
    }

    #[test]
    fn parse_unknown_command() {
        assert_eq!(
            parse("frobnicate x"),
            Err(ParseError::Unknown("frobnicate".into()))
        );
    }

    #[test]
    fn parse_is_case_insensitive_on_the_command_word() {
        assert_eq!(parse("QUIT").unwrap(), Command::Quit);
        assert_eq!(
            parse("CD /tmp").unwrap(),
            Command::Cd {
                path: Some("/tmp".into())
            }
        );
    }

    #[test]
    fn parse_cd_variants() {
        assert_eq!(parse("cd").unwrap(), Command::Cd { path: None });
        assert_eq!(
            parse("cd ~/Documents").unwrap(),
            Command::Cd {
                path: Some("~/Documents".into())
            }
        );
        assert!(matches!(
            parse("cd a b"),
            Err(ParseError::Usage { command: "cd", .. })
        ));
    }

    #[test]
    fn parse_move_destination_is_optional() {
        assert_eq!(
            parse("move ../archive").unwrap(),
            Command::Move {
                destination: Some("../archive".into())
            }
        );
        assert_eq!(parse("move").unwrap(), Command::Move { destination: None });
        assert!(matches!(parse("move a b"), Err(ParseError::Usage { .. })));
    }

    #[test]
    fn parse_mv_alias() {
        assert_eq!(
            parse("mv dest").unwrap(),
            Command::Move {
                destination: Some("dest".into())
            }
        );
        assert_eq!(parse("mv").unwrap(), Command::Move { destination: None });
    }

    #[test]
    fn parse_rename() {
        assert_eq!(
            parse(r#"rename "new name.txt""#).unwrap(),
            Command::Rename {
                name: "new name.txt".into()
            }
        );
        assert!(matches!(parse("rename"), Err(ParseError::Usage { .. })));
    }

    #[test]
    fn parse_no_arg_commands_reject_args() {
        assert_eq!(parse("open").unwrap(), Command::Open);
        assert_eq!(parse("edit").unwrap(), Command::Edit);
        assert_eq!(parse("preview").unwrap(), Command::Preview);
        assert_eq!(parse("help").unwrap(), Command::Help);
        assert!(matches!(parse("edit foo"), Err(ParseError::Usage { .. })));
        assert!(matches!(parse("open foo"), Err(ParseError::Usage { .. })));
    }

    #[test]
    fn parse_log_aliases_take_no_argument() {
        assert_eq!(parse("log").unwrap(), Command::Log);
        assert_eq!(parse("job").unwrap(), Command::Log);
        assert_eq!(parse("LOG").unwrap(), Command::Log);
        // There is one run, so there is nothing to name.
        assert!(matches!(
            parse("log run.txt"),
            Err(ParseError::Usage { command: "log", .. })
        ));
        assert!(matches!(
            parse("job 2"),
            Err(ParseError::Usage { command: "job", .. })
        ));
    }

    #[test]
    fn parse_summarize_aliases_take_no_argument() {
        assert_eq!(parse("summarize").unwrap(), Command::Summarize);
        assert_eq!(parse("summary").unwrap(), Command::Summarize);
        assert_eq!(parse("SUMMARIZE").unwrap(), Command::Summarize);
        assert!(matches!(
            parse("summarize a.pdf"),
            Err(ParseError::Usage {
                command: "summarize",
                ..
            })
        ));
        assert!(matches!(
            parse("summary a.pdf"),
            Err(ParseError::Usage {
                command: "summary",
                ..
            })
        ));
    }

    #[test]
    fn parse_quit_aliases() {
        assert_eq!(parse("quit").unwrap(), Command::Quit);
        assert_eq!(parse("q").unwrap(), Command::Quit);
        assert_eq!(parse("exit").unwrap(), Command::Quit);
    }

    #[test]
    fn parse_agent_accepts_free_args() {
        assert_eq!(parse("agent").unwrap(), Command::Agent { args: vec![] });
        assert_eq!(
            parse("agent summarize this").unwrap(),
            Command::Agent {
                args: vec!["summarize".into(), "this".into()]
            }
        );
    }

    #[test]
    fn parse_trash_aliases() {
        assert_eq!(parse("delete").unwrap(), Command::Trash);
        assert_eq!(parse("trash").unwrap(), Command::Trash);
        assert_eq!(parse("DELETE").unwrap(), Command::Trash);
    }

    #[test]
    fn parse_trash_takes_no_argument() {
        // `delete` acts on the *selected* entry; a path argument would
        // read like `rm <path>` and mean something Filecraft never does.
        assert!(matches!(
            parse("delete a.txt"),
            Err(ParseError::Usage {
                command: "delete",
                ..
            })
        ));
        assert!(matches!(
            parse("trash a.txt"),
            Err(ParseError::Usage {
                command: "trash",
                ..
            })
        ));
    }

    #[test]
    fn a_usage_line_names_the_alias_the_user_typed() {
        for (line, expected) in [
            ("delete a.txt", "usage: delete"),
            ("trash a.txt", "usage: trash"),
            ("move a b", "usage: move"),
            ("mv a b", "usage: mv"),
            ("q now", "usage: q"),
        ] {
            let err = parse(line).unwrap_err();
            assert!(
                err.to_string().starts_with(expected),
                "'{line}' reported '{err}', which names a command nobody typed"
            );
        }
    }

    #[test]
    fn no_permanent_delete_command_exists() {
        // Removal is move-to-Trash and nothing else. The POSIX removal
        // words stay unknown so no one can type one and be surprised
        // either way - by a deletion, or by a trash they meant as a rm.
        for word in ["rm", "del", "rmdir", "unlink", "shred"] {
            assert!(
                matches!(parse(word), Err(ParseError::Unknown(_))),
                "'{word}' must not parse as a command"
            );
        }
    }
}
