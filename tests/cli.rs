//! Integration tests for the `filecraft` binary: argv, non-TTY listing,
//! and error reporting. `Command::output` gives the child no TTY, so
//! invoking the binary without `--list` also exercises the fallback.

use std::process::{Command, Stdio};

/// The binary, speaking English.
///
/// Pinned rather than inherited: filecraft resolves its language from
/// the system locale, so a `LANG` of `zh_TW.UTF-8` on the machine
/// running the suite would otherwise make every English assertion below
/// fail for a reason that has nothing to do with the code. [`bin_in`]
/// is how a language is chosen deliberately.
fn bin() -> Command {
    bin_in("en")
}

/// The binary, speaking `lang` - and only because of `FILECRAFT_LANG`:
/// every locale variable is cleared, so the test says which language it
/// is asserting about.
fn bin_in(lang: &str) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_filecraft"));
    cmd.env("FILECRAFT_LANG", lang)
        .env_remove("LC_ALL")
        .env_remove("LC_MESSAGES")
        .env_remove("LANG");
    cmd
}

/// The binary with no language named at all, so the locale decides.
fn bin_with_locale(variable: &str, value: &str) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_filecraft"));
    cmd.env_remove("FILECRAFT_LANG")
        .env_remove("LC_ALL")
        .env_remove("LC_MESSAGES")
        .env_remove("LANG")
        .env(variable, value);
    cmd
}

fn output_with_piped_stdio(cmd: &mut Command) -> std::process::Output {
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("failed to spawn filecraft")
}

#[test]
fn help_prints_usage_and_exits_zero() {
    let output = output_with_piped_stdio(bin().arg("--help"));
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("filecraft"));
    assert!(stdout.contains("--list"));
    assert!(stdout.contains("real TTY"));
    assert!(stdout.contains("update [--check]"));
}

#[test]
fn update_help_prints_update_usage() {
    let output = output_with_piped_stdio(bin().arg("update").arg("--help"));
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("filecraft update"));
    assert!(stdout.contains("--check"));
    assert!(stdout.contains("cargo install --git"));
}

#[test]
fn version_prints_crate_version() {
    let output = output_with_piped_stdio(bin().arg("--version"));
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains(env!("CARGO_PKG_VERSION")));
}

#[test]
fn unknown_option_exits_two() {
    let output = output_with_piped_stdio(bin().arg("--not-a-flag"));
    assert_eq!(output.status.code(), Some(2));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("unknown option"));
}

#[test]
fn update_unknown_option_exits_two() {
    let output = output_with_piped_stdio(bin().arg("update").arg("--not-a-flag"));
    assert_eq!(output.status.code(), Some(2));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("unknown option"));
}

#[test]
fn update_extra_argument_exits_two() {
    let output = output_with_piped_stdio(bin().arg("update").arg("extra"));
    assert_eq!(output.status.code(), Some(2));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("unexpected argument"));
}

#[test]
fn list_flag_prints_static_listing() {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::create_dir(tmp.path().join("sub dir")).unwrap();
    std::fs::write(tmp.path().join("notes 檔.md"), "hi").unwrap();

    let output = output_with_piped_stdio(bin().arg("--list").arg(tmp.path()));
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("no TTY"));
    assert!(stdout.contains("sub dir/"));
    assert!(stdout.contains("notes 檔.md"));
    assert!(stdout.contains("<DIR>"));
}

#[test]
fn no_tty_without_list_flag_still_prints_static_listing() {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(tmp.path().join("only.txt"), "x").unwrap();

    let output = output_with_piped_stdio(bin().arg(tmp.path()));
    assert!(output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("no TTY detected"));
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("only.txt"));
    assert!(stdout.contains("static listing"));
}

#[test]
fn missing_directory_is_a_failure() {
    let tmp = tempfile::tempdir().unwrap();
    let missing = tmp.path().join("does-not-exist");
    let output = output_with_piped_stdio(bin().arg("--list").arg(&missing));
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("not found") || stderr.contains("No such"));
}

/// `install.sh` is the fix for `zsh: command not found: filecraft`, so
/// its PATH detection is tested like any other code. The cases live in
/// `scripts/install_test.sh`, which sources the script in library mode;
/// running them from here is what puts them in `cargo test` and in CI.
#[test]
fn install_script_path_detection_passes_its_own_tests() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let script = root.join("scripts/install_test.sh");
    let output = Command::new("bash")
        .arg(&script)
        .current_dir(root)
        .stdin(Stdio::null())
        .output()
        .expect("failed to run scripts/install_test.sh");

    assert!(
        output.status.success(),
        "install.sh tests failed\n--- stdout ---\n{}\n--- stderr ---\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
}

