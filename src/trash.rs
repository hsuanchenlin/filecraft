//! Move-to-Trash: Filecraft's only removal operation, and it is
//! recoverable.
//!
//! Rules enforced here:
//! - the entry is *moved* into the system Trash, never unlinked. Filecraft
//!   calls no `unlink`, no `remove_file`, and no `remove_dir_all` anywhere
//!   in the product;
//! - on macOS the move goes through `NSFileManager`'s
//!   `trashItemAtURL:resultingItemURL:error:`, so the item lands in
//!   `~/.Trash` (or the volume's `.Trashes`) and Finder can put it back.
//!   The Finder-scripting route is deliberately not used: it needs an
//!   Automation permission prompt and fails silently without one;
//! - [`check_trashable`] refuses the paths that must never be trashable -
//!   `..`, `.`, and the filesystem root - before any confirmation prompt
//!   is raised, so the user is never asked to confirm something Filecraft
//!   would then refuse.
//!
//! The system Trash sits behind the [`Trasher`] seam for the same reason
//! `update::Host` exists: every caller above it - the state machine, the
//! confirmation flow - is then testable without touching the real
//! `~/.Trash`.

use std::path::Path;
#[cfg(test)]
use std::path::PathBuf;

use crate::fsops::FsError;

/// The one capability the state machine needs: put an existing entry in
/// the trash, recoverably.
pub trait Trasher {
    /// Move `path` to the trash. `path` must already exist.
    fn trash(&self, path: &Path) -> Result<(), FsError>;

    /// Where trashed entries land, in words, for messages and help.
    fn destination(&self) -> &str;
}

/// The real system Trash.
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemTrasher;

/// The [`Trasher`] Filecraft ships with.
pub fn system() -> Box<dyn Trasher> {
    Box::new(SystemTrasher)
}

impl Trasher for SystemTrasher {
    #[cfg(target_os = "macos")]
    fn trash(&self, path: &Path) -> Result<(), FsError> {
        use ::trash::macos::{DeleteMethod, TrashContextExtMacos};

        let mut context = ::trash::TrashContext::default();
        context.set_delete_method(DeleteMethod::NsFileManager);
        context.delete(path).map_err(|e| map_error(path, &e))
    }

    #[cfg(not(target_os = "macos"))]
    fn trash(&self, _path: &Path) -> Result<(), FsError> {
        Err(FsError::Unsupported(
            "moving to the Trash is only supported on macOS",
        ))
    }

    fn destination(&self) -> &str {
        "the Trash"
    }
}

/// Translate a [`trash`] error into Filecraft's vocabulary. The upstream
/// `Display` is a `Debug` dump; the message log gets a sentence instead.
#[cfg(target_os = "macos")]
fn map_error(path: &Path, err: &::trash::Error) -> FsError {
    use ::trash::Error;
    match err {
        Error::CouldNotAccess { .. } => {
            if std::fs::symlink_metadata(path).is_ok() {
                FsError::PermissionDenied(path.to_path_buf())
            } else {
                FsError::NotFound(path.to_path_buf())
            }
        }
        Error::CanonicalizePath { original } => FsError::NotFound(original.clone()),
        Error::TargetedRoot => FsError::Refused {
            path: path.to_path_buf(),
            reason: "the filesystem root cannot be trashed",
        },
        Error::Os { code, description } => FsError::Io {
            path: path.to_path_buf(),
            message: format!("Trash refused this entry (os error {code}: {description})"),
        },
        other => FsError::Io {
            path: path.to_path_buf(),
            message: format!("Trash refused this entry ({other:?})"),
        },
    }
}

/// Refuse the paths that must never reach the trash, before the user is
/// asked to confirm anything. Purely lexical: `path` is expected to be an
/// already-absolutized entry path.
///
/// `..` is the important one - it is a real row in every non-root listing,
/// and it names the directory the user is standing under.
pub fn check_trashable(path: &Path) -> Result<(), FsError> {
    // The last *written* segment, not the resolved one: `Path::components`
    // silently drops a trailing `.`, and this guard exists to answer
    // "what does the selected row say" rather than "where does it point".
    let raw = path.as_os_str().to_string_lossy();
    let last = raw
        .rsplit(std::path::MAIN_SEPARATOR)
        .find(|part| !part.is_empty());
    match last {
        Some("..") => Err(FsError::Refused {
            path: path.to_path_buf(),
            reason: "'..' is the parent directory, not an entry - select a real entry",
        }),
        Some(".") => Err(FsError::Refused {
            path: path.to_path_buf(),
            reason: "'.' is the current directory, not an entry - select a real entry",
        }),
        // Nothing but separators: the filesystem root, or an empty path.
        None => Err(FsError::Refused {
            path: path.to_path_buf(),
            reason: "the filesystem root cannot be trashed",
        }),
        Some(_) => Ok(()),
    }
}

