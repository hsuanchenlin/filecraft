//! Interactive folder picker for `:move` with no destination path.
//!
//! The picker is a directory listing of folders only: the current
//! directory (`.`), its parent (`..` when there is one), and enterable
//! children. Cursor motion, descend, and ascend are pure against that
//! snapshot plus a re-read of the directory being entered. Selecting a
//! folder returns its path; the caller routes that through the existing
//! move confirmation. Nothing here touches a file.

use std::path::{Path, PathBuf};

use crate::fsops::{self, FsError};
use crate::nav::{self, EntryKind};

/// Rows the picker's own frame costs inside the listing area: two
/// border rows plus the dest header that names the targeted folder.
pub const FRAME_ROWS: usize = 3;
/// Columns the picker's own frame costs inside the listing area: two
/// border columns plus one column of padding on each side.
pub const FRAME_COLS: usize = 4;

/// What a picker row is. Files never appear.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PickerKind {
    /// The directory currently listed; selecting it keeps the file here.
    Current,
    /// The parent of the listed directory.
    Parent,
    Dir,
    SymlinkDir,
}

/// One folder row in the picker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PickerEntry {
    pub name: String,
    pub kind: PickerKind,
    /// Canonical path of this folder when it could be resolved.
    pub path: PathBuf,
}

impl PickerEntry {
    /// Name as shown: `/` suffix for directories, `@/` for symlink dirs.
    pub fn display_name(&self) -> String {
        match self.kind {
            PickerKind::Current => "./".to_string(),
            PickerKind::Parent => "../".to_string(),
            PickerKind::Dir => format!("{}/", self.name),
            PickerKind::SymlinkDir => format!("{}@/", self.name),
        }
    }

    /// `l` / Right enter this row. `.` is already the listed directory.
    pub fn is_enterable(&self) -> bool {
        !matches!(self.kind, PickerKind::Current)
    }
}

/// Folder-only navigation state for the move picker.
///
/// Independent of [`crate::nav::NavState`]: descending here must not
/// change the listing underneath, so cancelling lands on the same row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FolderPicker {
    pub cwd: PathBuf,
    pub entries: Vec<PickerEntry>,
    pub cursor: usize,
    pub source_name: String,
    pub source_path: PathBuf,
    pub show_hidden: bool,
}

impl FolderPicker {
    /// Open the picker on `start` (canonicalized), listing folders there.
    pub fn open(
        start: &Path,
        source_name: impl Into<String>,
        source_path: PathBuf,
        show_hidden: bool,
    ) -> Result<Self, FsError> {
        let cwd = std::fs::canonicalize(start).map_err(|e| fsops::io_error(start, &e))?;
        let mut picker = FolderPicker {
            cwd,
            entries: Vec::new(),
            cursor: 0,
            source_name: source_name.into(),
            source_path,
            show_hidden,
        };
        picker.reload()?;
        picker.focus_current();
        Ok(picker)
    }

    /// The folder under the cursor, if the listing is not empty.
    pub fn focused(&self) -> Option<&PickerEntry> {
        self.entries.get(self.cursor)
    }

    /// Canonical destination folder the confirmation will target.
    pub fn destination(&self) -> PathBuf {
        self.focused()
            .map(|entry| entry.path.clone())
            .unwrap_or_else(|| self.cwd.clone())
    }

