//! Path canonicalization, validation, and safe file operations.
//!
//! Safety rules enforced here:
//! - every operation target is expanded (`~`), absolutized against the
//!   current directory, and lexically normalized before use, so the user
//!   always sees the real destination in confirmations;
//! - moves and renames never overwrite an existing entry (the only
//!   exception is a pure case-change of the same file on a
//!   case-insensitive filesystem);
//! - there is no delete operation of any kind in v0;
//! - cross-volume moves are refused rather than emulated with
//!   copy+delete.

use std::io;
use std::path::{Component, Path, PathBuf};

/// Errors from path validation and file operations, with user-facing text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FsError {
    NotFound(PathBuf),
    NotADirectory(PathBuf),
    NotAFile(PathBuf),
    PermissionDenied(PathBuf),
    AlreadyExists(PathBuf),
    CrossDevice,
    InvalidName { name: String, reason: &'static str },
    HomeNotFound,
    Io { path: PathBuf, message: String },
}

impl std::fmt::Display for FsError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FsError::NotFound(p) => write!(f, "not found: {}", p.display()),
            FsError::NotADirectory(p) => {
                write!(f, "not a directory: {}", p.display())
            }
            FsError::NotAFile(p) => {
                write!(f, "not a regular file: {}", p.display())
            }
            FsError::PermissionDenied(p) => {
                write!(f, "permission denied: {}", p.display())
            }
            FsError::AlreadyExists(p) => {
                write!(f, "destination already exists: {}", p.display())
            }
            FsError::CrossDevice => write!(
                f,
                "cross-volume move is not supported in v0; \
                 destination must be on the same volume"
            ),
            FsError::InvalidName { name, reason } => {
                write!(f, "invalid name '{name}': {reason}")
            }
            FsError::HomeNotFound => {
                write!(f, "cannot expand '~': home directory unknown")
            }
            FsError::Io { path, message } => {
                write!(f, "{message}: {}", path.display())
            }
        }
    }
}

impl std::error::Error for FsError {}

/// Map an [`io::Error`] for `path` onto the closest [`FsError`].
pub fn io_error(path: &Path, err: &io::Error) -> FsError {
    match err.kind() {
        io::ErrorKind::NotFound => FsError::NotFound(path.to_path_buf()),
        io::ErrorKind::PermissionDenied => FsError::PermissionDenied(path.to_path_buf()),
        io::ErrorKind::AlreadyExists => FsError::AlreadyExists(path.to_path_buf()),
        io::ErrorKind::CrossesDevices => FsError::CrossDevice,
        io::ErrorKind::NotADirectory => FsError::NotADirectory(path.to_path_buf()),
        _ => FsError::Io {
            path: path.to_path_buf(),
            message: err.to_string(),
        },
    }
}

/// Expand a leading `~` or `~/…` using `home`.
///
/// `~user` forms are rejected: Filecraft only ever acts as the invoking
/// user. Anything not starting with `~` is returned unchanged.
pub fn expand_tilde(input: &str, home: Option<&Path>) -> Result<PathBuf, FsError> {
    if input == "~" {
        return home.map(Path::to_path_buf).ok_or(FsError::HomeNotFound);
    }
    if let Some(rest) = input.strip_prefix("~/") {
        let home = home.ok_or(FsError::HomeNotFound)?;
        return Ok(home.join(rest));
    }
    if input.starts_with('~') {
        return Err(FsError::InvalidName {
            name: input.to_string(),
            reason: "'~user' expansion is not supported",
        });
    }
    Ok(PathBuf::from(input))
}

/// Lexically normalize a path: resolve `.` and `..` components without
/// touching the filesystem. `..` at the root stays at the root.
pub fn lexical_normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                // Only pop a real name; keep prefix/root, and keep leading
                // `..` on relative paths.
                let popped = matches!(out.components().next_back(), Some(Component::Normal(_)));
                if popped {
                    out.pop();
                } else if !matches!(out.components().next_back(), Some(Component::RootDir)) {
                    out.push("..");
                }
            }
            other => out.push(other.as_os_str()),
        }
    }
    if out.as_os_str().is_empty() {
        out.push(".");
    }
    out
}