/// A trash can that is an ordinary directory.
///
/// Test-only, and deliberately a real move rather than a mock: "the entry
/// left the listing and is recoverable from the trash" is then an
/// assertion about files on disk, which is the property that matters.
#[cfg(test)]
pub struct DirTrasher {
    pub root: PathBuf,
}

#[cfg(test)]
impl DirTrasher {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        DirTrasher { root: root.into() }
    }

    /// Everything currently in the can, sorted.
    pub fn contents(&self) -> Vec<String> {
        let Ok(read) = std::fs::read_dir(&self.root) else {
            return Vec::new();
        };
        let mut names: Vec<String> = read
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }
}

#[cfg(test)]
impl Trasher for DirTrasher {
    fn trash(&self, path: &Path) -> Result<(), FsError> {
        let name = path.file_name().ok_or_else(|| FsError::Refused {
            path: path.to_path_buf(),
            reason: "the filesystem root cannot be trashed",
        })?;
        std::fs::create_dir_all(&self.root).map_err(|e| crate::fsops::io_error(&self.root, &e))?;
        let mut dst = self.root.join(name);
        let mut suffix = 2;
        while std::fs::symlink_metadata(&dst).is_ok() {
            dst = self
                .root
                .join(format!("{} {suffix}", name.to_string_lossy()));
            suffix += 1;
        }
        crate::fsops::safe_move(path, &dst)
    }

    fn destination(&self) -> &str {
        "the Trash"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parent_row_is_never_trashable() {
        let err = check_trashable(Path::new("/Users/me/docs/..")).unwrap_err();
        assert!(
            matches!(err, FsError::Refused { .. }),
            "'..' must be refused, got {err:?}"
        );
        assert!(err.to_string().contains("parent directory"), "{err}");
    }

    #[test]
    fn current_dir_row_is_never_trashable() {
        let err = check_trashable(Path::new("/Users/me/docs/.")).unwrap_err();
        assert!(matches!(err, FsError::Refused { .. }), "{err:?}");
        assert!(err.to_string().contains("current directory"), "{err}");
    }

    #[test]
    fn a_trailing_separator_does_not_hide_the_parent_row() {
        // A directory row is drawn with a trailing `/`; the guard must
        // read the same answer either way.
        for written in ["/Users/me/docs/../", "/Users/me/docs/.."] {
            assert!(
                check_trashable(Path::new(written)).is_err(),
                "'{written}' must be refused"
            );
        }
    }

    #[test]
    fn an_empty_path_names_no_entry() {
        assert!(check_trashable(Path::new("")).is_err());
    }

    #[test]
    fn the_filesystem_root_is_never_trashable() {
        let err = check_trashable(Path::new("/")).unwrap_err();
        assert!(matches!(err, FsError::Refused { .. }), "{err:?}");
        assert!(err.to_string().contains("root"), "{err}");
    }

    #[test]
    fn an_ordinary_entry_is_trashable() {
        assert!(check_trashable(Path::new("/Users/me/docs/notes.md")).is_ok());
        assert!(check_trashable(Path::new("/Users/me/docs")).is_ok());
        // A dotfile is an ordinary entry.
        assert!(check_trashable(Path::new("/Users/me/.zshrc")).is_ok());
        // So is a name that merely starts with dots.
        assert!(check_trashable(Path::new("/Users/me/...odd")).is_ok());
    }

    #[test]
    fn dir_trasher_moves_the_entry_and_keeps_it_recoverable() {
        let tmp = tempfile::tempdir().unwrap();
        let can = tempfile::tempdir().unwrap();
        let victim = tmp.path().join("notes.md");
        std::fs::write(&victim, "keep me").unwrap();

        let trasher = DirTrasher::new(can.path());
        trasher.trash(&victim).unwrap();

        assert!(!victim.exists(), "the entry must leave its directory");
        assert_eq!(trasher.contents(), vec!["notes.md".to_string()]);
        assert_eq!(
            std::fs::read_to_string(can.path().join("notes.md")).unwrap(),
            "keep me",
            "a trashed entry must still be readable - trashing is a move"
        );
    }

    #[test]
    fn dir_trasher_keeps_both_when_two_entries_share_a_name() {
        let tmp = tempfile::tempdir().unwrap();
        let can = tempfile::tempdir().unwrap();
        let trasher = DirTrasher::new(can.path());

        for body in ["first", "second"] {
            let victim = tmp.path().join("dup.txt");
            std::fs::write(&victim, body).unwrap();
            trasher.trash(&victim).unwrap();
        }

        assert_eq!(
            trasher.contents(),
            vec!["dup.txt".to_string(), "dup.txt 2".to_string()],
            "a name collision must never cost the earlier entry"
        );
    }

    #[test]
    fn dir_trasher_reports_a_missing_entry_instead_of_succeeding() {
        let can = tempfile::tempdir().unwrap();
        let trasher = DirTrasher::new(can.path());
        let err = trasher
            .trash(Path::new("/nope/does/not/exist"))
            .unwrap_err();
        assert!(matches!(err, FsError::NotFound(_)), "{err:?}");
    }

    /// The product half of every source file, with its `mod tests` cut
    /// off. Guard tests below assert about what Filecraft *ships*, not
    /// about what its fixtures do.
    fn shipped_source() -> Vec<(String, String)> {
        let src = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut out = Vec::new();
        for entry in std::fs::read_dir(&src).unwrap().flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("rs") {
                continue;
            }
            let body = std::fs::read_to_string(&path).unwrap();
            let shipped = match body.find("#[cfg(test)]\nmod tests {") {
                Some(cut) => body[..cut].to_string(),
                None => body,
            };
            out.push((
                path.file_name().unwrap().to_string_lossy().into_owned(),
                shipped,
            ));
        }
        assert!(out.len() > 5, "expected to find the source tree");
        out
    }

