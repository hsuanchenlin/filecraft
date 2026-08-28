//! Self-update for the `filecraft` binary.
//!
//! `filecraft update` detects whether this binary came from a git clone
//! (`cargo run` / `cargo install --path`) or a global `cargo install --git`,
//! then either pulls and reinstalls the clone or re-runs the git install.
//! `filecraft update --check` reports current vs target version without
//! installing. Nothing here panics on I/O: missing `cargo`, network
//! failures, and permission errors become [`UpdateError`]s.
//!
//! Every report also carries a `PATH` self-check: an install that lands
//! in `~/.cargo/bin` is useless if the shell never looks there, which is
//! exactly how `zsh: command not found: filecraft` happens after a
//! successful install. [`crate::pathcheck`] decides that; this module only
//! supplies it the environment.
//!
//! [`Host`] is the seam tests inject so this module never needs a TTY
//! or the network.

use std::io;
use std::path::{Path, PathBuf};
use std::process::Stdio;

use crate::editor;
use crate::pathcheck::{self, PathAdvice};

/// Canonical source used for a global cargo git install.
pub const GIT_URL: &str = "https://github.com/hsuanchenlin/filecraft.git";

const MANIFEST_URL: &str =
    "https://raw.githubusercontent.com/hsuanchenlin/filecraft/main/Cargo.toml";
const MAIN_REF: &str = "refs/heads/main";

/// How this binary was installed, and therefore how to update it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InstallKind {
    /// A filecraft git working tree (dev binary or `cargo install --path`).
    GitClone { root: PathBuf },
    /// `cargo install --git <url>` (commit recorded when known).
    CargoGit { url: String, commit: Option<String> },
}

impl InstallKind {
    fn source_label(&self) -> String {
        match self {
            InstallKind::GitClone { root } => {
                format!("git clone at {}", root.display())
            }
            InstallKind::CargoGit { url, .. } => {
                format!("cargo install --git {url}")
            }
        }
    }
}

/// Outcome of a successful check or install.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpdateReport {
    pub current_version: String,
    pub target_version: String,
    pub source: String,
    pub status: UpdateStatus,
    /// Set when the shell cannot reach the binary this update installs.
    pub path_advice: Option<PathAdvice>,
}

/// Whether an update was needed, available, or applied.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UpdateStatus {
    UpToDate,
    Available,
    Updated,
}

impl std::fmt::Display for UpdateReport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        writeln!(f, "current version: {}", self.current_version)?;
        writeln!(f, "target version:  {}", self.target_version)?;
        writeln!(f, "install source:  {}", self.source)?;
        match self.status {
            UpdateStatus::UpToDate => writeln!(f, "ok: already up to date"),
            UpdateStatus::Available => writeln!(
                f,
                "update available: {} -> {}",
                self.current_version, self.target_version
            ),
            UpdateStatus::Updated => {
                writeln!(f, "ok: updated to {}", self.target_version)
            }
        }?;
        match &self.path_advice {
            Some(advice) => write!(f, "\n{advice}"),
            None => Ok(()),
        }
    }
}

/// Why an update or check could not be completed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UpdateError {
    MissingCargo,
    MissingGit,
    MissingCurl,
    Network(String),
    Permission(String),
    Failed { step: String, message: String },
}

impl std::fmt::Display for UpdateError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            UpdateError::MissingCargo => write!(
                f,
                "cargo is not installed; install a Rust toolchain \
                 (https://rustup.rs) and make sure `cargo` is on PATH"
            ),
            UpdateError::MissingGit => {
                write!(f, "git is not installed; needed to update a local clone")
            }
            UpdateError::MissingCurl => {
                write!(f, "cannot check for updates: `curl` is not on PATH")
            }
            UpdateError::Network(msg) => write!(f, "network error: {msg}"),
            UpdateError::Permission(msg) => write!(f, "permission denied: {msg}"),
            UpdateError::Failed { step, message } => {
                write!(f, "update failed ({step}): {message}")
            }
        }
    }
}

impl std::error::Error for UpdateError {}

/// Filesystem and process access used by [`run_with`].
pub trait Host {
    fn current_exe(&self) -> Option<PathBuf>;
    /// Where `cargo install` writes: `$CARGO_INSTALL_ROOT`, else
    /// `$CARGO_HOME`, else `~/.cargo`. It holds `bin/` and `.crates.toml`.
    fn install_root(&self) -> Option<PathBuf>;
    fn path_env(&self) -> Option<String>;
    fn home(&self) -> Option<PathBuf>;
    fn shell(&self) -> Option<String>;
    fn current_version(&self) -> &str;
    fn is_dir(&self, path: &Path) -> bool;
    fn is_file(&self, path: &Path) -> bool;
    fn read_to_string(&self, path: &Path) -> Result<String, String>;
    fn has_program(&self, name: &str) -> bool;
    fn run(&self, spec: &CommandSpec) -> CommandResult;
}

/// One external command the updater may run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandSpec {
    pub program: String,
    pub args: Vec<String>,
    pub env: Vec<(String, String)>,
    pub inherit_stdio: bool,
}

/// Result of running a [`CommandSpec`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandResult {
    pub status: Option<i32>,
    pub stdout: String,
    pub stderr: String,
    pub spawn_error: Option<SpawnError>,
}

/// Why the process could not even be started.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SpawnError {
    NotFound,
    PermissionDenied(String),
    Other(String),
}