/// The install path and the update path must not drift: both tell the
/// user to add the same line to the same file.
#[test]
fn install_script_and_update_advice_agree_on_the_path_line() {
    use filecraft::pathcheck::{advise, Shell};

    let home = std::path::PathBuf::from("/home/tester");
    let advice = advise(
        Some(&home.join(".cargo/bin")),
        None,
        Some("/usr/bin:/bin"),
        Some(&home),
        Some("/bin/zsh"),
    )
    .expect("the cargo bin directory is not on that PATH");
    assert_eq!(
        advice.export_line(),
        "export PATH=\"$HOME/.cargo/bin:$PATH\""
    );
    assert_eq!(advice.profile(), "~/.zshrc");
    assert_eq!(Shell::from_env(Some("/bin/zsh")), Shell::Zsh);

    let help = output_with_piped_stdio(
        Command::new("bash")
            .arg(std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("install.sh"))
            .arg("--help"),
    );
    assert!(help.status.success());
    let help = String::from_utf8_lossy(&help.stdout);
    assert!(
        help.contains(&advice.export_line()),
        "install.sh --help does not mention `{}`:\n{help}",
        advice.export_line()
    );
}

/// The two implementations of "is this directory on PATH" - `install.sh`
/// at install time and `pathcheck` inside `filecraft update` - must
/// answer the same question the same way, or one of them will tell
/// someone their PATH is fine while the other says it is broken.
#[test]
fn install_script_and_pathcheck_agree_on_what_is_on_path() {
    use filecraft::pathcheck::dir_on_path;

    const HOME: &str = "/home/tester";
    const WANT: &str = "/home/tester/.cargo/bin";
    let script = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("install.sh");

    for path_value in [
        "/usr/bin:/home/tester/.cargo/bin:/bin",
        "/opt/homebrew/bin:/usr/bin:/bin",
        "/usr/bin:~/.cargo/bin",
        "/usr/bin:$HOME/.cargo/bin",
        "/usr/bin:${HOME}/.cargo/bin",
        "/usr/bin:/home/tester/.cargo/bin/",
        "/usr/bin:/home/tester/./.cargo/bin",
        "/usr/bin:/home/tester/.cargo",
        "/usr/bin:/home/tester/.cargo/bin2",
        ":/home/tester/.cargo/bin:",
        "/opt/*:/usr/bin",
        "",
    ] {
        let rust = dir_on_path(
            std::path::Path::new(WANT),
            Some(path_value),
            Some(std::path::Path::new(HOME)),
        );

        let output = output_with_piped_stdio(Command::new("bash").arg("-c").arg(format!(
            "HOME={HOME}; export HOME; \
             FILECRAFT_INSTALL_LIB=1 . '{}'; \
             if path_contains_dir '{WANT}' '{path_value}'; then echo yes; else echo no; fi",
            script.display(),
        )));
        assert!(output.status.success(), "install.sh could not be sourced");
        let shell = String::from_utf8_lossy(&output.stdout).trim() == "yes";

        assert_eq!(
            rust, shell,
            "PATH [{path_value}]: pathcheck says {rust}, install.sh says {shell}"
        );
    }
}

#[test]
fn filecraft_lang_puts_the_whole_static_listing_into_traditional_chinese() {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::create_dir(tmp.path().join("sub dir")).unwrap();
    std::fs::write(tmp.path().join("notes 檔.md"), "hi").unwrap();

    let output = output_with_piped_stdio(bin_in("zh-TW").arg("--list").arg(tmp.path()));
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("靜態列表 (沒有 TTY)"), "{stdout}");
    assert!(stdout.contains("按鍵: j/k 移動"), "{stdout}");
    // The name and the kind marker are not words, so they do not change.
    assert!(stdout.contains("sub dir/"), "{stdout}");
    assert!(stdout.contains("notes 檔.md"), "{stdout}");
    assert!(stdout.contains("<DIR>"), "{stdout}");
}

