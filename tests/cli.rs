//! Integration tests for the `filecraft` binary: argv, non-TTY listing,
//! and error reporting. `Command::output` gives the child no TTY, so
//! invoking the binary without `--list` also exercises the fallback.

use std::process::{Command, Stdio};

fn bin() -> Command {
    Command::new(env!("CARGO_BIN_EXE_filecraft"))
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
