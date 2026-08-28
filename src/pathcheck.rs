//! Whether a shell can actually find `filecraft`, and what to say when it
//! cannot.
//!
//! A `cargo install` puts the binary in the install root's `bin` directory
//! (normally `~/.cargo/bin`), which a macOS zsh does not have on `PATH`
//! unless something put it there. The install then looks like it worked and
//! the next `filecraft` is `zsh: command not found`. This module is the pure
//! half of the answer: given a `PATH` value and where the binary lives, it
//! decides whether the shell can reach it and builds the exact line to add
//! to the right startup file. `install.sh` does the same job at install
//! time; `filecraft update` reports it through [`crate::update`].
//!
//! Everything here is string and path arithmetic - no filesystem, no
//! environment - so the whole decision is testable.

use std::fmt;
use std::path::{Component, Path, PathBuf};

/// The startup file a POSIX shell reads when no better guess exists.
const FALLBACK_PROFILE: &str = "~/.profile";

/// The shell whose startup file should carry the `PATH` line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Shell {
    Zsh,
    Bash,
    Fish,
    /// Anything else, including an unset `SHELL`.
    Other,
}

impl Shell {
    /// Classify the value of `$SHELL` (a path such as `/bin/zsh`).
    pub fn from_env(shell: Option<&str>) -> Self {
        let name = shell
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .and_then(|s| s.rsplit('/').next())
            .unwrap_or("");
        // A login shell is conventionally written `-zsh`.
        match name.trim_start_matches('-') {
            "zsh" => Shell::Zsh,
            "bash" | "sh" => Shell::Bash,
            "fish" => Shell::Fish,
            _ => Shell::Other,
        }
    }

    /// The startup file this shell reads for interactive sessions.
    pub fn profile(self) -> &'static str {
        match self {
            Shell::Zsh => "~/.zshrc",
            Shell::Bash => "~/.bashrc",
            Shell::Fish => "~/.config/fish/config.fish",
            Shell::Other => FALLBACK_PROFILE,
        }
    }

    /// The line that puts `dir` on `PATH` in this shell's syntax.
    ///
    /// `home` lets a directory under the user's home print as `$HOME/...`,
    /// so the line stays correct when it is copied to another machine.
    pub fn export_line(self, dir: &Path, home: Option<&Path>) -> String {
        let dir = portable_dir(dir, home);
        match self {
            Shell::Fish => format!("fish_add_path {dir}"),
            _ => format!("export PATH=\"{dir}:$PATH\""),
        }
    }
}

/// A directory the shell cannot reach, and the shell that needs to learn it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PathAdvice {
    /// The directory that has to go on `PATH`.
    pub dir: PathBuf,
    /// The shell whose startup file should carry the line.
    pub shell: Shell,
    /// `$HOME`, when known, so the line can be written portably.
    pub home: Option<PathBuf>,
    /// The running binary is a build inside a `target/` tree, so [`dir`]
    /// is where an install would land rather than where this binary is.
    ///
    /// [`dir`]: PathAdvice::dir
    pub from_build_tree: bool,
}

impl PathAdvice {
    /// The line to append to [`Self::profile`].
    pub fn export_line(&self) -> String {
        self.shell.export_line(&self.dir, self.home.as_deref())
    }

    /// The startup file that line belongs in.
    pub fn profile(&self) -> &'static str {
        self.shell.profile()
    }
}

impl fmt::Display for PathAdvice {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(f, "warning: {} is not on your PATH", self.dir.display())?;
        if self.from_build_tree {
            writeln!(
                f,
                "  this is a build from the source tree; an installed \
                 filecraft goes there"
            )?;
        }
        writeln!(
            f,
            "  until it is, `filecraft` only runs by full path, not by name"
        )?;
        writeln!(
            f,
            "  add it:  echo '{}' >> {}",
            self.export_line(),
            self.profile()
        )?;
        writeln!(
            f,
            "  then open a new terminal, or run ./install.sh from a \
             filecraft clone to do this for you"
        )
    }
}

