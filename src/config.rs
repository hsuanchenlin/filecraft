//! The user's own settings: where they live, how they are read, and how
//! one of them is changed without disturbing the rest.
//!
//! There is exactly one setting today - the screen language - so this is
//! deliberately not a TOML library. It is a line-oriented reader and a
//! line-oriented rewriter over a `key = "value"` file, which buys the
//! one property a settings file has to have: **anything Filecraft does
//! not understand is left exactly as it was.** Comments, blank lines,
//! ordering, and keys written by a later version all survive a `:lang`,
//! because the rewrite replaces one line and copies every other.
//!
//! The pure half ([`read_language`], [`with_language`]) is a total
//! function of the file's text, so persistence is tested without a home
//! directory. The IO half ([`path`], [`load`], [`save`]) is the thin
//! shell around it, and it is handed the directories to use rather than
//! reading the environment itself.

use std::io;
use std::path::{Path, PathBuf};

use crate::i18n::Lang;

/// The key the language is stored under.
const LANGUAGE_KEY: &str = "language";

/// The directory Filecraft's own settings live in, under whichever
/// config root applies.
const APP_DIR: &str = "filecraft";

/// The file itself.
const FILE_NAME: &str = "config.toml";

/// Where the config file is, given the environment's two answers to
/// "where does configuration go".
///
/// `XDG_CONFIG_HOME` wins when it is set to an absolute path, because a
/// user who set it meant it; otherwise it is `~/.config`, which is where
/// the documented path `~/.config/filecraft/config.toml` comes from.
/// `None` when neither is known - Filecraft then runs on the resolved
/// language without being able to remember a change to it, and says so.
pub fn path(xdg_config_home: Option<&Path>, home: Option<&Path>) -> Option<PathBuf> {
    let root = match xdg_config_home {
        Some(dir) if dir.is_absolute() => dir.to_path_buf(),
        _ => home?.join(".config"),
    };
    Some(root.join(APP_DIR).join(FILE_NAME))
}