impl CommandResult {
    #[cfg(test)]
    fn success(stdout: impl Into<String>) -> Self {
        Self {
            status: Some(0),
            stdout: stdout.into(),
            stderr: String::new(),
            spawn_error: None,
        }
    }

    fn ok(&self) -> bool {
        self.spawn_error.is_none() && self.status == Some(0)
    }
}

/// Live host: real filesystem, `PATH`, and subprocesses.
pub struct RealHost;

impl Host for RealHost {
    fn current_exe(&self) -> Option<PathBuf> {
        let exe = std::env::current_exe().ok()?;
        Some(exe.canonicalize().unwrap_or(exe))
    }

    fn install_root(&self) -> Option<PathBuf> {
        for key in ["CARGO_INSTALL_ROOT", "CARGO_HOME"] {
            if let Some(root) = std::env::var_os(key) {
                return Some(PathBuf::from(root));
            }
        }
        std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cargo"))
    }

    fn path_env(&self) -> Option<String> {
        std::env::var("PATH").ok()
    }

    fn home(&self) -> Option<PathBuf> {
        std::env::var_os("HOME").map(PathBuf::from)
    }

    fn shell(&self) -> Option<String> {
        std::env::var("SHELL").ok()
    }

    fn current_version(&self) -> &str {
        env!("CARGO_PKG_VERSION")
    }

    fn is_dir(&self, path: &Path) -> bool {
        std::fs::metadata(path).map(|m| m.is_dir()).unwrap_or(false)
    }

    fn is_file(&self, path: &Path) -> bool {
        std::fs::metadata(path)
            .map(|m| m.is_file())
            .unwrap_or(false)
    }

    fn read_to_string(&self, path: &Path) -> Result<String, String> {
        std::fs::read_to_string(path).map_err(|e| e.to_string())
    }

    fn has_program(&self, name: &str) -> bool {
        editor::find_in_path(name, self.path_env().as_deref()).is_some()
    }

    fn run(&self, spec: &CommandSpec) -> CommandResult {
        let mut cmd = std::process::Command::new(&spec.program);
        cmd.args(&spec.args);
        for (key, value) in &spec.env {
            cmd.env(key, value);
        }
        cmd.stdin(Stdio::null());
        if spec.inherit_stdio {
            return match cmd.status() {
                Ok(status) => CommandResult {
                    status: status.code(),
                    stdout: String::new(),
                    stderr: String::new(),
                    spawn_error: None,
                },
                Err(error) => spawn_failed(error),
            };
        }
        match cmd.output() {
            Ok(output) => CommandResult {
                status: output.status.code(),
                stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
                stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
                spawn_error: None,
            },
            Err(error) => spawn_failed(error),
        }
    }
}

fn spawn_failed(error: io::Error) -> CommandResult {
    let spawn_error = match error.kind() {
        io::ErrorKind::NotFound => SpawnError::NotFound,
        io::ErrorKind::PermissionDenied => SpawnError::PermissionDenied(error.to_string()),
        _ => SpawnError::Other(error.to_string()),
    };
    CommandResult {
        status: None,
        stdout: String::new(),
        stderr: String::new(),
        spawn_error: Some(spawn_error),
    }
}

/// Run a check or install against the real machine.
pub fn run(check: bool) -> Result<UpdateReport, UpdateError> {
    run_with(check, &RealHost)
}

/// Run a check or install against an injected [`Host`].
pub fn run_with<H: Host>(check: bool, host: &H) -> Result<UpdateReport, UpdateError> {
    let kind = detect_install(host);
    if !check {
        require_apply_tools(host, &kind)?;
    }

    let current = host.current_version().to_string();
    let remote = probe_remote(host, check)?;
    let local_commit = local_commit(host, &kind);
    let available = update_available(&current, &remote, local_commit.as_deref());
    let target_version = display_target(&remote);

    let advice = path_advice(host);
    let report = |status: UpdateStatus| UpdateReport {
        current_version: display_current(&current, local_commit.as_deref()),
        target_version: target_version.clone(),
        source: kind.source_label(),
        status,
        path_advice: advice.clone(),
    };

    if check {
        return Ok(report(if available {
            UpdateStatus::Available
        } else {
            UpdateStatus::UpToDate
        }));
    }

    if !available && remote.version.is_some() {
        return Ok(report(UpdateStatus::UpToDate));
    }

    apply_update(host, &kind)?;
    let installed = installed_clone_info(host, &kind)?;
    Ok(UpdateReport {
        current_version: display_current(&current, local_commit.as_deref()),
        target_version: installed
            .as_ref()
            .map(display_target)
            .unwrap_or(target_version),
        source: kind.source_label(),
        status: UpdateStatus::Updated,
        path_advice: advice,
    })
}

/// The `PATH` self-check every report carries: can a shell find the
/// binary this update just installed, or the one it is running as?
///
/// A binary in a `target/` build tree is judged by `$CARGO_HOME/bin`,
/// because that is where the install writes it.
fn path_advice<H: Host>(host: &H) -> Option<PathAdvice> {
    let exe = host.current_exe();
    let exe_dir = exe.as_deref().and_then(Path::parent);
    let cargo_bin = host.install_root().map(|root| root.join("bin"));
    pathcheck::advise(
        exe_dir,
        cargo_bin.as_deref(),
        host.path_env().as_deref(),
        host.home().as_deref(),
        host.shell().as_deref(),
    )
}

struct RemoteInfo {
    version: Option<String>,
    commit: Option<String>,
}