/// Decide whether anything needs saying about `PATH`.
///
/// `exe_dir` is the directory of the running binary and `cargo_bin` is the
/// Cargo install root's `bin` directory. A binary running out of a `target/`
/// build tree is judged by `cargo_bin`, because that is where installing it
/// would put it - telling someone to add `target/debug` to `PATH` would be
/// wrong.
/// Returns `None` when the shell can already find the binary.
pub fn advise(
    exe_dir: Option<&Path>,
    cargo_bin: Option<&Path>,
    path_env: Option<&str>,
    home: Option<&Path>,
    shell: Option<&str>,
) -> Option<PathAdvice> {
    let from_build_tree = exe_dir.is_some_and(looks_like_build_dir);
    let dir = if from_build_tree {
        cargo_bin.or(exe_dir)?
    } else {
        exe_dir.or(cargo_bin)?
    };
    if dir_on_path(dir, path_env, home) {
        return None;
    }
    Some(PathAdvice {
        dir: dir.to_path_buf(),
        shell: Shell::from_env(shell),
        home: home.map(Path::to_path_buf),
        from_build_tree,
    })
}

/// Is `dir` one of the directories this `PATH` tells a shell to search?
pub fn dir_on_path(dir: &Path, path_env: Option<&str>, home: Option<&Path>) -> bool {
    let want = normalize(dir, home);
    path_dirs(path_env, home).contains(&want)
}

/// The directories a shell would search, in order, with `~` expanded and
/// trailing slashes removed.
///
/// An empty entry means the current directory; it is dropped, because
/// nothing it could match is knowable from a string.
pub fn path_dirs(path_env: Option<&str>, home: Option<&Path>) -> Vec<PathBuf> {
    let Some(path_env) = path_env else {
        return Vec::new();
    };
    path_env
        .split(':')
        .filter(|entry| !entry.is_empty())
        .map(|entry| normalize(Path::new(entry), home))
        .collect()
}

