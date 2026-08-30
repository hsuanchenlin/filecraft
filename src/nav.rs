//! Directory listings and navigation state (cursor, filter, hidden files).
//!
//! Listing snapshots are plain data; every state transition is a pure
//! function of that data, so navigation behavior is testable without a
//! terminal. Reading a directory is the only filesystem access here.

use std::path::{Path, PathBuf};
use std::time::SystemTime;

use crate::fsops::{self, FsError};
use crate::owner;

/// What kind of thing a directory entry is, symlinks kept distinct so the
/// UI can mark them and operations can decide whether to follow them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EntryKind {
    Dir,
    File,
    /// Symlink to a directory (target exists).
    SymlinkDir,
    /// Symlink to a regular file (target exists).
    SymlinkFile,
    /// Symlink whose target is missing or unresolvable.
    SymlinkBroken,
    /// Sockets, FIFOs, devices, and anything else.
    Other,
}

/// One row in the listing. `is_parent` marks the synthetic `..` entry.
///
/// Everything a column can say about an entry is read once, here, off
/// the `stat` the listing already had to do. Nothing downstream touches
/// the filesystem again to draw a row: [`crate::columns`] is a pure
/// function of this snapshot, which is what lets a whole screen be
/// asserted without a TTY.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub name: String,
    pub kind: EntryKind,
    pub size: u64,
    pub modified: Option<SystemTime>,
    /// Birth time, where the filesystem records one. `None` on a
    /// filesystem that does not - the `created` column then falls back
    /// to [`Entry::modified`], which is decided in [`crate::columns`] so
    /// the fallback itself is testable.
    pub created: Option<SystemTime>,
    /// The entry's own `st_mode`, as `ls -l` reads it: for a symlink
    /// this is the link's mode, not its target's.
    pub mode: Option<u32>,
    /// Owning user, by name where the system database knows one and by
    /// number where it does not.
    pub owner: Option<String>,
    /// Owning group, resolved the same way.
    pub group: Option<String>,
    pub symlink_target: Option<PathBuf>,
    pub is_parent: bool,
}

impl Entry {
    /// Directories and symlinks-to-directories can be entered.
    pub fn is_enterable(&self) -> bool {
        matches!(self.kind, EntryKind::Dir | EntryKind::SymlinkDir)
    }

    /// Regular files and symlinks-to-files can be edited/previewed as text.
    pub fn is_file_like(&self) -> bool {
        matches!(self.kind, EntryKind::File | EntryKind::SymlinkFile)
    }

    /// Name as shown in the list: `/` suffix for directories, `@` for
    /// symlinks - textual markers so kind never depends on color alone.
    pub fn display_name(&self) -> String {
        match self.kind {
            EntryKind::Dir => format!("{}/", self.name),
            EntryKind::SymlinkDir => format!("{}@/", self.name),
            EntryKind::SymlinkFile => format!("{}@", self.name),
            EntryKind::SymlinkBroken => format!("{}@!", self.name),
            _ => self.name.clone(),
        }
    }
}

/// Everything one `stat` tells the listing, beyond the kind and size the
/// sort needs: the two timestamps and the ownership every optional
/// column reads.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct Facts {
    modified: Option<SystemTime>,
    created: Option<SystemTime>,
    mode: Option<u32>,
    owner: Option<String>,
    group: Option<String>,
}

/// Read the facts off metadata already in hand.
///
/// The owner and group are resolved to names here rather than at draw
/// time because [`owner`] memoizes: a directory of ten thousand files
/// owned by one person costs one lookup, and the drawing layer stays a
/// pure function of the snapshot.
fn facts_of(meta: &std::fs::Metadata) -> Facts {
    let mut facts = Facts {
        modified: meta.modified().ok(),
        // A filesystem with no birth time is not an error: the `created`
        // column falls back to the modification time.
        created: meta.created().ok(),
        ..Facts::default()
    };
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        facts.mode = Some(meta.mode());
        facts.owner = Some(owner::user(meta.uid()));
        facts.group = Some(owner::group(meta.gid()));
    }
    facts
}