fn probe_remote<H: Host>(host: &H, check: bool) -> Result<RemoteInfo, UpdateError> {
    let version = match fetch_remote_version(host) {
        Ok(version) => Some(version),
        Err(error) if check => return Err(error),
        Err(_) => None,
    };
    let commit = fetch_remote_commit(host);
    if version.is_none() && commit.is_none() && check {
        return Err(UpdateError::Network(
            "could not determine the latest version".to_string(),
        ));
    }
    Ok(RemoteInfo { version, commit })
}

fn fetch_remote_version<H: Host>(host: &H) -> Result<String, UpdateError> {
    if !host.has_program("curl") {
        return Err(UpdateError::MissingCurl);
    }
    let result = host.run(&curl_manifest_spec());
    classify_command("fetch latest Cargo.toml", &result)?;
    parse_package_field(&result.stdout, "version").ok_or_else(|| UpdateError::Failed {
        step: "fetch latest Cargo.toml".to_string(),
        message: "remote Cargo.toml has no package version".to_string(),
    })
}

fn fetch_remote_commit<H: Host>(host: &H) -> Option<String> {
    if !host.has_program("git") {
        return None;
    }
    let result = host.run(&git_ls_remote_spec());
    if !result.ok() {
        return None;
    }
    parse_ls_remote_sha(&result.stdout, MAIN_REF)
}

fn local_commit<H: Host>(host: &H, kind: &InstallKind) -> Option<String> {
    match kind {
        InstallKind::CargoGit { commit, .. } => commit.clone(),
        InstallKind::GitClone { root } => {
            if !host.has_program("git") {
                return None;
            }
            let result = host.run(&git_rev_parse_spec(root));
            if !result.ok() {
                return None;
            }
            let sha = result.stdout.trim();
            if sha.is_empty() {
                None
            } else {
                Some(sha.to_string())
            }
        }
    }
}

fn update_available(current: &str, remote: &RemoteInfo, local_commit: Option<&str>) -> bool {
    if let Some(target) = remote.version.as_deref() {
        if is_newer(target, current) {
            return true;
        }
        if parse_semver(target) != parse_semver(current) && target != current {
            return false;
        }
    }
    match (remote.commit.as_deref(), local_commit) {
        (Some(remote_sha), Some(local_sha)) => !same_commit(remote_sha, local_sha),
        _ => false,
    }
}

fn display_current(version: &str, commit: Option<&str>) -> String {
    match commit {
        Some(sha) => format!("{version} ({})", short_sha(sha)),
        None => version.to_string(),
    }
}

fn display_target(remote: &RemoteInfo) -> String {
    match (&remote.version, &remote.commit) {
        (Some(version), Some(sha)) => format!("{version} ({})", short_sha(sha)),
        (Some(version), None) => version.clone(),
        (None, Some(sha)) => format!("latest ({})", short_sha(sha)),
        (None, None) => "latest".to_string(),
    }
}

fn require_apply_tools<H: Host>(host: &H, kind: &InstallKind) -> Result<(), UpdateError> {
    if !host.has_program("cargo") {
        return Err(UpdateError::MissingCargo);
    }
    if matches!(kind, InstallKind::GitClone { .. }) && !host.has_program("git") {
        return Err(UpdateError::MissingGit);
    }
    Ok(())
}

fn apply_update<H: Host>(host: &H, kind: &InstallKind) -> Result<(), UpdateError> {
    for spec in install_commands(kind) {
        let step = format!("{} {}", spec.program, spec.args.join(" "));
        let result = host.run(&spec);
        classify_command(&step, &result)?;
    }
    Ok(())
}

fn installed_clone_info<H: Host>(
    host: &H,
    kind: &InstallKind,
) -> Result<Option<RemoteInfo>, UpdateError> {
    let InstallKind::GitClone { root } = kind else {
        return Ok(None);
    };
    let manifest = host
        .read_to_string(&root.join("Cargo.toml"))
        .map_err(|message| UpdateError::Failed {
            step: "verify installed clone".to_string(),
            message,
        })?;
    let version = parse_package_field(&manifest, "version").ok_or_else(|| UpdateError::Failed {
        step: "verify installed clone".to_string(),
        message: "clone Cargo.toml has no package version".to_string(),
    })?;
    let commit = local_commit(host, kind).ok_or_else(|| UpdateError::Failed {
        step: "verify installed clone".to_string(),
        message: "could not determine the installed clone commit".to_string(),
    })?;
    Ok(Some(RemoteInfo {
        version: Some(version),
        commit: Some(commit),
    }))
}

/// Commands that apply an update for `kind`, in order.
pub fn install_commands(kind: &InstallKind) -> Vec<CommandSpec> {
    let quiet_git = vec![("GIT_TERMINAL_PROMPT".to_string(), "0".to_string())];
    match kind {
        InstallKind::GitClone { root } => vec![
            CommandSpec {
                program: "git".to_string(),
                args: vec![
                    "-C".to_string(),
                    root.display().to_string(),
                    "pull".to_string(),
                    "--ff-only".to_string(),
                ],
                env: quiet_git.clone(),
                inherit_stdio: true,
            },
            CommandSpec {
                program: "cargo".to_string(),
                args: vec![
                    "install".to_string(),
                    "--path".to_string(),
                    root.display().to_string(),
                    "--locked".to_string(),
                    "--force".to_string(),
                ],
                env: quiet_git,
                inherit_stdio: true,
            },
        ],
        InstallKind::CargoGit { url, .. } => vec![CommandSpec {
            program: "cargo".to_string(),
            args: vec![
                "install".to_string(),
                "--git".to_string(),
                url.clone(),
                "--locked".to_string(),
                "--force".to_string(),
            ],
            env: quiet_git,
            inherit_stdio: true,
        }],
    }
}

