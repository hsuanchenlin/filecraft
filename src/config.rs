//! The user's own settings: where they live, how they are read, and how
//! one of them is changed without disturbing the rest.
//!
//! There are two settings today - the screen language, and the shape of
//! the listing - so this is deliberately not a TOML library. It is a
//! line-oriented reader and a line-oriented rewriter over a
//! `key = "value"` file, which buys the one property a settings file has
//! to have: **anything Filecraft does not understand is left exactly as
//! it was.** Comments, blank lines, ordering, and keys written by a later
//! version all survive a `:lang` or a `:columns`, because the rewrite
//! replaces the lines it owns and copies every other.
//!
//! The file has one top-level key and one table:
//!
//! ```toml
//! language = "zh-TW"
//!
//! [columns]
//! visible = ["name", "size", "modified"]
//! header = true
//! ```
//!
//! The split between them is the sharp edge here. A top-level key
//! written *after* a `[table]` header would belong to that table, so
//! [`with_language`] is careful to put `language` ahead of the first
//! header; and a `[columns]` key is only read while the reader is inside
//! that table, so a stray `header = true` at the top of the file is not
//! mistaken for one.
//!
//! The pure half ([`read_language`], [`with_language`], [`read_columns`],
//! [`with_columns`]) is a total function of the file's text, so
//! persistence is tested without a home directory. The IO half
//! ([`path`], [`load`], [`save`]) is the thin shell around it, and it is
//! handed the directories to use rather than reading the environment
//! itself.

use std::io;
use std::path::{Path, PathBuf};

use crate::columns::{Column, ColumnSet};
use crate::i18n::Lang;

/// The key the language is stored under.
const LANGUAGE_KEY: &str = "language";

/// The table the listing's shape is stored under.
const COLUMNS_TABLE: &str = "columns";

/// The key inside it that lists the columns, in order.
const VISIBLE_KEY: &str = "visible";

/// The key inside it that turns the column header row on and off.
const HEADER_KEY: &str = "header";

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
    rewrite(path, |text| with_language(text, lang))
}

/// Write `set` into the config file at `path`, the same way and with the
/// same promise: every line this does not own is copied through.
pub fn save_columns(path: &Path, set: &ColumnSet) -> io::Result<()> {
    rewrite(path, |text| with_columns(text, set))
}

fn rewrite(path: &Path, change: impl FnOnce(&str) -> String) -> io::Result<()> {
    let existing = std::fs::read_to_string(path).unwrap_or_default();
    let updated = change(&existing);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, updated)
}

/// The listing shape a config file asks for, as far as it says anything.
///
/// Both halves are independent: a file that sets `visible` but not
/// `header` keeps the default header, and a word naming no column is
/// skipped rather than fatal - a settings file written by a later
/// version must not stop this one from starting.
pub fn read_columns(text: &str, default: &ColumnSet) -> ColumnSet {
    let mut visible: Option<Vec<Column>> = None;
    let mut header: Option<bool> = None;
    for (key, value) in table_entries(text, COLUMNS_TABLE) {
        match key {
            VISIBLE_KEY => visible = Some(read_column_list(value)),
            HEADER_KEY => header = read_bool(value).or(header),
            _ => {}
        }
    }
    let header = header.unwrap_or(default.header);
    match visible {
        // A `visible = []` names no column at all, which is not a
        // listing; the default stands and the header setting still takes.
        Some(columns) if !columns.is_empty() => ColumnSet::new(columns, header),
        _ => ColumnSet::new(default.visible().iter().copied(), header),
    }
}

/// Every `key = value` pair inside `[table]`, in file order.
///
/// A duplicated table is read as one, which is what a TOML parser would
/// refuse and what a person pasting a second block expects to work.
fn table_entries<'a>(text: &'a str, table: &str) -> Vec<(&'a str, &'a str)> {
    let header = format!("[{table}]");
    let mut inside = false;
    let mut found = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.starts_with('#') || line.is_empty() {
            continue;
        }
        if line.starts_with('[') {
            inside = line == header;
            continue;
        }
        if !inside {
            continue;
        }
        if let Some((key, value)) = line.split_once('=') {
            found.push((key.trim(), value.trim()));
        }
    }
    found
}