fn classify(path: &Path) -> (EntryKind, u64, Facts, Option<PathBuf>) {
    let Ok(lmeta) = std::fs::symlink_metadata(path) else {
        return (EntryKind::Other, 0, Facts::default(), None);
    };
    // `ls -l` reports a symlink's own mode and its own dates, not its
    // target's, and so does the listing.
    let facts = facts_of(&lmeta);
    if lmeta.is_symlink() {
        let target = std::fs::read_link(path).ok();
        return match std::fs::metadata(path) {
            Ok(tmeta) if tmeta.is_dir() => (EntryKind::SymlinkDir, tmeta.len(), facts, target),
            Ok(tmeta) if tmeta.is_file() => (EntryKind::SymlinkFile, tmeta.len(), facts, target),
            Ok(_) => (EntryKind::Other, 0, facts, target),
            Err(_) => (EntryKind::SymlinkBroken, 0, facts, target),
        };
    }
    if lmeta.is_dir() {
        (EntryKind::Dir, 0, facts, None)
    } else if lmeta.is_file() {
        (EntryKind::File, lmeta.len(), facts, None)
    } else {
        (EntryKind::Other, lmeta.len(), facts, None)
    }
}

/// Read `dir` into a sorted listing: `..` first (unless at the root), then
/// directories, then everything else, each group case-insensitively by
/// name. Dotfiles are skipped unless `show_hidden`.
pub fn read_directory(dir: &Path, show_hidden: bool) -> Result<Vec<Entry>, FsError> {
    let read = std::fs::read_dir(dir).map_err(|e| fsops::io_error(dir, &e))?;
    let mut entries: Vec<Entry> = Vec::new();
    for item in read {
        let item = item.map_err(|e| fsops::io_error(dir, &e))?;
        let name = item.file_name().to_string_lossy().into_owned();
        if !show_hidden && name.starts_with('.') {
            continue;
        }
        let (kind, size, facts, symlink_target) = classify(&item.path());
        entries.push(Entry {
            name,
            kind,
            size,
            modified: facts.modified,
            created: facts.created,
            mode: facts.mode,
            owner: facts.owner,
            group: facts.group,
            symlink_target,
            is_parent: false,
        });
    }
    entries.sort_by(|a, b| {
        let a_dir = a.is_enterable();
        let b_dir = b.is_enterable();
        b_dir
            .cmp(&a_dir)
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
            .then_with(|| a.name.cmp(&b.name))
    });
    if dir.parent().is_some() {
        entries.insert(
            0,
            Entry {
                name: "..".to_string(),
                kind: EntryKind::Dir,
                size: 0,
                modified: None,
                created: None,
                mode: None,
                owner: None,
                group: None,
                symlink_target: None,
                is_parent: true,
            },
        );
    }
    Ok(entries)
}

/// Cursor + filter state over a directory snapshot.
///
/// The cursor indexes into [`NavState::visible`], the filtered view, so it
/// always points at something the user can see.
#[derive(Debug, Clone)]
pub struct NavState {
    pub cwd: PathBuf,
    pub entries: Vec<Entry>,
    pub cursor: usize,
    pub filter: String,
    pub show_hidden: bool,
}

impl NavState {
    /// Open `start` (canonicalized) as the initial directory.
    pub fn new(start: &Path) -> Result<Self, FsError> {
        let cwd = std::fs::canonicalize(start).map_err(|e| fsops::io_error(start, &e))?;
        let entries = read_directory(&cwd, false)?;
        Ok(NavState {
            cwd,
            entries,
            cursor: 0,
            filter: String::new(),
            show_hidden: false,
        })
    }

    /// Indices (into `entries`) of rows that pass the filter. The `..` row
    /// always passes so the user can never filter themselves into a trap.
    /// Matching is case-insensitive substring.
    pub fn visible(&self) -> Vec<usize> {
        if self.filter.is_empty() {
            return (0..self.entries.len()).collect();
        }
        let needle = self.filter.to_lowercase();
        self.entries
            .iter()
            .enumerate()
            .filter(|(_, e)| e.is_parent || e.name.to_lowercase().contains(&needle))
            .map(|(i, _)| i)
            .collect()
    }

    /// The entry under the cursor, if any row is visible.
    pub fn selected(&self) -> Option<&Entry> {
        let visible = self.visible();
        visible.get(self.cursor).map(|&i| &self.entries[i])
    }

    pub fn move_cursor(&mut self, delta: isize) {
        let len = self.visible().len();
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
        self.cursor = self.visible().len().saturating_sub(1);
    }