fn curl_manifest_spec() -> CommandSpec {
    CommandSpec {
        program: "curl".to_string(),
        args: vec![
            "-fsSL".to_string(),
            "--max-time".to_string(),
            "20".to_string(),
            "-A".to_string(),
            format!("filecraft/{}", env!("CARGO_PKG_VERSION")),
            MANIFEST_URL.to_string(),
        ],
        env: Vec::new(),
        inherit_stdio: false,
    }
}

fn git_ls_remote_spec() -> CommandSpec {
    CommandSpec {
        program: "git".to_string(),
        args: vec![
            "ls-remote".to_string(),
            GIT_URL.to_string(),
            MAIN_REF.to_string(),
        ],
        env: vec![("GIT_TERMINAL_PROMPT".to_string(), "0".to_string())],
        inherit_stdio: false,
    }
}

fn git_rev_parse_spec(root: &Path) -> CommandSpec {
    CommandSpec {
        program: "git".to_string(),
        args: vec![
            "-C".to_string(),
            root.display().to_string(),
            "rev-parse".to_string(),
            "HEAD".to_string(),
        ],
        env: vec![("GIT_TERMINAL_PROMPT".to_string(), "0".to_string())],
        inherit_stdio: false,
    }
}

fn classify_command(step: &str, result: &CommandResult) -> Result<(), UpdateError> {
    if result.ok() {
        return Ok(());
    }
    if let Some(spawn) = &result.spawn_error {
        return Err(match spawn {
            SpawnError::NotFound => match step {
                s if s.starts_with("cargo") => UpdateError::MissingCargo,
                s if s.starts_with("git") => UpdateError::MissingGit,
                s if s.starts_with("curl") || s.starts_with("fetch") => UpdateError::MissingCurl,
                _ => UpdateError::Failed {
                    step: step.to_string(),
                    message: "command not found".to_string(),
                },
            },
            SpawnError::PermissionDenied(msg) => UpdateError::Permission(msg.clone()),
            SpawnError::Other(msg) => UpdateError::Failed {
                step: step.to_string(),
                message: msg.clone(),
            },
        });
    }
    let combined = format!("{}\n{}", result.stdout, result.stderr);
    if looks_like_permission(&combined) {
        return Err(UpdateError::Permission(first_line(&combined)));
    }
    if looks_like_network(&combined) {
        return Err(UpdateError::Network(first_line(&combined)));
    }
    let message = first_line(&combined);
    let message = if message.is_empty() {
        match result.status {
            Some(code) => format!("exited with {code}"),
            None => "failed".to_string(),
        }
    } else {
        message
    };
    Err(UpdateError::Failed {
        step: step.to_string(),
        message,
    })
}

fn looks_like_network(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    const NEEDLES: &[&str] = &[
        "could not resolve",
        "failed to connect",
        "failed to fetch",
        "unable to access",
        "connection refused",
        "connection reset",
        "timed out",
        "timeout",
        "network is unreachable",
        "could not fetch",
        "curl: (6)",
        "curl: (7)",
        "curl: (28)",
        "error sending request",
        "ssl connect error",
    ];
    NEEDLES.iter().any(|n| lower.contains(n))
}

fn looks_like_permission(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    lower.contains("permission denied")
        || lower.contains("operation not permitted")
        || lower.contains("read-only file system")
}

fn first_line(text: &str) -> String {
    text.lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or("")
        .to_string()
}

fn detect_install<H: Host>(host: &H) -> InstallKind {
    if let Some(exe) = host.current_exe() {
        if let Some(root) = git_root_from_dev_binary(&exe, host) {
            return InstallKind::GitClone { root };
        }
    }
    if let Some(root) = host.install_root() {
        let crates = root.join(".crates.toml");
        if let Ok(text) = host.read_to_string(&crates) {
            if let Some(source) = parse_crates_filecraft(&text) {
                match source {
                    CargoSource::Path(path) if is_filecraft_git_root(&path, host) => {
                        return InstallKind::GitClone { root: path };
                    }
                    CargoSource::Git { url, commit } => {
                        return InstallKind::CargoGit { url, commit };
                    }
                    CargoSource::Path(_) | CargoSource::Registry => {}
                }
            }
        }
    }
    InstallKind::CargoGit {
        url: GIT_URL.to_string(),
        commit: None,
    }
}

fn git_root_from_dev_binary<H: Host>(exe: &Path, host: &H) -> Option<PathBuf> {
    for ancestor in exe.ancestors() {
        if ancestor.file_name().is_some_and(|name| name == "target") {
            let root = ancestor.parent()?;
            if is_filecraft_git_root(root, host) {
                return Some(root.to_path_buf());
            }
        }
    }
    None
}