/// The columns a `visible = [...]` value names.
///
/// Written as a TOML array, read as the words in it: brackets, quotes,
/// and commas are stripped, and a word naming no column is skipped so a
/// file from a later version still starts this one.
fn read_column_list(value: &str) -> Vec<Column> {
    let inner = value
        .trim()
        .trim_start_matches('[')
        .split(']')
        .next()
        .unwrap_or("");
    let mut columns = Vec::new();
    for word in inner.split(',') {
        let word = word.trim().trim_matches(['"', '\'']).trim();
        if word.is_empty() {
            continue;
        }
        if let Some(column) = Column::parse(word) {
            if !columns.contains(&column) {
                columns.push(column);
            }
        }
    }
    columns
}

/// A TOML boolean, tolerant of the spellings a person writes by hand.
fn read_bool(value: &str) -> Option<bool> {
    let value = value.split('#').next().unwrap_or("").trim();
    let value = value.trim_matches('"').trim().to_ascii_lowercase();
    match value.as_str() {
        "true" | "on" | "yes" | "1" => Some(true),
        "false" | "off" | "no" | "0" => Some(false),
        _ => None,
    }
}

/// `text` with the listing shape set to `set`, and nothing else touched.
///
/// An existing `[columns]` table is rewritten key by key where it
/// stands, so a comment above `visible` still describes it, and any key
/// in that table Filecraft does not know is copied through. A file with
/// no such table gets one appended - at the end, which is the only place
/// a new table cannot capture a top-level key that was already there.
pub fn with_columns(text: &str, set: &ColumnSet) -> String {
    let visible = format!("{VISIBLE_KEY} = {}", visible_array(set));
    let header = format!("{HEADER_KEY} = {}", set.header);
    let mut out = String::with_capacity(text.len() + visible.len() + header.len() + 16);
    let mut inside = false;
    let mut seen_table = false;
    let (mut wrote_visible, mut wrote_header) = (false, false);
    // Blank lines held back at the end of the table, so the two keys are
    // written against the block they belong to rather than after a gap.
    let mut pending: Vec<&str> = Vec::new();
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('[') {
            if inside {
                flush_table(
                    &mut out,
                    &visible,
                    &header,
                    &mut wrote_visible,
                    &mut wrote_header,
                );
            }
            for held in pending.drain(..) {
                push_line(&mut out, held);
            }
            inside = trimmed == format!("[{COLUMNS_TABLE}]");
            seen_table |= inside;
            push_line(&mut out, line);
            continue;
        }
        if inside && trimmed.is_empty() {
            pending.push(line);
            continue;
        }
        for held in pending.drain(..) {
            push_line(&mut out, held);
        }
        let key = if inside {
            trimmed.split_once('=').map(|(key, _)| key.trim())
        } else {
            None
        };
        match key {
            Some(VISIBLE_KEY) if !wrote_visible => {
                push_line(&mut out, &visible);
                wrote_visible = true;
            }
            Some(HEADER_KEY) if !wrote_header => {
                push_line(&mut out, &header);
                wrote_header = true;
            }
            // A duplicate of a key just written would shadow it.
            Some(VISIBLE_KEY) | Some(HEADER_KEY) => {}
            _ => push_line(&mut out, line),
        }
    }
    if inside {
        flush_table(
            &mut out,
            &visible,
            &header,
            &mut wrote_visible,
            &mut wrote_header,
        );
    }
    for held in pending.drain(..) {
        push_line(&mut out, held);
    }
    if !seen_table {
        if !out.is_empty() && !out.ends_with("\n\n") {
            out.push('\n');
        }
        push_line(&mut out, &format!("[{COLUMNS_TABLE}]"));
        push_line(&mut out, &visible);
        push_line(&mut out, &header);
    }
    out
}