    /// The invariant the whole feature rests on: there is no code path in
    /// the shipped binary that unlinks a file or removes a tree. Deletion
    /// is a move into the Trash and nothing else.
    #[test]
    fn filecraft_never_calls_a_permanent_removal() {
        for (name, body) in shipped_source() {
            for banned in [
                "remove_file",
                "remove_dir",
                "remove_dir_all",
                "unlink(",
                "DeleteMethod::Finder",
            ] {
                // Prose about the rule is how the rule stays readable.
                let code: String = body
                    .lines()
                    .filter(|l| !l.trim_start().starts_with("//"))
                    .collect::<Vec<_>>()
                    .join("\n");
                assert!(
                    !code.contains(banned),
                    "{name} calls '{banned}'; removal must stay a move to the Trash"
                );
            }
        }
    }

    /// The real thing, proved by putting it back: a file goes to `~/.Trash`
    /// and is then recovered from there with a plain move. Nothing is
    /// unlinked, and the user's Trash is left exactly as it was found.
    #[test]
    #[cfg(target_os = "macos")]
    fn macos_trash_moves_the_file_somewhere_it_can_be_recovered_from() {
        let Some(home) = std::env::var_os("HOME").map(PathBuf::from) else {
            eprintln!("skipping: no HOME");
            return;
        };
        let can = home.join(".Trash");
        if !can.is_dir() {
            eprintln!("skipping: {} is not a directory", can.display());
            return;
        }

        let tmp = tempfile::tempdir().unwrap();
        // Unique, so the landing name in ~/.Trash is not uniquified and a
        // concurrent run cannot collide with this one.
        let name = format!(
            "filecraft-trash-test-{}-{:?}.txt",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );
        let victim = tmp.path().join(&name);
        std::fs::write(&victim, "recover me").unwrap();

        SystemTrasher.trash(&victim).unwrap();
        assert!(!victim.exists(), "the entry must leave its directory");

        let landed = can.join(&name);
        assert!(
            landed.exists(),
            "expected the entry in {}",
            landed.display()
        );
        assert_eq!(std::fs::read_to_string(&landed).unwrap(), "recover me");

        // Put it back. This test never deletes anything.
        std::fs::rename(&landed, &victim).unwrap();
        assert!(victim.exists(), "the entry must be recoverable");
    }
}