    /// Header line naming the destination currently under the cursor.
    pub fn dest_line(&self) -> String {
        format!("dest: {}", self.destination().display())
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

    /// Descend into the focused folder. `.` is a no-op; `..` is [`Self::go_up`].
    pub fn enter_focused(&mut self) -> Result<(), FsError> {
        let Some(entry) = self.focused() else {
            return Ok(());
        };
        match entry.kind {
            PickerKind::Current => Ok(()),
            PickerKind::Parent => {
                let _ = self.go_up()?;
                Ok(())
            }
            PickerKind::Dir | PickerKind::SymlinkDir => {
                let dest = std::fs::canonicalize(&entry.path)
                    .map_err(|e| fsops::io_error(&entry.path, &e))?;
                self.cwd = dest;
                self.reload()?;
                self.focus_current();
                Ok(())
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
        self.cwd = dest;
        self.reload()?;
        if let Some(name) = from {
            if let Some(pos) = self.entries.iter().position(|entry| entry.name == name) {
                self.cursor = pos;
            } else {
                self.focus_current();
            }
        } else {
            self.focus_current();
        }
        Ok(true)
    }

    fn focus_current(&mut self) {
        self.cursor = self
            .entries
            .iter()
            .position(|entry| entry.kind == PickerKind::Current)
            .unwrap_or(0);
    }

    fn reload(&mut self) -> Result<(), FsError> {
        let listing = nav::read_directory(&self.cwd, self.show_hidden)?;
        let mut entries = Vec::new();
        entries.push(PickerEntry {
            name: ".".to_string(),
            kind: PickerKind::Current,
            path: self.cwd.clone(),
        });
        for entry in listing {
            if entry.is_parent {
                let Some(parent) = self.cwd.parent() else {
                    continue;
                };
                let path = std::fs::canonicalize(parent).unwrap_or_else(|_| parent.to_path_buf());
                entries.insert(
                    0,
                    PickerEntry {
                        name: "..".to_string(),
                        kind: PickerKind::Parent,
                        path,
                    },
                );
            } else if entry.is_enterable() {
                let child = self.cwd.join(&entry.name);
                let path = std::fs::canonicalize(&child).unwrap_or(child);
                let kind = match entry.kind {
                    EntryKind::SymlinkDir => PickerKind::SymlinkDir,
                    _ => PickerKind::Dir,
                };
                entries.push(PickerEntry {
                    name: entry.name,
                    kind,
                    path,
                });
            }
        }
        self.entries = entries;
        if self.cursor >= self.entries.len() {
            self.cursor = self.entries.len().saturating_sub(1);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn fixture() -> (tempfile::TempDir, FolderPicker) {
        let tmp = tempfile::tempdir().unwrap();
        fs::create_dir(tmp.path().join("archive")).unwrap();
        fs::create_dir(tmp.path().join("docs")).unwrap();
        fs::create_dir(tmp.path().join("archive/nested")).unwrap();
        fs::write(tmp.path().join("note.txt"), "n").unwrap();
        fs::write(tmp.path().join(".hidden.txt"), "h").unwrap();
        fs::create_dir(tmp.path().join(".secret")).unwrap();
        let src = tmp.path().join("note.txt");
        let picker = FolderPicker::open(tmp.path(), "note.txt", src, false).unwrap();
        (tmp, picker)
    }

    fn names(picker: &FolderPicker) -> Vec<String> {
        picker.entries.iter().map(|e| e.name.clone()).collect()
    }

    #[test]
    fn open_lists_current_parent_and_child_folders_not_files() {
        let (_tmp, picker) = fixture();
        let listed = names(&picker);
        assert!(listed.contains(&".".to_string()));
        assert!(listed.contains(&"..".to_string()));
        assert!(listed.contains(&"archive".to_string()));
        assert!(listed.contains(&"docs".to_string()));
        assert!(!listed.contains(&"note.txt".to_string()));
        assert!(!listed.contains(&".secret".to_string()));
        assert_eq!(picker.focused().unwrap().kind, PickerKind::Current);
        assert_eq!(picker.destination(), picker.cwd);
    }

    #[test]
    fn hidden_folders_follow_the_listing_flag() {
        let tmp = tempfile::tempdir().unwrap();
        fs::create_dir(tmp.path().join(".secret")).unwrap();
        fs::write(tmp.path().join("note.txt"), "n").unwrap();
        let src = tmp.path().join("note.txt");
        let hidden = FolderPicker::open(tmp.path(), "note.txt", src.clone(), false).unwrap();
        assert!(!names(&hidden).contains(&".secret".to_string()));
        let shown = FolderPicker::open(tmp.path(), "note.txt", src, true).unwrap();
        assert!(names(&shown).contains(&".secret".to_string()));
    }

    #[test]
    fn cursor_moves_and_clamps() {
        let (_tmp, mut picker) = fixture();
        picker.cursor_to_start();
        picker.move_cursor(-3);
        assert_eq!(picker.cursor, 0);
        picker.move_cursor(2);
        assert_eq!(picker.cursor, 2);
        picker.move_cursor(100);
        assert_eq!(picker.cursor, names(&picker).len() - 1);
        picker.cursor_to_start();
        assert_eq!(picker.cursor, 0);
        picker.cursor_to_end();
        assert_eq!(picker.cursor, names(&picker).len() - 1);
    }

    #[test]
    fn enter_child_lists_that_folder_and_focuses_current() {
        let (_tmp, mut picker) = fixture();
        let start = picker.cwd.clone();
        picker.cursor = picker
            .entries
            .iter()
            .position(|e| e.name == "archive")
            .unwrap();
        picker.enter_focused().unwrap();
        assert_eq!(picker.cwd, start.join("archive"));
        assert!(names(&picker).contains(&"nested".to_string()));
        assert_eq!(picker.focused().unwrap().kind, PickerKind::Current);
        assert_eq!(picker.destination(), picker.cwd);
    }

    #[test]
    fn go_up_selects_the_directory_we_left() {
        let (_tmp, mut picker) = fixture();
        let start = picker.cwd.clone();
        picker.cursor = picker
            .entries
            .iter()
            .position(|e| e.name == "archive")
            .unwrap();
        picker.enter_focused().unwrap();
        assert!(picker.go_up().unwrap());
        assert_eq!(picker.cwd, start);
        assert_eq!(picker.focused().unwrap().name, "archive");
        assert_eq!(picker.destination(), start.join("archive"));
    }

    #[test]
    fn dest_line_names_the_focused_folder() {
        let (_tmp, mut picker) = fixture();
        assert!(picker.dest_line().starts_with("dest: "));
        assert!(picker.dest_line().contains(picker.cwd.to_str().unwrap()));
        picker.cursor = picker
            .entries
            .iter()
            .position(|e| e.name == "docs")
            .unwrap();
        assert!(picker.destination().ends_with("docs"));
        assert!(picker.dest_line().contains("docs"));
    }

    #[test]
    fn enter_on_current_is_a_noop() {
        let (_tmp, mut picker) = fixture();
        let cwd = picker.cwd.clone();
        let cursor = picker.cursor;
        picker.enter_focused().unwrap();
        assert_eq!(picker.cwd, cwd);
        assert_eq!(picker.cursor, cursor);
    }

    #[test]
    fn display_names_carry_textual_kind_markers() {
        let (_tmp, picker) = fixture();
        let current = picker
            .entries
            .iter()
            .find(|e| e.kind == PickerKind::Current)
            .unwrap();
        assert_eq!(current.display_name(), "./");
        let parent = picker
            .entries
            .iter()
            .find(|e| e.kind == PickerKind::Parent)
            .unwrap();
        assert_eq!(parent.display_name(), "../");
        let dir = picker.entries.iter().find(|e| e.name == "docs").unwrap();
        assert_eq!(dir.display_name(), "docs/");
    }
}