/// Expand `~`, make absolute against `base`, and lexically normalize.
/// Purely lexical: the result may or may not exist.
pub fn absolutize(base: &Path, input: &str, home: Option<&Path>) -> Result<PathBuf, FsError> {
    if input.is_empty() {
        return Err(FsError::InvalidName {
            name: String::new(),
            reason: "empty path",
        });
    }
    let expanded = expand_tilde(input, home)?;
    let joined = if expanded.is_absolute() {
        expanded
    } else {
        base.join(expanded)
    };
    Ok(lexical_normalize(&joined))
}

/// Resolve a `cd` target: expand/absolutize, then canonicalize and require
/// an existing directory. Returns the canonical (symlink-resolved) path.
pub fn canonical_dir(base: &Path, input: &str, home: Option<&Path>) -> Result<PathBuf, FsError> {
    let target = absolutize(base, input, home)?;
    let canonical = std::fs::canonicalize(&target).map_err(|e| io_error(&target, &e))?;
    let meta = std::fs::metadata(&canonical).map_err(|e| io_error(&canonical, &e))?;
    if !meta.is_dir() {
        return Err(FsError::NotADirectory(canonical));
    }
    Ok(canonical)
}

/// Resolve a `move` destination for an entry named `src_name`.
///
/// If the destination exists and is a directory (symlinks followed), the
/// entry keeps its name and moves into it; otherwise the destination is the
/// full target path and its parent directory must already exist.
pub fn canonical_move_target(
    base: &Path,
    input: &str,
    src_name: &str,
    home: Option<&Path>,
) -> Result<PathBuf, FsError> {
    let target = absolutize(base, input, home)?;
    if let Ok(meta) = std::fs::metadata(&target) {
        if meta.is_dir() {
            let canonical = std::fs::canonicalize(&target).map_err(|e| io_error(&target, &e))?;
            return Ok(canonical.join(src_name));
        }
    }
    let parent = target.parent().ok_or_else(|| FsError::Io {
        path: target.clone(),
        message: "destination has no parent directory".to_string(),
    })?;
    let file_name = target
        .file_name()
        .ok_or_else(|| FsError::InvalidName {
            name: input.to_string(),
            reason: "destination has no file name",
        })?
        .to_os_string();
    let canonical_parent = std::fs::canonicalize(parent).map_err(|e| io_error(parent, &e))?;
    let parent_meta =
        std::fs::metadata(&canonical_parent).map_err(|e| io_error(&canonical_parent, &e))?;
    if !parent_meta.is_dir() {
        return Err(FsError::NotADirectory(canonical_parent));
    }
    Ok(canonical_parent.join(file_name))
}

/// Validate a `rename` target name: a single path component, nothing else.
pub fn validate_new_name(name: &str) -> Result<(), FsError> {
    let invalid = |reason| {
        Err(FsError::InvalidName {
            name: name.to_string(),
            reason,
        })
    };
    if name.is_empty() {
        return invalid("empty name");
    }
    if name == "." || name == ".." {
        return invalid("'.' and '..' are reserved");
    }
    if name.contains('/') {
        return invalid("must not contain '/' (rename stays in the same directory)");
    }
    if name.contains('\0') {
        return invalid("must not contain NUL");
    }
    Ok(())
}

/// Whether two existing paths refer to the same underlying file (device and
/// inode), without following symlinks.
#[cfg(unix)]
pub fn same_file(a: &Path, b: &Path) -> bool {
    use std::os::unix::fs::MetadataExt;
    match (std::fs::symlink_metadata(a), std::fs::symlink_metadata(b)) {
        (Ok(ma), Ok(mb)) => ma.dev() == mb.dev() && ma.ino() == mb.ino(),
        _ => false,
    }
}

#[cfg(not(unix))]
pub fn same_file(_a: &Path, _b: &Path) -> bool {
    false
}

