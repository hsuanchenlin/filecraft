//! The multi-file selector behind `:summarize` / `S`.
//!
//! A navigable listing of folders and summarizable documents. Selection is
//! a set that survives changing directory, so a summary can span folders;
//! the order files were picked in is kept, because the first one decides
//! where the summary is written.
//!
//! Like [`crate::picker`], this is independent of
//! [`NavState`](crate::nav::NavState): moving around in here must not move
//! the listing underneath, so cancelling lands on the same row. Nothing in
//! this module touches a file - it only reads directories.

use std::path::{Path, PathBuf};

use crate::fsops::{self, FsError};
use crate::i18n::Lang;
use crate::nav::{self, EntryKind};
use crate::summarize;

/// Rows the selector's own frame costs inside the listing area: two
/// border rows plus the header that counts what is selected.
pub const FRAME_ROWS: usize = 3;

/// What a selector row is. Files that cannot be summarized never appear.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SelectKind {
    /// The parent of the listed directory.
    Parent,
    Dir,
    SymlinkDir,
    /// A `.pdf`, `.md`, `.markdown`, or `.txt` file.
    File,
}

/// One row in the selector.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SelectEntry {
    pub name: String,
    pub kind: SelectKind,
    /// Canonical path of this row when it could be resolved.
    pub path: PathBuf,
}

impl SelectEntry {
    /// Name as shown: `/` for directories, `@/` for symlinked ones.
    pub fn display_name(&self) -> String {
        match self.kind {
            SelectKind::Parent => "../".to_string(),
            SelectKind::Dir => format!("{}/", self.name),
            SelectKind::SymlinkDir => format!("{}@/", self.name),
            SelectKind::File => self.name.clone(),
        }
    }

    /// `l` / Right enters folders; a file is a leaf.
    pub fn is_enterable(&self) -> bool {
        !matches!(self.kind, SelectKind::File)
    }

    pub fn is_file(&self) -> bool {
        matches!(self.kind, SelectKind::File)
    }
}

/// Why a Space press did not select anything.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToggleError {
    /// Space landed on a folder row.
    NotAFile,
    /// There was no row at all.
    NothingFocused,
}

impl ToggleError {
    /// Why the press did nothing, in `lang`.
    pub fn message(&self, lang: Lang) -> String {
        match self {
            ToggleError::NotAFile => lang.only_files_selectable(&summarize::summarizable_note()),
            ToggleError::NothingFocused => lang.nothing_focused().to_string(),
        }
    }
}

impl std::fmt::Display for ToggleError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message(Lang::En))
    }
}

/// What a Space press did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Toggled {
    Added(PathBuf),
    Removed(PathBuf),
}

/// Folder navigation plus an ordered selection set.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileSelector {
    pub cwd: PathBuf,
    pub entries: Vec<SelectEntry>,
    pub cursor: usize,
    /// Selected files, in the order they were picked. The first decides
    /// where the summary lands, so this is a list and not a set.
    pub chosen: Vec<PathBuf>,
    pub show_hidden: bool,
}

impl FileSelector {
    /// Open the selector on `start` (canonicalized).
    pub fn open(start: &Path, show_hidden: bool) -> Result<Self, FsError> {
        let cwd = std::fs::canonicalize(start).map_err(|e| fsops::io_error(start, &e))?;
        let entries = Self::read_entries(&cwd, show_hidden)?;
        Ok(FileSelector {
            cwd,
            entries,
            cursor: 0,
            chosen: Vec::new(),
            show_hidden,
        })
    }

    /// The row under the cursor, if the listing is not empty.
    pub fn focused(&self) -> Option<&SelectEntry> {
        self.entries.get(self.cursor)
    }

    /// Whether `path` is in the selection.
    pub fn is_chosen(&self, path: &Path) -> bool {
        self.chosen.iter().any(|p| p == path)
    }