    /// Replace the filter and keep the cursor in range.
    pub fn set_filter(&mut self, filter: String) {
        self.filter = filter;
        let len = self.visible().len();
        if self.cursor >= len {
            self.cursor = len.saturating_sub(1);
        }
    }

    /// Toggle dotfiles and re-read the directory.
    pub fn toggle_hidden(&mut self) -> Result<(), FsError> {
        self.show_hidden = !self.show_hidden;
        self.refresh()
    }

    /// Re-read the current directory, keeping the selection on the same
    /// name when it still exists. If the directory itself has vanished,
    /// walk up to the nearest existing ancestor.
    pub fn refresh(&mut self) -> Result<(), FsError> {
        let keep = self.selected().map(|e| e.name.clone());
        let mut dir = self.cwd.clone();
        loop {
            match read_directory(&dir, self.show_hidden) {
                Ok(entries) => {
                    self.cwd = dir;
                    self.entries = entries;
                    match keep.as_deref().and_then(|name| self.index_of(name)) {
                        Some(pos) => self.cursor = pos,
                        None => {
                            let len = self.visible().len();
                            if self.cursor >= len {
                                self.cursor = len.saturating_sub(1);
                            }
                        }
                    }
                    return Ok(());
                }
                Err(FsError::NotFound(_)) => match dir.parent() {
                    Some(parent) => dir = parent.to_path_buf(),
                    None => return Err(FsError::NotFound(self.cwd.clone())),
                },
                Err(other) => return Err(other),
            }
        }
    }

    /// Enter `dir` (already canonical), optionally selecting `select` by
    /// name. Clears the filter.
    pub fn change_dir(&mut self, dir: PathBuf, select: Option<&str>) -> Result<(), FsError> {
        let entries = read_directory(&dir, self.show_hidden)?;
        self.cwd = dir;
        self.entries = entries;
        // The filter belongs to the listing it filtered.
        self.filter.clear();
        self.cursor = 0;
        if let Some(name) = select {
            if let Some(pos) = self.index_of(name) {
                self.cursor = pos;
            }
        }
        Ok(())
    }

    /// Go to the parent directory, selecting the directory we left.
    pub fn go_up(&mut self) -> Result<bool, FsError> {
        let Some(parent) = self.cwd.parent().map(Path::to_path_buf) else {
            return Ok(false);
        };
        let from = self
            .cwd
            .file_name()
            .map(|n| n.to_string_lossy().into_owned());
        self.change_dir(parent, from.as_deref())?;
        Ok(true)
    }