fn is_filecraft_git_root<H: Host>(path: &Path, host: &H) -> bool {
    let git = path.join(".git");
    if !host.is_dir(&git) && !host.is_file(&git) {
        return false;
    }
    let cargo = path.join("Cargo.toml");
    host.is_file(&cargo)
        && host
            .read_to_string(&cargo)
            .ok()
            .is_some_and(|text| parse_package_field(&text, "name").as_deref() == Some("filecraft"))
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum CargoSource {
    Path(PathBuf),
    Git { url: String, commit: Option<String> },
    Registry,
}

fn parse_crates_filecraft(text: &str) -> Option<CargoSource> {
    for line in text.lines() {
        let line = line.trim();
        let Some(rest) = line.strip_prefix('"') else {
            continue;
        };
        let Some(rest) = rest.strip_prefix("filecraft ") else {
            continue;
        };
        let Some(open) = rest.find('(') else {
            continue;
        };
        let Some(close) = rest.find(")\"") else {
            continue;
        };
        if close < open {
            continue;
        }
        return Some(parse_cargo_source(&rest[open + 1..close]));
    }
    None
}

fn parse_cargo_source(source: &str) -> CargoSource {
    if let Some(path) = source.strip_prefix("path+file://") {
        percent_decode(path)
            .map(PathBuf::from)
            .map(CargoSource::Path)
            .unwrap_or(CargoSource::Registry)
    } else if let Some(git) = source.strip_prefix("git+") {
        match git.rsplit_once('#') {
            Some((url, commit)) => CargoSource::Git {
                url: url.to_string(),
                commit: Some(commit.to_string()),
            },
            None => CargoSource::Git {
                url: git.to_string(),
                commit: None,
            },
        }
    } else {
        CargoSource::Registry
    }
}

fn percent_decode(value: &str) -> Option<String> {
    let bytes = value.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' {
            let high = hex_value(*bytes.get(index + 1)?)?;
            let low = hex_value(*bytes.get(index + 2)?)?;
            decoded.push((high << 4) | low);
            index += 3;
        } else {
            decoded.push(bytes[index]);
            index += 1;
        }
    }
    String::from_utf8(decoded).ok()
}

fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

fn parse_package_field(toml: &str, field: &str) -> Option<String> {
    let mut in_package = false;
    for line in toml.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('[') {
            in_package = trimmed == "[package]";
            continue;
        }
        if !in_package || trimmed.starts_with('#') {
            continue;
        }
        let Some(rest) = trimmed.strip_prefix(field) else {
            continue;
        };
        let rest = rest.trim_start();
        let Some(rest) = rest.strip_prefix('=') else {
            continue;
        };
        let value = rest.trim().trim_matches('"');
        if !value.is_empty() {
            return Some(value.to_string());
        }
    }
    None
}

fn parse_ls_remote_sha(output: &str, want: &str) -> Option<String> {
    let mut first = None;
    for line in output.lines() {
        let mut bits = line.split_whitespace();
        let Some(sha) = bits.next() else {
            continue;
        };
        if sha.is_empty() {
            continue;
        }
        if first.is_none() {
            first = Some(sha.to_string());
        }
        let name = bits.next().unwrap_or("");
        if name == want || name.ends_with(want) {
            return Some(sha.to_string());
        }
    }
    first
}

fn parse_semver(version: &str) -> Option<(u64, u64, u64)> {
    let mut parts = version.trim().split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next()?.parse().ok()?;
    let patch = parts.next()?.parse().ok()?;
    Some((major, minor, patch))
}

fn is_newer(target: &str, current: &str) -> bool {
    match (parse_semver(target), parse_semver(current)) {
        (Some(target), Some(current)) => target > current,
        _ => target != current,
    }
}

fn same_commit(left: &str, right: &str) -> bool {
    let left = left.trim().to_ascii_lowercase();
    let right = right.trim().to_ascii_lowercase();
    if left.is_empty() || right.is_empty() {
        return false;
    }
    left == right || left.starts_with(&right) || right.starts_with(&left)
}