/// The `language` value in a config file's text, if it sets one.
///
/// Reads the *last* assignment rather than the first, which is what a
/// TOML parser would do with a duplicated key and what a person editing
/// the file by hand expects when they paste a new line at the bottom.
/// A `[section]` header ends the top-level table, so a `language` key
/// nested under some future section is not mistaken for this one.
pub fn read_language(text: &str) -> Option<&str> {
    let mut found = None;
    for line in text.lines() {
        let line = line.trim();
        if line.starts_with('#') || line.is_empty() {
            continue;
        }
        if line.starts_with('[') {
            // Everything past the first table header belongs to it.
            break;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        if key.trim() != LANGUAGE_KEY {
            continue;
        }
        let value = value.trim();
        // A trailing comment is not part of the value.
        let value = match value.strip_prefix('"') {
            Some(rest) => rest.split('"').next().unwrap_or(""),
            None => value.split('#').next().unwrap_or("").trim(),
        };
        if !value.is_empty() {
            found = Some(value);
        }
    }
    found
}

/// `text` with the language set to `lang`, and nothing else touched.
///
/// An existing top-level `language` line is rewritten where it stands,
/// so a comment above it still describes it. A file that does not set
/// one gets the line added - and *where* it is added is the whole care
/// here: a top-level key written after a `[table]` header would belong
/// to that table and [`read_language`] would never see it again, so the
/// line goes in ahead of the first header, and ahead of the comment
/// block introducing it. Every other line - comments, blanks, keys from
/// a later version - is copied through unchanged.
pub fn with_language(text: &str, lang: Lang) -> String {
    let assignment = format!("{LANGUAGE_KEY} = \"{}\"", lang.code());
    let mut out = String::with_capacity(text.len() + assignment.len() + 2);
    let mut replaced = false;
    let mut in_table = false;
    // Comments and blanks held back, because a comment run directly
    // above a table header introduces that header and must stay with it.
    let mut pending: Vec<&str> = Vec::new();
    for line in text.lines() {
        let trimmed = line.trim();
        if !in_table && trimmed.starts_with('[') {
            in_table = true;
            if !replaced {
                // The last place this key is still top-level.
                push_line(&mut out, &assignment);
                out.push('\n');
                replaced = true;
            }
        }
        if !in_table && !replaced && (trimmed.is_empty() || trimmed.starts_with('#')) {
            pending.push(line);
            continue;
        }
        for held in pending.drain(..) {
            push_line(&mut out, held);
        }
        let is_language = !in_table
            && trimmed
                .split_once('=')
                .is_some_and(|(key, _)| key.trim() == LANGUAGE_KEY);
        if is_language && !replaced {
            push_line(&mut out, &assignment);
            replaced = true;
        } else if is_language {
            // A duplicate of a key we just wrote would shadow it.
            continue;
        } else {
            push_line(&mut out, line);
        }
    }
    for held in pending.drain(..) {
        push_line(&mut out, held);
    }
    if !replaced {
        if out.is_empty() {
            push_line(&mut out, "# filecraft settings");
        }
        push_line(&mut out, &assignment);
    }
    out
}

fn push_line(out: &mut String, line: &str) {
    out.push_str(line);
    out.push('\n');
}

/// The whole config file, or `None` when there is not one.
///
/// A file that cannot be read is not an error worth stopping for: the
/// language falls through to the system locale exactly as it would with
/// no file at all.
pub fn load(path: &Path) -> Option<String> {
    std::fs::read_to_string(path).ok()
}

/// Write `lang` into the config file at `path`, preserving everything
/// else it holds and creating it (and its directory) when it does not
/// exist yet.
pub fn save(path: &Path, lang: Lang) -> io::Result<()> {
    let existing = std::fs::read_to_string(path).unwrap_or_default();
    let updated = with_language(&existing, lang);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, updated)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_documented_path_is_under_dot_config() {
        let home = PathBuf::from("/Users/someone");
        assert_eq!(
            path(None, Some(&home)),
            Some(PathBuf::from(
                "/Users/someone/.config/filecraft/config.toml"
            ))
        );
    }

    #[test]
    fn an_absolute_xdg_config_home_wins() {
        let xdg = PathBuf::from("/elsewhere/cfg");
        let home = PathBuf::from("/Users/someone");
        assert_eq!(
            path(Some(&xdg), Some(&home)),
            Some(PathBuf::from("/elsewhere/cfg/filecraft/config.toml"))
        );
    }

    #[test]
    fn a_relative_xdg_config_home_is_ignored() {
        // A relative value would put the file wherever filecraft happened
        // to be started from, which is not a settings directory.
        let xdg = PathBuf::from("cfg");
        let home = PathBuf::from("/Users/someone");
        assert_eq!(
            path(Some(&xdg), Some(&home)),
            Some(PathBuf::from(
                "/Users/someone/.config/filecraft/config.toml"
            ))
        );
    }

    #[test]
    fn no_home_and_no_xdg_means_no_config_file() {
        assert_eq!(path(None, None), None);
    }

    #[test]
    fn reads_a_quoted_value() {
        assert_eq!(read_language("language = \"zh-TW\"\n"), Some("zh-TW"));
    }

    #[test]
    fn reads_an_unquoted_value_and_ignores_a_trailing_comment() {
        assert_eq!(read_language("language = zh-TW  # mine\n"), Some("zh-TW"));
    }

    #[test]
    fn ignores_comments_blanks_and_other_keys() {
        let text = "# a comment\n\ntheme = \"dark\"\nlanguage = \"en\"\n";
        assert_eq!(read_language(text), Some("en"));
    }

    #[test]
    fn a_file_without_the_key_sets_nothing() {
        assert_eq!(read_language("# nothing here\ntheme = \"dark\"\n"), None);
        assert_eq!(read_language(""), None);
    }

    #[test]
    fn a_key_inside_a_later_table_is_not_the_top_level_one() {
        let text = "[experimental]\nlanguage = \"zh-TW\"\n";
        assert_eq!(read_language(text), None);
    }

    #[test]
    fn the_last_top_level_assignment_wins() {
        assert_eq!(
            read_language("language = \"en\"\nlanguage = \"zh-TW\"\n"),
            Some("zh-TW")
        );
    }

    #[test]
    fn writing_into_an_empty_file_produces_a_readable_one() {
        let text = with_language("", Lang::ZhTw);
        assert_eq!(read_language(&text), Some("zh-TW"));
        assert!(text.starts_with("# filecraft settings\n"));
    }

    #[test]
    fn writing_keeps_every_line_it_does_not_own() {
        let before = "# my settings\ntheme = \"dark\"\nlanguage = \"en\"\nrows = 40\n";
        let after = with_language(before, Lang::ZhTw);
        assert_eq!(read_language(&after), Some("zh-TW"));
        for kept in ["# my settings", "theme = \"dark\"", "rows = 40"] {
            assert!(after.contains(kept), "lost '{kept}' from:\n{after}");
        }
        // Rewritten in place, so the comment above it still applies.
        let keys: Vec<&str> = after.lines().map(str::trim).collect();
        assert_eq!(keys[2], "language = \"zh-TW\"");
    }

    #[test]
    fn writing_into_a_file_without_the_key_appends_it() {
        let after = with_language("theme = \"dark\"\n", Lang::ZhTw);
        assert_eq!(read_language(&after), Some("zh-TW"));
        assert!(after.starts_with("theme = \"dark\"\n"));
    }

    #[test]
    fn a_duplicated_key_is_collapsed_rather_than_left_to_shadow() {
        let after = with_language("language = \"en\"\nlanguage = \"en\"\n", Lang::ZhTw);
        assert_eq!(
            after.matches("language =").count(),
            1,
            "a second assignment would win over the one we wrote:\n{after}"
        );
        assert_eq!(read_language(&after), Some("zh-TW"));
    }

    #[test]
    fn a_key_under_a_table_is_left_alone_and_the_top_level_one_is_added() {
        let before = "[experimental]\nlanguage = \"en\"\n";
        let after = with_language(before, Lang::ZhTw);
        assert!(after.contains("[experimental]"));
        assert_eq!(read_language(&after), Some("zh-TW"));
    }

    #[test]
    fn what_is_written_is_what_is_read_back() {
        for lang in Lang::ALL {
            let text = with_language("", lang);
            assert_eq!(read_language(&text).and_then(Lang::parse), Some(lang));
        }
    }

    #[test]
    fn a_round_trip_through_a_real_file_survives_other_settings() {
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("nested").join("config.toml");
        save(&file, Lang::ZhTw).unwrap();
        assert_eq!(
            load(&file).as_deref().and_then(read_language),
            Some("zh-TW")
        );

        std::fs::write(&file, "theme = \"dark\"\nlanguage = \"zh-TW\"\n").unwrap();
        save(&file, Lang::En).unwrap();
        let text = load(&file).unwrap();
        assert_eq!(read_language(&text), Some("en"));
        assert!(text.contains("theme = \"dark\""));
    }

    #[test]
    fn a_missing_file_reads_as_no_setting() {
        let tmp = tempfile::tempdir().unwrap();
        assert_eq!(load(&tmp.path().join("absent.toml")), None);
    }
}