    /// Position of `name` in the *visible* list.
    fn index_of(&self, name: &str) -> Option<usize> {
        self.visible()
            .iter()
            .position(|&i| self.entries[i].name == name)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn fixture() -> (tempfile::TempDir, NavState) {
        let tmp = tempfile::tempdir().unwrap();
        fs::create_dir(tmp.path().join("beta dir")).unwrap();
        fs::create_dir(tmp.path().join("Alpha")).unwrap();
        fs::write(tmp.path().join("zeta.txt"), "z").unwrap();
        fs::write(tmp.path().join("apple.md"), "a").unwrap();
        fs::write(tmp.path().join(".hidden"), "h").unwrap();
        fs::write(tmp.path().join("ünïcødé 檔案.txt"), "u").unwrap();
        let nav = NavState::new(tmp.path()).unwrap();
        (tmp, nav)
    }

    fn names(nav: &NavState) -> Vec<String> {
        nav.visible()
            .iter()
            .map(|&i| nav.entries[i].name.clone())
            .collect()
    }

    #[test]
    fn listing_sorted_dirs_first_then_case_insensitive() {
        let (_tmp, nav) = fixture();
        assert_eq!(
            names(&nav),
            vec![
                "..",
                "Alpha",
                "beta dir",
                "apple.md",
                "zeta.txt",
                "ünïcødé 檔案.txt"
            ]
        );
    }

    #[test]
    fn hidden_files_excluded_by_default_and_toggleable() {
        let (_tmp, mut nav) = fixture();
        assert!(!names(&nav).contains(&".hidden".to_string()));
        nav.toggle_hidden().unwrap();
        assert!(names(&nav).contains(&".hidden".to_string()));
        nav.toggle_hidden().unwrap();
        assert!(!names(&nav).contains(&".hidden".to_string()));
    }

    #[test]
    fn cursor_moves_and_clamps() {
        let (_tmp, mut nav) = fixture();
        assert_eq!(nav.cursor, 0);
        nav.move_cursor(-3);
        assert_eq!(nav.cursor, 0);
        nav.move_cursor(2);
        assert_eq!(nav.cursor, 2);
        nav.move_cursor(100);
        assert_eq!(nav.cursor, names(&nav).len() - 1);
        nav.cursor_to_start();
        assert_eq!(nav.cursor, 0);
        nav.cursor_to_end();
        assert_eq!(nav.cursor, names(&nav).len() - 1);
    }

    #[test]
    fn filter_narrows_and_keeps_parent_row() {
        let (_tmp, mut nav) = fixture();
        nav.set_filter("APP".to_string());
        assert_eq!(names(&nav), vec!["..", "apple.md"]);
        nav.set_filter(String::new());
        assert_eq!(names(&nav).len(), 6);
    }

    #[test]
    fn filter_clamps_cursor() {
        let (_tmp, mut nav) = fixture();
        nav.cursor_to_end();
        nav.set_filter("apple".to_string());
        assert!(nav.cursor < names(&nav).len());
        assert_eq!(nav.selected().unwrap().name, "apple.md");
    }

    #[test]
    fn filter_matches_unicode() {
        let (_tmp, mut nav) = fixture();
        nav.set_filter("檔案".to_string());
        assert_eq!(names(&nav), vec!["..", "ünïcødé 檔案.txt"]);
    }

    #[test]
    fn enter_and_go_up_reselects_origin() {
        let (_tmp, mut nav) = fixture();
        let sub = nav.cwd.join("beta dir");
        nav.change_dir(sub.clone(), None).unwrap();
        assert_eq!(nav.cwd, sub);
        assert!(nav.go_up().unwrap());
        assert_eq!(nav.selected().unwrap().name, "beta dir");
    }

    #[test]
    fn change_dir_clears_filter() {
        let (_tmp, mut nav) = fixture();
        nav.set_filter("beta".to_string());
        let sub = nav.cwd.join("beta dir");
        nav.change_dir(sub, None).unwrap();
        assert!(nav.filter.is_empty());
    }

    #[test]
    fn go_up_at_root_is_a_noop() {
        let mut nav = NavState::new(Path::new("/")).unwrap();
        assert!(!nav.go_up().unwrap());
        assert_eq!(nav.cwd, PathBuf::from("/"));
    }

    #[test]
    fn root_listing_has_no_parent_row() {
        let nav = NavState::new(Path::new("/")).unwrap();
        assert!(!nav.entries.iter().any(|e| e.is_parent));
    }

    #[test]
    fn refresh_keeps_selection_by_name() {
        let (tmp, mut nav) = fixture();
        nav.set_filter(String::new());
        let pos = names(&nav).iter().position(|n| n == "zeta.txt").unwrap();
        nav.cursor = pos;
        fs::write(tmp.path().join("aaa_new.txt"), "n").unwrap();
        nav.refresh().unwrap();
        assert_eq!(nav.selected().unwrap().name, "zeta.txt");
    }

    #[test]
    fn refresh_walks_up_when_cwd_vanishes() {
        let tmp = tempfile::tempdir().unwrap();
        let sub = tmp.path().join("doomed");
        fs::create_dir(&sub).unwrap();
        let mut nav = NavState::new(&sub).unwrap();
        fs::remove_dir(&sub).unwrap();
        nav.refresh().unwrap();
        assert_eq!(nav.cwd, tmp.path().canonicalize().unwrap());
    }

    #[cfg(unix)]
    #[test]
    fn unreadable_directory_is_an_error_not_a_panic() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let locked = tmp.path().join("locked");
        fs::create_dir(&locked).unwrap();
        let mut perms = fs::metadata(&locked).unwrap().permissions();
        perms.set_mode(0o000);
        fs::set_permissions(&locked, perms.clone()).unwrap();

        let result = read_directory(&locked, false);
        // Root can read anything; only assert when the OS actually refused.
        if let Err(err) = result {
            assert!(matches!(err, FsError::PermissionDenied(_)));
        }

        perms.set_mode(0o755);
        fs::set_permissions(&locked, perms).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn symlinks_classified_and_marked() {
        let tmp = tempfile::tempdir().unwrap();
        fs::create_dir(tmp.path().join("real_dir")).unwrap();
        fs::write(tmp.path().join("real_file"), "x").unwrap();
        std::os::unix::fs::symlink(tmp.path().join("real_dir"), tmp.path().join("link_dir"))
            .unwrap();
        std::os::unix::fs::symlink(tmp.path().join("real_file"), tmp.path().join("link_file"))
            .unwrap();
        std::os::unix::fs::symlink(tmp.path().join("gone"), tmp.path().join("link_broken"))
            .unwrap();

        let entries = read_directory(tmp.path(), false).unwrap();
        let kind = |name: &str| {
            entries
                .iter()
                .find(|e| e.name == name)
                .map(|e| e.kind.clone())
                .unwrap()
        };
        assert_eq!(kind("link_dir"), EntryKind::SymlinkDir);
        assert_eq!(kind("link_file"), EntryKind::SymlinkFile);
        assert_eq!(kind("link_broken"), EntryKind::SymlinkBroken);

        let broken = entries.iter().find(|e| e.name == "link_broken").unwrap();
        assert_eq!(broken.display_name(), "link_broken@!");
        assert!(broken.symlink_target.is_some());
    }

    #[test]
    fn every_entry_carries_what_a_column_needs_to_draw_it() {
        let (_tmp, nav) = fixture();
        let entry = nav
            .entries
            .iter()
            .find(|e| e.name == "apple.md")
            .expect("apple.md");
        assert!(entry.modified.is_some());
        // macOS records a birth time, so a file just written has one.
        // A filesystem without one is not an error - the `created`
        // column falls back to `modified`.
        if let Some(created) = entry.created {
            assert!(created <= SystemTime::now());
        }
        #[cfg(unix)]
        {
            assert!(entry.mode.is_some(), "a unix listing has a mode");
            assert!(entry.owner.is_some(), "a unix listing has an owner");
            assert!(entry.group.is_some(), "a unix listing has a group");
            // A regular file, and the bits `ls -l` would print.
            assert_eq!(entry.mode.unwrap() & 0o170000, 0o100000);
        }
    }

    #[test]
    fn the_parent_row_carries_no_metadata_of_its_own() {
        // `..` is synthetic: it is a way back, not an entry that was
        // read, so every column that describes a file is blank on it.
        let (_tmp, nav) = fixture();
        let parent = nav.entries.iter().find(|e| e.is_parent).expect("..");
        assert_eq!(parent.modified, None);
        assert_eq!(parent.created, None);
        assert_eq!(parent.mode, None);
        assert_eq!(parent.owner, None);
        assert_eq!(parent.group, None);
    }

    #[cfg(unix)]
    #[test]
    fn a_symlink_reports_its_own_mode_the_way_ls_does() {
        let tmp = tempfile::tempdir().unwrap();
        fs::write(tmp.path().join("real"), "x").unwrap();
        std::os::unix::fs::symlink(tmp.path().join("real"), tmp.path().join("link")).unwrap();
        let entries = read_directory(tmp.path(), false).unwrap();
        let link = entries.iter().find(|e| e.name == "link").unwrap();
        // `l`, not `-`: the link's own mode, not its target's.
        assert_eq!(link.mode.unwrap() & 0o170000, 0o120000);
    }

    #[cfg(unix)]
    #[test]
    fn a_files_owner_is_the_name_the_running_user_has() {
        let tmp = tempfile::tempdir().unwrap();
        fs::write(tmp.path().join("mine"), "x").unwrap();
        let entries = read_directory(tmp.path(), false).unwrap();
        let mine = entries.iter().find(|e| e.name == "mine").unwrap();
        // SAFETY: `getuid` reads a process attribute and cannot fail.
        let uid = unsafe { libc::getuid() } as u32;
        assert_eq!(
            mine.owner.as_deref(),
            Some(crate::owner::user(uid).as_str())
        );
    }

    #[test]
    fn missing_directory_read_is_not_found() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(matches!(
            read_directory(&tmp.path().join("nope"), false),
            Err(FsError::NotFound(_))
        ));
    }
}
