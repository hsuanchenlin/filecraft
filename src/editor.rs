//! Editor and preview invocation construction.
//!
//! Commands are built as argv vectors and executed directly (no shell).
//! `$EDITOR` is split on whitespace so values like `"code --wait"` work;
//! shell syntax beyond that (quotes, `$VAR`) is intentionally not
//! interpreted - Filecraft never evaluates shell.

use std::path::{Path, PathBuf};

/// Fallback editor when `$EDITOR` is unset or blank.
pub const DEFAULT_EDITOR: &str = "nvim";

/// Resolve the editor argv prefix from an `$EDITOR`-style value.
pub fn resolve_editor(editor_env: Option<&str>) -> Vec<String> {
    match editor_env {
        Some(value) if !value.trim().is_empty() => {
            value.split_whitespace().map(str::to_string).collect()
        }
        _ => vec![DEFAULT_EDITOR.to_string()],
    }
}

/// Full argv for `edit`: editor prefix, `--` when safe, then the file.
///
/// `--` stops option parsing so a file named `-R` cannot become a flag.
/// It is only added for the default nvim fallback and known `vi`-family
/// editors; arbitrary `$EDITOR` values may not understand `--`.
pub fn build_edit_command(editor_env: Option<&str>, file: &Path) -> Vec<String> {
    let mut argv = resolve_editor(editor_env);
    if editor_supports_dashdash(&argv[0]) {
        argv.push("--".to_string());
    }
    argv.push(file.to_string_lossy().into_owned());
    argv
}

fn editor_supports_dashdash(program: &str) -> bool {
    let base = Path::new(program)
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    matches!(base.as_str(), "nvim" | "vim" | "vi" | "nano" | "hx" | "kak")
}

/// Full argv for a read-only Neovim preview.
///
/// `-R` read-only, `-M` forbids modification, `-n` disables swap files so
/// previewing never writes anything anywhere.
pub fn build_preview_command(file: &Path) -> Vec<String> {
    vec![
        "nvim".to_string(),
        "-R".to_string(),
        "-M".to_string(),
        "-n".to_string(),
        "--".to_string(),
        file.to_string_lossy().into_owned(),
    ]
}

/// Search a PATH-style string for an executable named `program`.
/// Pure with respect to its inputs so it can be tested with fixture dirs.
pub fn find_in_path(program: &str, path_env: Option<&str>) -> Option<PathBuf> {
    let path_env = path_env?;
    for dir in path_env.split(':').filter(|d| !d.is_empty()) {
        let candidate = Path::new(dir).join(program);
        if is_executable_file(&candidate) {
            return Some(candidate);
        }
    }
    None
}

#[cfg(unix)]
fn is_executable_file(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path)
        .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

#[cfg(not(unix))]
fn is_executable_file(path: &Path) -> bool {
    std::fs::metadata(path)
        .map(|m| m.is_file())
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_editor_defaults_to_nvim() {
        assert_eq!(resolve_editor(None), vec!["nvim"]);
        assert_eq!(resolve_editor(Some("")), vec!["nvim"]);
        assert_eq!(resolve_editor(Some("   ")), vec!["nvim"]);
    }

    #[test]
    fn resolve_editor_splits_arguments() {
        assert_eq!(resolve_editor(Some("code --wait")), vec!["code", "--wait"]);
        assert_eq!(resolve_editor(Some("vim")), vec!["vim"]);
    }

    #[test]
    fn edit_command_with_default_editor_uses_dashdash() {
        let argv = build_edit_command(None, Path::new("/tmp/-R"));
        assert_eq!(argv, vec!["nvim", "--", "/tmp/-R"]);
    }

    #[test]
    fn edit_command_with_custom_editor_appends_file() {
        let argv = build_edit_command(Some("code --wait"), Path::new("/tmp/a b.txt"));
        assert_eq!(argv, vec!["code", "--wait", "/tmp/a b.txt"]);
    }

    #[test]
    fn edit_command_vi_family_gets_dashdash_even_with_full_path() {
        let argv = build_edit_command(Some("/usr/bin/vim -u NONE"), Path::new("/tmp/x"));
        assert_eq!(argv, vec!["/usr/bin/vim", "-u", "NONE", "--", "/tmp/x"]);
    }

    #[test]
    fn preview_command_is_read_only_no_swap() {
        let argv = build_preview_command(Path::new("/tmp/notes.md"));
        assert_eq!(argv, vec!["nvim", "-R", "-M", "-n", "--", "/tmp/notes.md"]);
    }

    #[test]
    fn preview_command_handles_spaces_and_unicode() {
        let argv = build_preview_command(Path::new("/tmp/my nötes 檔.md"));
        assert_eq!(argv.last().unwrap(), "/tmp/my nötes 檔.md");
    }

    #[cfg(unix)]
    #[test]
    fn find_in_path_finds_executables_only() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let bin = tmp.path().join("bin");
        std::fs::create_dir(&bin).unwrap();
        let exe = bin.join("mytool");
        std::fs::write(&exe, "#!/bin/sh\n").unwrap();
        let mut perms = std::fs::metadata(&exe).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&exe, perms).unwrap();
        std::fs::write(bin.join("notexec"), "").unwrap();

        let path_env = format!("/nonexistent:{}", bin.display());
        assert_eq!(find_in_path("mytool", Some(&path_env)), Some(exe));
        assert_eq!(find_in_path("notexec", Some(&path_env)), None);
        assert_eq!(find_in_path("missing", Some(&path_env)), None);
        assert_eq!(find_in_path("mytool", None), None);
    }
}