    /// The `[x]` / `[ ]` box a row is drawn with, or blanks for a folder.
    /// A textual mark, so selection is never carried by color alone.
    pub fn mark(&self, entry: &SelectEntry) -> &'static str {
        if !entry.is_file() {
            return "   ";
        }
        if self.is_chosen(&entry.path) {
            "[x]"
        } else {
            "[ ]"
        }
    }

    /// The header: how many files are selected right now, and where the
    /// summary would land.
    pub fn header_line(&self, lang: Lang) -> String {
        lang.selector_header(self.chosen.len(), &summarize::summarizable_note())
    }

    /// The count the status bar shows while the selector is open.
    pub fn count(&self) -> usize {
        self.chosen.len()
    }

    /// Space on the focused row: select it, or unselect it.
    pub fn toggle_focused(&mut self) -> Result<Toggled, ToggleError> {
        let Some(entry) = self.focused() else {
            return Err(ToggleError::NothingFocused);
        };
        if !entry.is_file() {
            return Err(ToggleError::NotAFile);
        }
        let path = entry.path.clone();
        match self.chosen.iter().position(|p| *p == path) {
            Some(index) => {
                self.chosen.remove(index);
                Ok(Toggled::Removed(path))
            }
            None => {
                self.chosen.push(path.clone());
                Ok(Toggled::Added(path))
            }
        }
    }

    pub fn move_cursor(&mut self, delta: isize) {
        let len = self.entries.len();
        if len == 0 {
            self.cursor = 0;
            return;
        }
        let new = self.cursor as isize + delta;
        self.cursor = new.clamp(0, len as isize - 1) as usize;
    }

    pub fn cursor_to_start(&mut self) {
        self.cursor = 0;
    }

    pub fn cursor_to_end(&mut self) {
        self.cursor = self.entries.len().saturating_sub(1);
    }

    /// Descend into the focused folder. A file row is a no-op.
    pub fn enter_focused(&mut self) -> Result<(), FsError> {
        let Some(entry) = self.focused() else {
            return Ok(());
        };
        match entry.kind {
            SelectKind::File => Ok(()),
            SelectKind::Parent => {
                let _ = self.go_up()?;
                Ok(())
            }
            SelectKind::Dir | SelectKind::SymlinkDir => {
                let dest = std::fs::canonicalize(&entry.path)
                    .map_err(|e| fsops::io_error(&entry.path, &e))?;
                self.transition_to(dest)
            }
        }
    }

    /// Go to the parent directory, focusing the directory we left.
    ///
    /// Returns `Ok(false)` at the filesystem root.
    pub fn go_up(&mut self) -> Result<bool, FsError> {
        let Some(parent) = self.cwd.parent() else {
            return Ok(false);
        };
        let from = self
            .cwd
            .file_name()
            .map(|name| name.to_string_lossy().into_owned());
        let dest = std::fs::canonicalize(parent).map_err(|e| fsops::io_error(parent, &e))?;
        self.transition_to(dest)?;
        if let Some(name) = from {
            if let Some(pos) = self.entries.iter().position(|entry| entry.name == name) {
                self.cursor = pos;
            }
        }
        Ok(true)
    }

    fn transition_to(&mut self, cwd: PathBuf) -> Result<(), FsError> {
        let entries = Self::read_entries(&cwd, self.show_hidden)?;
        self.cwd = cwd;
        self.entries = entries;
        self.cursor = 0;
        Ok(())
    }

    /// Folders and summarizable files only, in the listing's own order.
    fn read_entries(cwd: &Path, show_hidden: bool) -> Result<Vec<SelectEntry>, FsError> {
        let listing = nav::read_directory(cwd, show_hidden)?;
        let mut entries = Vec::new();
        for entry in listing {
            if entry.is_parent {
                let Some(parent) = cwd.parent() else {
                    continue;
                };
                let path = std::fs::canonicalize(parent).unwrap_or_else(|_| parent.to_path_buf());
                entries.push(SelectEntry {
                    name: "..".to_string(),
                    kind: SelectKind::Parent,
                    path,
                });
                continue;
            }
            let child = cwd.join(&entry.name);
            match entry.kind {
                // `cwd` is canonical, so a real child already is too; only
                // a symlink needs resolving to name its target.
                EntryKind::Dir => entries.push(SelectEntry {
                    name: entry.name,
                    kind: SelectKind::Dir,
                    path: child,
                }),
                EntryKind::SymlinkDir => entries.push(SelectEntry {
                    name: entry.name,
                    kind: SelectKind::SymlinkDir,
                    path: std::fs::canonicalize(&child).unwrap_or(child),
                }),
                EntryKind::File | EntryKind::SymlinkFile if summarize::is_summarizable(&child) => {
                    let path = match entry.kind {
                        EntryKind::SymlinkFile => {
                            std::fs::canonicalize(&child).unwrap_or_else(|_| child.clone())
                        }
                        _ => child,
                    };
                    entries.push(SelectEntry {
                        name: entry.name,
                        kind: SelectKind::File,
                        path,
                    });
                }
                _ => {}
            }
        }
        Ok(entries)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::i18n::Lang;
    use std::fs;

    fn fixture() -> (tempfile::TempDir, FileSelector) {
        let tmp = tempfile::tempdir().unwrap();
        fs::create_dir(tmp.path().join("notes")).unwrap();
        fs::write(tmp.path().join("report.pdf"), "%PDF-1.4").unwrap();
        fs::write(tmp.path().join("readme.md"), "# hi").unwrap();
        fs::write(tmp.path().join("log.txt"), "log").unwrap();
        fs::write(tmp.path().join("book.markdown"), "# book").unwrap();
        fs::write(tmp.path().join("photo.png"), "png").unwrap();
        fs::write(tmp.path().join("main.rs"), "fn main() {}").unwrap();
        fs::write(tmp.path().join("notes/deep.md"), "# deep").unwrap();
        let selector = FileSelector::open(tmp.path(), false).unwrap();
        (tmp, selector)
    }

    fn names(selector: &FileSelector) -> Vec<String> {
        selector.entries.iter().map(|e| e.name.clone()).collect()
    }

    fn focus(selector: &mut FileSelector, name: &str) {
        selector.cursor = selector
            .entries
            .iter()
            .position(|e| e.name == name)
            .unwrap_or_else(|| panic!("row '{name}' not listed"));
    }

    #[test]
    fn lists_folders_and_summarizable_files_only() {
        let (_tmp, selector) = fixture();
        let listed = names(&selector);
        for shown in [
            "..",
            "notes",
            "report.pdf",
            "readme.md",
            "log.txt",
            "book.markdown",
        ] {
            assert!(
                listed.contains(&shown.to_string()),
                "{shown} should be listed"
            );
        }
        for hidden in ["photo.png", "main.rs"] {
            assert!(
                !listed.contains(&hidden.to_string()),
                "{hidden} must not be listed"
            );
        }
    }

    #[test]
    fn space_selects_and_unselects_a_file() {
        let (_tmp, mut selector) = fixture();
        focus(&mut selector, "readme.md");
        let path = selector.focused().unwrap().path.clone();
        assert_eq!(selector.toggle_focused(), Ok(Toggled::Added(path.clone())));
        assert!(selector.is_chosen(&path));
        assert_eq!(selector.count(), 1);
        assert_eq!(
            selector.toggle_focused(),
            Ok(Toggled::Removed(path.clone()))
        );
        assert!(!selector.is_chosen(&path));
        assert_eq!(selector.count(), 0);
    }

    #[test]
    fn space_on_a_folder_selects_nothing_and_says_why() {
        let (_tmp, mut selector) = fixture();
        focus(&mut selector, "notes");
        assert_eq!(selector.toggle_focused(), Err(ToggleError::NotAFile));
        assert_eq!(selector.count(), 0);
        let message = ToggleError::NotAFile.to_string();
        assert!(message.contains(".pdf"));
        assert!(message.contains(".markdown"));
    }

    #[test]
    fn selection_survives_changing_directory_and_keeps_its_order() {
        let (_tmp, mut selector) = fixture();
        focus(&mut selector, "log.txt");
        selector.toggle_focused().unwrap();
        focus(&mut selector, "report.pdf");
        selector.toggle_focused().unwrap();

        focus(&mut selector, "notes");
        selector.enter_focused().unwrap();
        assert!(names(&selector).contains(&"deep.md".to_string()));
        focus(&mut selector, "deep.md");
        selector.toggle_focused().unwrap();

        assert_eq!(selector.count(), 3);
        let picked: Vec<String> = selector
            .chosen
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert_eq!(picked, vec!["log.txt", "report.pdf", "deep.md"]);

        // And it is still all there after coming back up.
        assert!(selector.go_up().unwrap());
        assert_eq!(selector.count(), 3);
        assert_eq!(selector.focused().unwrap().name, "notes");
    }

    #[test]
    fn marks_are_textual_and_folders_have_none() {
        let (_tmp, mut selector) = fixture();
        focus(&mut selector, "readme.md");
        let file = selector.focused().unwrap().clone();
        assert_eq!(selector.mark(&file), "[ ]");
        selector.toggle_focused().unwrap();
        assert_eq!(selector.mark(&file), "[x]");
        focus(&mut selector, "notes");
        let folder = selector.focused().unwrap().clone();
        assert_eq!(selector.mark(&folder), "   ");
    }

    #[test]
    fn the_header_counts_what_is_selected() {
        let (_tmp, mut selector) = fixture();
        assert!(selector
            .header_line(Lang::En)
            .starts_with("selected: 0 files"));
        assert!(selector.header_line(Lang::En).contains(".pdf"));
        focus(&mut selector, "readme.md");
        selector.toggle_focused().unwrap();
        assert_eq!(selector.header_line(Lang::En), "selected: 1 file");
        focus(&mut selector, "log.txt");
        selector.toggle_focused().unwrap();
        assert_eq!(selector.header_line(Lang::En), "selected: 2 files");
    }

    #[test]
    fn cursor_moves_and_clamps() {
        let (_tmp, mut selector) = fixture();
        selector.move_cursor(-5);
        assert_eq!(selector.cursor, 0);
        selector.move_cursor(2);
        assert_eq!(selector.cursor, 2);
        selector.move_cursor(500);
        assert_eq!(selector.cursor, selector.entries.len() - 1);
        selector.cursor_to_start();
        assert_eq!(selector.cursor, 0);
        selector.cursor_to_end();
        assert_eq!(selector.cursor, selector.entries.len() - 1);
    }

    #[test]
    fn entering_a_file_row_is_a_noop() {
        let (_tmp, mut selector) = fixture();
        focus(&mut selector, "readme.md");
        let before = selector.clone();
        selector.enter_focused().unwrap();
        assert_eq!(selector, before);
    }

    #[test]
    fn hidden_files_follow_the_listing_flag() {
        let tmp = tempfile::tempdir().unwrap();
        fs::write(tmp.path().join(".secret.md"), "s").unwrap();
        let plain = FileSelector::open(tmp.path(), false).unwrap();
        assert!(!names(&plain).contains(&".secret.md".to_string()));
        let shown = FileSelector::open(tmp.path(), true).unwrap();
        assert!(names(&shown).contains(&".secret.md".to_string()));
    }

    #[test]
    fn display_names_carry_textual_kind_markers() {
        let (_tmp, selector) = fixture();
        let by_name = |name: &str| {
            selector
                .entries
                .iter()
                .find(|e| e.name == name)
                .unwrap()
                .display_name()
        };
        assert_eq!(by_name(".."), "../");
        assert_eq!(by_name("notes"), "notes/");
        assert_eq!(by_name("readme.md"), "readme.md");
    }

    #[test]
    fn a_failed_transition_leaves_the_selector_untouched() {
        let (tmp, mut selector) = fixture();
        focus(&mut selector, "readme.md");
        selector.toggle_focused().unwrap();
        let before = selector.clone();
        assert!(selector.transition_to(tmp.path().join("nope")).is_err());
        assert_eq!(selector, before);
    }
}