/// Move or rename `src` to `dst` without ever overwriting.
///
/// `src` must exist (as itself - a broken symlink is movable). `dst` must
/// not exist, except when it is the same file as `src` (a pure case change
/// on a case-insensitive filesystem). Cross-volume moves are refused.
///
/// The existence check happens just before the rename, so a race with an
/// external writer is possible in principle; Filecraft accepts that narrow
/// window in exchange for portable, EXDEV-aware `rename` semantics.
pub fn safe_move(src: &Path, dst: &Path) -> Result<(), FsError> {
    std::fs::symlink_metadata(src).map_err(|e| io_error(src, &e))?;
    if src == dst {
        return Err(FsError::AlreadyExists(dst.to_path_buf()));
    }
    let dst_exists = std::fs::symlink_metadata(dst).is_ok();
    if dst_exists && !same_file(src, dst) {
        return Err(FsError::AlreadyExists(dst.to_path_buf()));
    }
    std::fs::rename(src, dst).map_err(|e| io_error(src, &e))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn home() -> PathBuf {
        PathBuf::from("/Users/testuser")
    }

    #[test]
    fn expand_tilde_bare() {
        assert_eq!(expand_tilde("~", Some(&home())).unwrap(), home());
    }

    #[test]
    fn expand_tilde_with_path() {
        assert_eq!(
            expand_tilde("~/docs/a.txt", Some(&home())).unwrap(),
            home().join("docs/a.txt")
        );
    }

    #[test]
    fn expand_tilde_user_form_rejected() {
        assert!(matches!(
            expand_tilde("~root/x", Some(&home())),
            Err(FsError::InvalidName { .. })
        ));
    }

    #[test]
    fn expand_tilde_without_home_fails() {
        assert_eq!(expand_tilde("~", None), Err(FsError::HomeNotFound));
    }

    #[test]
    fn expand_tilde_passthrough() {
        assert_eq!(
            expand_tilde("plain/path", Some(&home())).unwrap(),
            PathBuf::from("plain/path")
        );
    }

    #[test]
    fn lexical_normalize_dots() {
        assert_eq!(
            lexical_normalize(Path::new("/a/b/../c/./d")),
            PathBuf::from("/a/c/d")
        );
    }

    #[test]
    fn lexical_normalize_parent_at_root_stays_at_root() {
        assert_eq!(
            lexical_normalize(Path::new("/../../a")),
            PathBuf::from("/a")
        );
    }

    #[test]
    fn lexical_normalize_relative_keeps_leading_parents() {
        assert_eq!(
            lexical_normalize(Path::new("../../a/../b")),
            PathBuf::from("../../b")
        );
    }

    #[test]
    fn lexical_normalize_empty_result_becomes_dot() {
        assert_eq!(lexical_normalize(Path::new("a/..")), PathBuf::from("."));
    }

    #[test]
    fn absolutize_relative_joins_base() {
        assert_eq!(
            absolutize(Path::new("/base"), "x/../y", Some(&home())).unwrap(),
            PathBuf::from("/base/y")
        );
    }

    #[test]
    fn absolutize_absolute_ignores_base() {
        assert_eq!(
            absolutize(Path::new("/base"), "/other/z", Some(&home())).unwrap(),
            PathBuf::from("/other/z")
        );
    }

    #[test]
    fn absolutize_empty_rejected() {
        assert!(matches!(
            absolutize(Path::new("/base"), "", Some(&home())),
            Err(FsError::InvalidName { .. })
        ));
    }

    #[test]
    fn canonical_dir_resolves_and_requires_directory() {
        let tmp = tempfile::tempdir().unwrap();
        let sub = tmp.path().join("sub dir");
        fs::create_dir(&sub).unwrap();
        let file = tmp.path().join("f.txt");
        fs::write(&file, "x").unwrap();

        let got = canonical_dir(tmp.path(), "sub dir", None).unwrap();
        assert_eq!(got, sub.canonicalize().unwrap());

        assert!(matches!(
            canonical_dir(tmp.path(), "f.txt", None),
            Err(FsError::NotADirectory(_))
        ));
        assert!(matches!(
            canonical_dir(tmp.path(), "missing", None),
            Err(FsError::NotFound(_))
        ));
    }

    #[cfg(unix)]
    #[test]
    fn canonical_dir_follows_symlinks() {
        let tmp = tempfile::tempdir().unwrap();
        let real = tmp.path().join("real");
        fs::create_dir(&real).unwrap();
        let link = tmp.path().join("link");
        std::os::unix::fs::symlink(&real, &link).unwrap();

        let got = canonical_dir(tmp.path(), "link", None).unwrap();
        assert_eq!(got, real.canonicalize().unwrap());
    }

    #[test]
    fn move_target_into_existing_directory_keeps_name() {
        let tmp = tempfile::tempdir().unwrap();
        let dest = tmp.path().join("archive");
        fs::create_dir(&dest).unwrap();

        let got = canonical_move_target(tmp.path(), "archive", "note.txt", None).unwrap();
        assert_eq!(got, dest.canonicalize().unwrap().join("note.txt"));
    }

    #[test]
    fn move_target_new_name_in_existing_parent() {
        let tmp = tempfile::tempdir().unwrap();
        let got = canonical_move_target(tmp.path(), "renamed.txt", "note.txt", None).unwrap();
        assert_eq!(got, tmp.path().canonicalize().unwrap().join("renamed.txt"));
    }

    #[test]
    fn move_target_missing_parent_fails() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(matches!(
            canonical_move_target(tmp.path(), "no/such/dir/x.txt", "x.txt", None),
            Err(FsError::NotFound(_))
        ));
    }

    #[test]
    fn validate_new_name_rules() {
        assert!(validate_new_name("ok.txt").is_ok());
        assert!(validate_new_name("with space and ünïcøde 檔").is_ok());
        assert!(validate_new_name(".hidden").is_ok());
        assert!(matches!(
            validate_new_name(""),
            Err(FsError::InvalidName { .. })
        ));
        assert!(matches!(
            validate_new_name("."),
            Err(FsError::InvalidName { .. })
        ));
        assert!(matches!(
            validate_new_name(".."),
            Err(FsError::InvalidName { .. })
        ));
        assert!(matches!(
            validate_new_name("a/b"),
            Err(FsError::InvalidName { .. })
        ));
        assert!(matches!(
            validate_new_name("a\0b"),
            Err(FsError::InvalidName { .. })
        ));
    }

    #[test]
    fn safe_move_moves_file() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("src with space.txt");
        let dst = tmp.path().join("dst-ünïcøde-檔案.txt");
        fs::write(&src, "hello").unwrap();

        safe_move(&src, &dst).unwrap();
        assert!(!src.exists());
        assert_eq!(fs::read_to_string(&dst).unwrap(), "hello");
    }

    #[test]
    fn safe_move_refuses_overwrite() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("a");
        let dst = tmp.path().join("b");
        fs::write(&src, "a").unwrap();
        fs::write(&dst, "b").unwrap();

        assert!(matches!(
            safe_move(&src, &dst),
            Err(FsError::AlreadyExists(_))
        ));
        assert_eq!(fs::read_to_string(&dst).unwrap(), "b");
        assert!(src.exists());
    }

    #[test]
    fn safe_move_missing_source_fails() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(matches!(
            safe_move(&tmp.path().join("ghost"), &tmp.path().join("x")),
            Err(FsError::NotFound(_))
        ));
    }

    #[test]
    fn safe_move_same_path_fails() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("a");
        fs::write(&src, "a").unwrap();
        assert!(matches!(
            safe_move(&src, &src),
            Err(FsError::AlreadyExists(_))
        ));
    }

    #[test]
    fn safe_move_moves_directory() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("dir one");
        fs::create_dir(&src).unwrap();
        fs::write(src.join("inner.txt"), "x").unwrap();
        let dst = tmp.path().join("dir two");

        safe_move(&src, &dst).unwrap();
        assert!(dst.join("inner.txt").exists());
        assert!(!src.exists());
    }

    #[cfg(unix)]
    #[test]
    fn safe_move_moves_broken_symlink() {
        let tmp = tempfile::tempdir().unwrap();
        let link = tmp.path().join("dangling");
        std::os::unix::fs::symlink(tmp.path().join("nowhere"), &link).unwrap();
        let dst = tmp.path().join("moved-link");

        safe_move(&link, &dst).unwrap();
        assert!(std::fs::symlink_metadata(&dst).unwrap().is_symlink());
        assert!(std::fs::symlink_metadata(&link).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn same_file_detects_identity() {
        let tmp = tempfile::tempdir().unwrap();
        let a = tmp.path().join("a");
        fs::write(&a, "x").unwrap();
        let b = tmp.path().join("b");
        fs::hard_link(&a, &b).unwrap();
        assert!(same_file(&a, &b));
        let c = tmp.path().join("c");
        fs::write(&c, "x").unwrap();
        assert!(!same_file(&a, &c));
    }
}