/// Reduce a directory to the form two spellings of it can be compared in:
/// `~` and `$HOME` expanded, `.` components and trailing slashes dropped.
///
/// This is lexical on purpose. It resolves no symlinks and touches no
/// disk, so it stays a pure comparison; the cost is that two paths that
/// differ only through a symlink read as different directories.
pub fn normalize(dir: &Path, home: Option<&Path>) -> PathBuf {
    let expanded = expand_home(dir, home);
    let mut out = PathBuf::new();
    for component in expanded.components() {
        match component {
            Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    if out.as_os_str().is_empty() {
        out.push(".");
    }
    out
}

/// Rewrite a leading `~` or `$HOME` as the real home directory.
fn expand_home(dir: &Path, home: Option<&Path>) -> PathBuf {
    let Some(home) = home else {
        return dir.to_path_buf();
    };
    let text = dir.to_string_lossy();
    for prefix in ["~", "$HOME", "${HOME}"] {
        let Some(rest) = text.strip_prefix(prefix) else {
            continue;
        };
        if rest.is_empty() {
            return home.to_path_buf();
        }
        if let Some(rest) = rest.strip_prefix('/') {
            return home.join(rest);
        }
    }
    dir.to_path_buf()
}

/// Write a directory the way it should appear in a startup file: under the
/// home directory it becomes `$HOME/...`, so the line survives being
/// copied to another machine or another user.
fn portable_dir(dir: &Path, home: Option<&Path>) -> String {
    let normalized = normalize(dir, home);
    if let Some(home) = home {
        if let Ok(rest) = normalized.strip_prefix(normalize(home, Some(home))) {
            if !rest.as_os_str().is_empty() {
                return format!("$HOME/{}", rest.display());
            }
            return "$HOME".to_string();
        }
    }
    normalized.display().to_string()
}

/// Does this directory look like Cargo's build output rather than an
/// installed binary's home?
///
/// Cargo builds land in `target/debug`, `target/release`, or
/// `target/<triple>/<profile>`, so `target` is within the last few
/// components. Looking no further up keeps a project that merely lives
/// under a directory named `target` from being mistaken for one.
fn looks_like_build_dir(dir: &Path) -> bool {
    dir.ancestors()
        .take(4)
        .any(|a| a.file_name().is_some_and(|name| name == "target"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn home() -> PathBuf {
        PathBuf::from("/Users/tester")
    }

    #[test]
    fn a_directory_spelled_the_same_way_is_found() {
        assert!(dir_on_path(
            Path::new("/Users/tester/.cargo/bin"),
            Some("/usr/bin:/Users/tester/.cargo/bin:/bin"),
            Some(&home()),
        ));
    }

    /// The reported bug: the binary is installed, the directory is not on
    /// `PATH`, so the shell reports the command as missing.
    #[test]
    fn the_cargo_bin_directory_missing_from_path_is_detected() {
        assert!(!dir_on_path(
            Path::new("/Users/tester/.cargo/bin"),
            Some("/opt/homebrew/bin:/usr/bin:/bin:/Users/tester/.local/bin"),
            Some(&home()),
        ));
    }

    #[test]
    fn path_spellings_that_mean_the_same_directory_all_match() {
        for spelling in [
            "~/.cargo/bin",
            "$HOME/.cargo/bin",
            "${HOME}/.cargo/bin",
            "/Users/tester/.cargo/bin/",
            "/Users/tester/.cargo/bin//",
            "/Users/tester/./.cargo/bin",
        ] {
            assert!(
                dir_on_path(
                    Path::new("/Users/tester/.cargo/bin"),
                    Some(&format!("/usr/bin:{spelling}")),
                    Some(&home()),
                ),
                "{spelling} should be the cargo bin directory"
            );
        }
    }

    #[test]
    fn a_prefix_of_a_path_entry_is_not_on_path() {
        // `~/.cargo` is not `~/.cargo/bin`, and neither is `~/.cargo/bin2`.
        assert!(!dir_on_path(
            Path::new("/Users/tester/.cargo/bin"),
            Some("/Users/tester/.cargo:/Users/tester/.cargo/bin2"),
            Some(&home()),
        ));
    }

    #[test]
    fn an_unset_or_empty_path_reaches_nothing() {
        assert!(!dir_on_path(Path::new("/x/bin"), None, Some(&home())));
        assert!(!dir_on_path(Path::new("/x/bin"), Some(""), Some(&home())));
        assert!(path_dirs(None, None).is_empty());
    }

    /// An empty `PATH` entry means the current directory. It is dropped
    /// rather than guessed at, and it must not swallow the entries by it.
    #[test]
    fn empty_path_entries_are_dropped_without_losing_their_neighbours() {
        assert_eq!(
            path_dirs(Some(":/usr/bin::/bin:"), None),
            vec![PathBuf::from("/usr/bin"), PathBuf::from("/bin")]
        );
    }

    #[test]
    fn tilde_without_a_home_is_left_alone_rather_than_guessed() {
        assert_eq!(
            path_dirs(Some("~/.cargo/bin"), None),
            vec![PathBuf::from("~/.cargo/bin")]
        );
        assert!(!dir_on_path(
            Path::new("/Users/tester/.cargo/bin"),
            Some("~/.cargo/bin"),
            None
        ));
    }

    #[test]
    fn no_advice_when_the_shell_can_already_find_the_binary() {
        assert_eq!(
            advise(
                Some(Path::new("/Users/tester/.cargo/bin")),
                Some(Path::new("/Users/tester/.cargo/bin")),
                Some("/usr/bin:/Users/tester/.cargo/bin"),
                Some(&home()),
                Some("/bin/zsh"),
            ),
            None
        );
    }

    #[test]
    fn advice_names_the_missing_directory_and_the_zsh_startup_file() {
        let advice = advise(
            Some(Path::new("/Users/tester/.cargo/bin")),
            Some(Path::new("/Users/tester/.cargo/bin")),
            Some("/usr/bin:/bin"),
            Some(&home()),
            Some("/bin/zsh"),
        )
        .expect("cargo bin is not on this PATH");

        assert_eq!(advice.dir, PathBuf::from("/Users/tester/.cargo/bin"));
        assert!(!advice.from_build_tree);
        assert_eq!(advice.profile(), "~/.zshrc");
        assert_eq!(
            advice.export_line(),
            "export PATH=\"$HOME/.cargo/bin:$PATH\""
        );

        let text = advice.to_string();
        assert!(text.contains("/Users/tester/.cargo/bin is not on your PATH"));
        assert!(text.contains("~/.zshrc"));
        assert!(text.contains("./install.sh"));
    }

    /// Someone running `cargo run` is not helped by being told to put
    /// `target/debug` on `PATH`; the directory that matters is the one an
    /// install writes to.
    #[test]
    fn a_binary_in_a_build_tree_is_judged_by_the_cargo_bin_directory() {
        let advice = advise(
            Some(Path::new("/src/filecraft/target/debug")),
            Some(Path::new("/Users/tester/.cargo/bin")),
            Some("/usr/bin"),
            Some(&home()),
            Some("/bin/zsh"),
        )
        .expect("cargo bin is not on this PATH");
        assert_eq!(advice.dir, PathBuf::from("/Users/tester/.cargo/bin"));
        assert!(advice.from_build_tree);
        assert!(advice.to_string().contains("build from the source tree"));

        // ...and stays quiet when that directory is reachable.
        assert_eq!(
            advise(
                Some(Path::new(
                    "/src/filecraft/target/aarch64-apple-darwin/release"
                )),
                Some(Path::new("/Users/tester/.cargo/bin")),
                Some("/usr/bin:~/.cargo/bin"),
                Some(&home()),
                Some("/bin/zsh"),
            ),
            None
        );
    }

    /// A directory that merely lives under one named `target` is not a
    /// build directory, so the advice is about that directory itself.
    #[test]
    fn a_deep_directory_under_a_target_folder_is_not_a_build_directory() {
        let advice = advise(
            Some(Path::new("/opt/target/a/b/c/bin")),
            Some(Path::new("/Users/tester/.cargo/bin")),
            Some("/usr/bin"),
            Some(&home()),
            Some("/bin/zsh"),
        )
        .expect("that directory is not on this PATH");
        assert_eq!(advice.dir, PathBuf::from("/opt/target/a/b/c/bin"));
        assert!(!advice.from_build_tree);
    }

    #[test]
    fn a_binary_outside_the_home_directory_gets_a_literal_line() {
        let advice = advise(
            Some(Path::new("/opt/filecraft/bin")),
            None,
            Some("/usr/bin"),
            Some(&home()),
            Some("/bin/bash"),
        )
        .expect("that directory is not on this PATH");
        assert_eq!(advice.profile(), "~/.bashrc");
        assert_eq!(
            advice.export_line(),
            "export PATH=\"/opt/filecraft/bin:$PATH\""
        );
    }

    #[test]
    fn fish_gets_fish_syntax_and_its_own_config_file() {
        let advice = advise(
            Some(Path::new("/Users/tester/.cargo/bin")),
            None,
            Some("/usr/bin"),
            Some(&home()),
            Some("/opt/homebrew/bin/fish"),
        )
        .expect("cargo bin is not on this PATH");
        assert_eq!(advice.profile(), "~/.config/fish/config.fish");
        assert_eq!(advice.export_line(), "fish_add_path $HOME/.cargo/bin");
    }

    #[test]
    fn an_unknown_or_unset_shell_falls_back_to_the_posix_profile() {
        for shell in [None, Some(""), Some("/usr/bin/nu")] {
            assert_eq!(Shell::from_env(shell).profile(), "~/.profile");
        }
        assert_eq!(Shell::from_env(Some("-zsh")), Shell::Zsh);
        assert_eq!(Shell::from_env(Some("/bin/sh")), Shell::Bash);
    }

    #[test]
    fn advice_needs_at_least_one_directory_to_talk_about() {
        assert_eq!(
            advise(None, None, Some("/usr/bin"), Some(&home()), None),
            None
        );
    }
}