#[test]
fn a_traditional_chinese_locale_is_enough_with_nothing_else_set() {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(tmp.path().join("only.txt"), "x").unwrap();

    for variable in ["LC_ALL", "LC_MESSAGES", "LANG"] {
        let output = output_with_piped_stdio(
            bin_with_locale(variable, "zh_TW.UTF-8")
                .arg("--list")
                .arg(tmp.path()),
        );
        assert!(output.status.success());
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(
            stdout.contains("靜態列表 (沒有 TTY)"),
            "{variable} did not select Traditional Chinese:\n{stdout}"
        );
    }
}

#[test]
fn a_simplified_chinese_locale_is_answered_in_english_not_in_the_wrong_chinese() {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(tmp.path().join("only.txt"), "x").unwrap();
    let output = output_with_piped_stdio(
        bin_with_locale("LANG", "zh_CN.UTF-8")
            .arg("--list")
            .arg(tmp.path()),
    );
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("static listing"), "{stdout}");
}

#[test]
fn filecraft_lang_overrides_the_system_locale() {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(tmp.path().join("only.txt"), "x").unwrap();
    let output = output_with_piped_stdio(
        bin_with_locale("LANG", "zh_TW.UTF-8")
            .env("FILECRAFT_LANG", "en")
            .arg("--list")
            .arg(tmp.path()),
    );
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("static listing"), "{stdout}");
}

#[test]
fn the_cli_help_is_written_in_the_screens_language_too() {
    let output = output_with_piped_stdio(bin_in("zh-TW").arg("--help"));
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("鍵盤優先"), "{stdout}");
    assert!(stdout.contains("FILECRAFT_LANG"), "{stdout}");
    // Flags are shell tokens, not words: they stay as they are typed.
    assert!(stdout.contains("-l, --list"), "{stdout}");

    let output = output_with_piped_stdio(bin_in("zh-TW").arg("update").arg("--help"));
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("安裝最新版的 filecraft"), "{stdout}");
    assert!(stdout.contains("cargo install --git"), "{stdout}");
}

#[test]
fn a_usage_error_is_reported_in_the_screens_language() {
    let output = output_with_piped_stdio(bin_in("zh-TW").arg("--not-a-flag"));
    assert_eq!(output.status.code(), Some(2));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("未知的選項 '--not-a-flag'"), "{stderr}");
    assert!(stderr.contains("filecraft --help"), "{stderr}");
}

/// A settings file under a fixture config root, as the binary reads it.
fn write_config(root: &std::path::Path, text: &str) {
    let dir = root.join("filecraft");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("config.toml"), text).unwrap();
}

/// The binary, reading a settings file the test wrote. `HOME` is cleared
/// so the person running the suite can never have their own
/// `~/.config/filecraft/config.toml` decide the answer.
fn bin_with_config(root: &std::path::Path) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_filecraft"));
    cmd.env_remove("FILECRAFT_LANG")
        .env_remove("LC_ALL")
        .env_remove("LC_MESSAGES")
        .env_remove("LANG")
        .env_remove("HOME")
        .env("XDG_CONFIG_HOME", root);
    cmd
}

#[test]
fn a_columns_table_in_the_settings_file_does_not_disturb_the_language() {
    // The sharp edge of a line-oriented settings file: a top-level key
    // written *after* a `[table]` header belongs to that table. The
    // binary has to read `language` past a `[columns]` block, so this
    // asserts the whole file end to end rather than the reader alone.
    let config = tempfile::tempdir().unwrap();
    write_config(
        config.path(),
        "language = \"zh-TW\"\n\n[columns]\nvisible = [\"name\", \"size\", \"kind\"]\nheader = false\n",
    );
    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(tmp.path().join("notes.md"), "hi").unwrap();

    let output =
        output_with_piped_stdio(bin_with_config(config.path()).arg("--list").arg(tmp.path()));
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    // The language still resolved from the file, past the table.
    assert!(stdout.contains("靜態列表 (沒有 TTY)"), "{stdout}");
    assert!(stdout.contains("notes.md"), "{stdout}");
}

#[test]
fn a_settings_file_naming_a_column_filecraft_does_not_have_still_starts() {
    // A file written by a later version must not stop this one.
    let config = tempfile::tempdir().unwrap();
    write_config(
        config.path(),
        "[columns]\nvisible = [\"name\", \"tags\"]\nheader = maybe\n",
    );
    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(tmp.path().join("notes.md"), "hi").unwrap();

    let output =
        output_with_piped_stdio(bin_with_config(config.path()).arg("--list").arg(tmp.path()));
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains("notes.md"));
}