fn flush_table(
    out: &mut String,
    visible: &str,
    header: &str,
    wrote_visible: &mut bool,
    wrote_header: &mut bool,
) {
    if !*wrote_visible {
        push_line(out, visible);
        *wrote_visible = true;
    }
    if !*wrote_header {
        push_line(out, header);
        *wrote_header = true;
    }
}

fn visible_array(set: &ColumnSet) -> String {
    let words: Vec<String> = set
        .visible()
        .iter()
        .map(|c| format!("\"{}\"", c.code()))
        .collect();
    format!("[{}]", words.join(", "))
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

    // ---- The `[columns]` table -------------------------------------------

    fn columns_of(text: &str) -> String {
        read_columns(text, &ColumnSet::default()).spec()
    }

    #[test]
    fn a_file_with_no_columns_table_reads_as_the_default_listing() {
        let default = ColumnSet::default();
        assert_eq!(read_columns("language = \"en\"\n", &default), default);
        assert_eq!(read_columns("", &default), default);
    }

    #[test]
    fn a_visible_list_is_read_in_the_order_it_is_written() {
        let text = "[columns]\nvisible = [\"name\", \"kind\", \"size\"]\n";
        assert_eq!(columns_of(text), "name,kind,size");
    }

    #[test]
    fn a_visible_list_without_a_name_column_gets_one_back() {
        let text = "[columns]\nvisible = [\"size\", \"owner\"]\n";
        assert_eq!(columns_of(text), "name,size,owner");
    }

    #[test]
    fn a_word_naming_no_column_is_skipped_rather_than_fatal() {
        // A settings file written by a later version must still start
        // this one, on the columns it does understand.
        let text = "[columns]\nvisible = [\"name\", \"tags\", \"size\"]\n";
        assert_eq!(columns_of(text), "name,size");
    }

    #[test]
    fn an_empty_visible_list_leaves_the_default_listing_alone() {
        let text = "[columns]\nvisible = []\nheader = false\n";
        let set = read_columns(text, &ColumnSet::default());
        assert_eq!(set.spec(), ColumnSet::default().spec());
        // The header half still takes: the two keys are independent.
        assert!(!set.header);
    }

    #[test]
    fn the_header_key_reads_the_spellings_a_person_writes() {
        for (written, expected) in [
            ("true", true),
            ("false", false),
            ("on", true),
            ("off", false),
            ("\"yes\"", true),
            ("0", false),
        ] {
            let text = format!("[columns]\nheader = {written}\n");
            assert_eq!(
                read_columns(&text, &ColumnSet::default()).header,
                expected,
                "header = {written}"
            );
        }
    }

    #[test]
    fn a_columns_key_outside_the_table_is_not_one_of_ours() {
        // A stray top-level `header = false` belongs to whoever wrote it.
        let text = "header = false\nvisible = [\"name\"]\n";
        let set = read_columns(text, &ColumnSet::default());
        assert_eq!(set, ColumnSet::default());
        // And so does one under somebody else's table.
        let text = "[experimental]\nheader = false\n";
        assert!(read_columns(text, &ColumnSet::default()).header);
    }

    #[test]
    fn writing_columns_keeps_every_line_it_does_not_own() {
        let before = "# my settings\nlanguage = \"zh-TW\"\n\n[columns]\n# which ones\nvisible = [\"name\"]\nheader = true\nfuture = 7\n";
        let set = ColumnSet::new([Column::Name, Column::Size, Column::Kind], false);
        let after = with_columns(before, &set);
        for kept in [
            "# my settings",
            "language = \"zh-TW\"",
            "# which ones",
            "future = 7",
        ] {
            assert!(after.contains(kept), "lost '{kept}' from:\n{after}");
        }
        assert_eq!(read_columns(&after, &ColumnSet::default()), set);
        assert_eq!(read_language(&after), Some("zh-TW"));
    }

    #[test]
    fn writing_columns_into_a_file_without_the_table_appends_one() {
        let set = ColumnSet::new([Column::Name, Column::Owner], true);
        let after = with_columns("language = \"en\"\n", &set);
        assert!(after.starts_with("language = \"en\"\n"), "{after}");
        assert_eq!(read_columns(&after, &ColumnSet::default()), set);
        // And the language is still top-level: the new table went last,
        // which is the only place it cannot capture an existing key.
        assert_eq!(read_language(&after), Some("en"));
    }

    #[test]
    fn a_language_written_after_a_columns_table_stays_top_level() {
        // The order that would break the file if `with_language` put its
        // key at the bottom: table first, language second.
        let with_table = with_columns("", &ColumnSet::default());
        let both = with_language(&with_table, Lang::ZhTw);
        assert_eq!(read_language(&both), Some("zh-TW"));
        assert_eq!(
            read_columns(&both, &ColumnSet::default()),
            ColumnSet::default()
        );
    }

    #[test]
    fn a_duplicated_columns_key_is_collapsed_rather_than_left_to_shadow() {
        let before =
            "[columns]\nheader = true\nheader = true\nvisible = [\"name\"]\nvisible = [\"name\"]\n";
        let after = with_columns(before, &ColumnSet::default());
        assert_eq!(after.matches("header =").count(), 1, "{after}");
        assert_eq!(after.matches("visible =").count(), 1, "{after}");
        assert_eq!(
            read_columns(&after, &ColumnSet::default()),
            ColumnSet::default()
        );
    }

    #[test]
    fn a_columns_table_followed_by_another_table_keeps_its_keys_inside() {
        let before = "[columns]\nvisible = [\"name\"]\n\n[experimental]\nthing = 1\n";
        let set = ColumnSet::new([Column::Name, Column::Kind], false);
        let after = with_columns(before, &set);
        assert!(after.contains("[experimental]"), "{after}");
        assert!(after.contains("thing = 1"), "{after}");
        assert_eq!(read_columns(&after, &ColumnSet::default()), set);
        // The two keys must land before the next header, or they belong
        // to `[experimental]` and are never read again.
        let lines: Vec<&str> = after.lines().collect();
        let table = lines
            .iter()
            .position(|l| l.trim() == "[experimental]")
            .unwrap();
        let header = lines
            .iter()
            .position(|l| l.starts_with("header ="))
            .unwrap();
        assert!(header < table, "{after}");
    }

    #[test]
    fn what_is_written_is_what_is_read_back_for_every_shape() {
        for header in [true, false] {
            for columns in [
                vec![Column::Name],
                vec![Column::Name, Column::Size, Column::Modified],
                Column::ALL.to_vec(),
            ] {
                let set = ColumnSet::new(columns, header);
                let text = with_columns("", &set);
                assert_eq!(read_columns(&text, &ColumnSet::default()), set, "{text}");
            }
        }
    }

    #[test]
    fn a_columns_round_trip_through_a_real_file_survives_the_language() {
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("nested").join("config.toml");
        save(&file, Lang::ZhTw).unwrap();
        let set = ColumnSet::new([Column::Name, Column::Kind, Column::Owner], false);
        save_columns(&file, &set).unwrap();
        let text = load(&file).unwrap();
        assert_eq!(read_language(&text), Some("zh-TW"));
        assert_eq!(read_columns(&text, &ColumnSet::default()), set);
        // And a later language change does not disturb the columns.
        save(&file, Lang::En).unwrap();
        let text = load(&file).unwrap();
        assert_eq!(read_language(&text), Some("en"));
        assert_eq!(read_columns(&text, &ColumnSet::default()), set);
    }

    #[test]
    fn a_missing_file_reads_as_no_setting() {
        let tmp = tempfile::tempdir().unwrap();
        assert_eq!(load(&tmp.path().join("absent.toml")), None);
    }
}