fn short_sha(sha: &str) -> &str {
    let end = sha
        .char_indices()
        .nth(7)
        .map(|(i, _)| i)
        .unwrap_or(sha.len());
    &sha[..end]
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::collections::{HashMap, HashSet};

    struct FakeHost {
        version: String,
        exe: Option<PathBuf>,
        install_root: Option<PathBuf>,
        path_env: Option<String>,
        home: Option<PathBuf>,
        shell: Option<String>,
        files: HashMap<PathBuf, String>,
        dirs: HashSet<PathBuf>,
        programs: HashSet<String>,
        curl: CommandResult,
        ls_remote: CommandResult,
        rev_parse: CommandResult,
        git_pull: CommandResult,
        cargo_install: CommandResult,
        ran: RefCell<Vec<CommandSpec>>,
    }

    impl FakeHost {
        fn new() -> Self {
            Self {
                version: "0.1.0".to_string(),
                exe: Some(PathBuf::from("/cargo/bin/filecraft")),
                install_root: Some(PathBuf::from("/cargo")),
                path_env: Some("/cargo/bin".to_string()),
                home: Some(PathBuf::from("/home/tester")),
                shell: Some("/bin/zsh".to_string()),
                files: HashMap::new(),
                dirs: HashSet::new(),
                programs: ["cargo", "git", "curl"]
                    .into_iter()
                    .map(str::to_string)
                    .collect(),
                curl: CommandResult::success(
                    "[package]\nname = \"filecraft\"\nversion = \"0.1.0\"\n",
                ),
                ls_remote: CommandResult::success(
                    "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\trefs/heads/main\n",
                ),
                rev_parse: CommandResult::success("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\n"),
                git_pull: CommandResult::success(""),
                cargo_install: CommandResult::success(""),
                ran: RefCell::new(Vec::new()),
            }
        }

        fn with_crates(mut self, line: &str) -> Self {
            self.files.insert(
                PathBuf::from("/cargo/.crates.toml"),
                format!("[v1]\n{line}\n"),
            );
            self
        }

        fn with_clone(mut self, root: &str) -> Self {
            let root = PathBuf::from(root);
            self.dirs.insert(root.join(".git"));
            self.files.insert(
                root.join("Cargo.toml"),
                "[package]\nname = \"filecraft\"\nversion = \"0.1.0\"\n".into(),
            );
            self
        }
    }

    impl Host for FakeHost {
        fn current_exe(&self) -> Option<PathBuf> {
            self.exe.clone()
        }
        fn install_root(&self) -> Option<PathBuf> {
            self.install_root.clone()
        }
        fn path_env(&self) -> Option<String> {
            self.path_env.clone()
        }
        fn home(&self) -> Option<PathBuf> {
            self.home.clone()
        }
        fn shell(&self) -> Option<String> {
            self.shell.clone()
        }
        fn current_version(&self) -> &str {
            &self.version
        }
        fn is_dir(&self, path: &Path) -> bool {
            self.dirs.contains(path)
        }
        fn is_file(&self, path: &Path) -> bool {
            self.files.contains_key(path)
        }
        fn read_to_string(&self, path: &Path) -> Result<String, String> {
            self.files
                .get(path)
                .cloned()
                .ok_or_else(|| "not found".to_string())
        }
        fn has_program(&self, name: &str) -> bool {
            self.programs.contains(name)
        }
        fn run(&self, spec: &CommandSpec) -> CommandResult {
            self.ran.borrow_mut().push(spec.clone());
            if spec.program == "curl" {
                return self.curl.clone();
            }
            if spec.program == "git" && spec.args.iter().any(|a| a == "ls-remote") {
                return self.ls_remote.clone();
            }
            if spec.program == "git" && spec.args.iter().any(|a| a == "rev-parse") {
                return self.rev_parse.clone();
            }
            if spec.program == "git" && spec.args.iter().any(|a| a == "pull") {
                return self.git_pull.clone();
            }
            if spec.program == "cargo" {
                return self.cargo_install.clone();
            }
            CommandResult {
                status: None,
                stdout: String::new(),
                stderr: String::new(),
                spawn_error: Some(SpawnError::NotFound),
            }
        }
    }

    fn programs_ran(host: &FakeHost) -> Vec<String> {
        host.ran
            .borrow()
            .iter()
            .map(|spec| spec.program.clone())
            .collect()
    }

    fn cargo_toml(version: &str) -> String {
        format!("[package]\nname = \"filecraft\"\nversion = \"{version}\"\n")
    }

    #[test]
    fn parse_package_version_reads_package_table() {
        let toml = "[package]\nname = \"filecraft\"\nversion = \"0.2.0\"\n\
                    [dependencies]\nversion = \"ignore\"\n";
        assert_eq!(
            parse_package_field(toml, "version").as_deref(),
            Some("0.2.0")
        );
        assert_eq!(
            parse_package_field(toml, "name").as_deref(),
            Some("filecraft")
        );
    }

    #[test]
    fn parse_crates_toml_path_and_git() {
        let path = parse_crates_filecraft(
            r#""filecraft 0.1.0 (path+file:///Users/me/filecraft)" = ["filecraft"]"#,
        );
        assert_eq!(
            path,
            Some(CargoSource::Path(PathBuf::from("/Users/me/filecraft")))
        );
        let escaped_path = parse_crates_filecraft(
            r#""filecraft 0.1.0 (path+file:///Users/me/File%20Craft%23one)" = ["filecraft"]"#,
        );
        assert_eq!(
            escaped_path,
            Some(CargoSource::Path(PathBuf::from("/Users/me/File Craft#one")))
        );
        let git = parse_crates_filecraft(
            r#""filecraft 0.1.0 (git+https://github.com/hsuanchenlin/filecraft.git#abc123)" = ["filecraft"]"#,
        );
        assert_eq!(
            git,
            Some(CargoSource::Git {
                url: GIT_URL.to_string(),
                commit: Some("abc123".to_string()),
            })
        );
    }

    #[test]
    fn detect_dev_binary_inside_target() {
        let mut host = FakeHost::new().with_clone("/work/filecraft");
        host.exe = Some(PathBuf::from("/work/filecraft/target/debug/filecraft"));
        assert_eq!(
            detect_install(&host),
            InstallKind::GitClone {
                root: PathBuf::from("/work/filecraft"),
            }
        );
    }

    #[test]
    fn detect_path_install_from_crates_toml() {
        let host = FakeHost::new()
            .with_clone("/Users/me/filecraft")
            .with_crates(r#""filecraft 0.1.0 (path+file:///Users/me/filecraft)" = ["filecraft"]"#);
        assert_eq!(
            detect_install(&host),
            InstallKind::GitClone {
                root: PathBuf::from("/Users/me/filecraft"),
            }
        );
    }

    #[test]
    fn detect_git_install_from_crates_toml() {
        let host = FakeHost::new().with_crates(
            r#""filecraft 0.1.0 (git+https://github.com/hsuanchenlin/filecraft.git#deadbeef)" = ["filecraft"]"#,
        );
        assert_eq!(
            detect_install(&host),
            InstallKind::CargoGit {
                url: GIT_URL.to_string(),
                commit: Some("deadbeef".to_string()),
            }
        );
    }

    #[test]
    fn detect_falls_back_to_canonical_git_url() {
        let host = FakeHost::new();
        assert_eq!(
            detect_install(&host),
            InstallKind::CargoGit {
                url: GIT_URL.to_string(),
                commit: None,
            }
        );
    }

    #[test]
    fn cargo_bin_does_not_inherit_a_home_clone() {
        let mut host = FakeHost::new().with_clone("/Users/me/filecraft");
        host.exe = Some(PathBuf::from("/Users/me/.cargo/bin/filecraft"));
        // Walking $HOME would hit ~/filecraft; only `target/` bins count.
        assert_eq!(
            detect_install(&host),
            InstallKind::CargoGit {
                url: GIT_URL.to_string(),
                commit: None,
            }
        );
    }

    #[test]
    fn install_commands_for_clone_pull_then_path_install() {
        let kind = InstallKind::GitClone {
            root: PathBuf::from("/work/filecraft"),
        };
        let cmds = install_commands(&kind);
        assert_eq!(cmds[0].program, "git");
        assert!(cmds[0].args.contains(&"pull".to_string()));
        assert!(cmds[0].args.contains(&"--ff-only".to_string()));
        assert_eq!(cmds[1].program, "cargo");
        assert_eq!(
            cmds[1].args,
            vec![
                "install",
                "--path",
                "/work/filecraft",
                "--locked",
                "--force"
            ]
        );
    }

    #[test]
    fn install_commands_for_git_use_locked_force() {
        let kind = InstallKind::CargoGit {
            url: GIT_URL.to_string(),
            commit: None,
        };
        let cmds = install_commands(&kind);
        assert_eq!(
            cmds[0].args,
            vec!["install", "--git", GIT_URL, "--locked", "--force"]
        );
    }

    #[test]
    fn check_up_to_date_does_not_install() {
        let host = FakeHost::new().with_crates(
            r#""filecraft 0.1.0 (git+https://github.com/hsuanchenlin/filecraft.git#aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa)" = ["filecraft"]"#,
        );
        let report = run_with(true, &host).unwrap();
        assert_eq!(report.status, UpdateStatus::UpToDate);
        assert!(report.current_version.starts_with("0.1.0"));
        assert!(report.target_version.starts_with("0.1.0"));
        assert!(report.to_string().contains("ok: already up to date"));
        assert!(!programs_ran(&host).contains(&"cargo".to_string()));
    }

    #[test]
    fn check_reports_newer_package_version() {
        let mut host = FakeHost::new().with_crates(
            r#""filecraft 0.1.0 (git+https://github.com/hsuanchenlin/filecraft.git#aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa)" = ["filecraft"]"#,
        );
        host.curl = CommandResult::success(cargo_toml("0.2.0"));
        let report = run_with(true, &host).unwrap();
        assert_eq!(report.status, UpdateStatus::Available);
        assert!(report.to_string().contains("update available:"));
        assert!(!programs_ran(&host).contains(&"cargo".to_string()));
    }

    #[test]
    fn check_reports_newer_commit_at_same_version() {
        let host = FakeHost::new().with_crates(
            r#""filecraft 0.1.0 (git+https://github.com/hsuanchenlin/filecraft.git#bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb)" = ["filecraft"]"#,
        );
        let report = run_with(true, &host).unwrap();
        assert_eq!(report.status, UpdateStatus::Available);
    }

    #[test]
    fn apply_git_install_when_available() {
        let mut host = FakeHost::new().with_crates(
            r#""filecraft 0.1.0 (git+https://github.com/hsuanchenlin/filecraft.git#bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb)" = ["filecraft"]"#,
        );
        host.curl = CommandResult::success(cargo_toml("0.2.0"));
        let report = run_with(false, &host).unwrap();
        assert_eq!(report.status, UpdateStatus::Updated);
        assert!(report.to_string().contains("ok: updated to"));
        assert!(programs_ran(&host).contains(&"cargo".to_string()));
        let cargo = host
            .ran
            .borrow()
            .iter()
            .find(|spec| spec.program == "cargo")
            .cloned()
            .unwrap();
        assert!(cargo.args.contains(&"--git".to_string()));
        assert!(cargo.args.contains(&"--locked".to_string()));
    }

    #[test]
    fn apply_clone_pulls_then_installs() {
        let mut host = FakeHost::new()
            .with_clone("/work/filecraft")
            .with_crates(r#""filecraft 0.1.0 (path+file:///work/filecraft)" = ["filecraft"]"#);
        host.curl = CommandResult::success(cargo_toml("0.2.0"));
        let report = run_with(false, &host).unwrap();
        assert_eq!(report.status, UpdateStatus::Updated);
        assert_eq!(report.target_version, "0.1.0 (aaaaaaa)");
        assert!(!report.target_version.starts_with("0.2.0"));
        let programs = programs_ran(&host);
        assert!(programs.contains(&"git".to_string()));
        assert!(programs.contains(&"cargo".to_string()));
        let cargo = host
            .ran
            .borrow()
            .iter()
            .find(|spec| spec.program == "cargo")
            .cloned()
            .unwrap();
        assert!(cargo.args.contains(&"--path".to_string()));
        let ran = host.ran.borrow();
        let install_index = ran.iter().position(|spec| spec.program == "cargo").unwrap();
        let verification_index = ran
            .iter()
            .rposition(|spec| spec.args.iter().any(|arg| arg == "rev-parse"))
            .unwrap();
        assert!(verification_index > install_index);
    }

    #[test]
    fn apply_up_to_date_skips_install() {
        let host = FakeHost::new().with_crates(
            r#""filecraft 0.1.0 (git+https://github.com/hsuanchenlin/filecraft.git#aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa)" = ["filecraft"]"#,
        );
        let report = run_with(false, &host).unwrap();
        assert_eq!(report.status, UpdateStatus::UpToDate);
        assert!(!programs_ran(&host).contains(&"cargo".to_string()));
    }

    #[test]
    fn missing_cargo_is_a_graceful_error() {
        let mut host = FakeHost::new();
        host.programs.remove("cargo");
        host.curl = CommandResult::success(cargo_toml("0.2.0"));
        let err = run_with(false, &host).unwrap_err();
        assert_eq!(err, UpdateError::MissingCargo);
        assert!(err.to_string().contains("cargo is not installed"));
    }

    #[test]
    fn missing_curl_on_check_is_a_graceful_error() {
        let mut host = FakeHost::new();
        host.programs.remove("curl");
        let err = run_with(true, &host).unwrap_err();
        assert_eq!(err, UpdateError::MissingCurl);
    }

    #[test]
    fn network_failure_on_check_does_not_panic() {
        let mut host = FakeHost::new();
        host.curl = CommandResult {
            status: Some(6),
            stdout: String::new(),
            stderr: "curl: (6) Could not resolve host: raw.githubusercontent.com\n".into(),
            spawn_error: None,
        };
        let err = run_with(true, &host).unwrap_err();
        match err {
            UpdateError::Network(msg) => {
                assert!(msg.to_ascii_lowercase().contains("could not resolve"))
            }
            other => panic!("expected network error, got {other:?}"),
        }
    }

    #[test]
    fn permission_error_on_install_does_not_panic() {
        let mut host = FakeHost::new().with_crates(
            r#""filecraft 0.1.0 (git+https://github.com/hsuanchenlin/filecraft.git#bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb)" = ["filecraft"]"#,
        );
        host.curl = CommandResult::success(cargo_toml("0.2.0"));
        host.cargo_install = CommandResult {
            status: Some(101),
            stdout: String::new(),
            stderr: "Permission denied (os error 13)\n".into(),
            spawn_error: None,
        };
        let err = run_with(false, &host).unwrap_err();
        match err {
            UpdateError::Permission(msg) => {
                assert!(msg.to_ascii_lowercase().contains("permission denied"))
            }
            other => panic!("expected permission error, got {other:?}"),
        }
    }

    #[test]
    fn spawn_permission_denied_is_classified() {
        let err = classify_command(
            "cargo install",
            &CommandResult {
                status: None,
                stdout: String::new(),
                stderr: String::new(),
                spawn_error: Some(SpawnError::PermissionDenied("permission denied".into())),
            },
        )
        .unwrap_err();
        assert!(matches!(err, UpdateError::Permission(_)));
    }

    /// The reported bug, from the updater's side: the install succeeds
    /// and the shell still cannot find the binary. Every report says so.
    #[test]
    fn a_report_warns_when_the_install_directory_is_not_on_path() {
        let mut host = FakeHost::new();
        host.path_env = Some("/opt/homebrew/bin:/usr/bin:/bin".to_string());

        let report = run_with(true, &host).unwrap();
        let advice = report
            .path_advice
            .as_ref()
            .expect("cargo bin is not on PATH");
        assert_eq!(advice.dir, PathBuf::from("/cargo/bin"));

        let text = report.to_string();
        assert!(text.contains("/cargo/bin is not on your PATH"), "{text}");
        assert!(text.contains("~/.zshrc"), "{text}");
        assert!(text.contains("./install.sh"), "{text}");
    }

    /// ...and stays quiet when it can, so the advice keeps its meaning.
    #[test]
    fn a_report_says_nothing_about_path_when_the_shell_can_find_the_binary() {
        let report = run_with(true, &FakeHost::new()).unwrap();
        assert_eq!(report.path_advice, None);
        assert!(!report.to_string().contains("PATH"));
    }

    /// `cargo install` writes to CARGO_INSTALL_ROOT when it is set, so the
    /// advice has to name that directory rather than CARGO_HOME's.
    #[test]
    fn the_install_root_is_the_directory_the_advice_talks_about() {
        let mut host = FakeHost::new();
        host.install_root = Some(PathBuf::from("/elsewhere"));
        host.exe = Some(PathBuf::from("/work/filecraft/target/debug/filecraft"));
        host.path_env = Some("/usr/bin".to_string());

        let advice = run_with(true, &host)
            .unwrap()
            .path_advice
            .expect("/elsewhere/bin is not on PATH");
        assert_eq!(advice.dir, PathBuf::from("/elsewhere/bin"));
        assert!(advice.from_build_tree);
    }

    #[test]
    fn report_lists_current_and_target_versions() {
        let report = UpdateReport {
            current_version: "0.1.0".into(),
            target_version: "0.2.0".into(),
            source: "cargo install --git https://example".into(),
            status: UpdateStatus::Updated,
            path_advice: None,
        };
        let text = report.to_string();
        assert!(text.contains("current version: 0.1.0"));
        assert!(text.contains("target version:  0.2.0"));
        assert!(text.contains("ok: updated to 0.2.0"));
    }
}
