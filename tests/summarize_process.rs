//! The real [`ProcessRunner`], end to end.
//!
//! Everything else about the summarizer is a pure function tested in
//! `src/summarize.rs`; this is the one place a child process is actually
//! spawned. Stub providers stand in for the real CLIs on a `$PATH` this
//! binary controls, so no AI tool is needed and nothing reaches a network.
//! Each stub is named after a provider in the fixed table, and each one
//! exercises a different way a run can end.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use filecraft::summarize::{
    output_path_with, Job, JobSpec, Outcome, ProcessRunner, Provider, Runner,
};

/// Longest a stub run may take before the test gives up. Generous: it is
/// a failure threshold, not a timing assertion.
const PATIENCE: Duration = Duration::from_secs(20);

/// Put a directory of stub providers at the front of this process's
/// `$PATH`, once per test binary. The directory is deliberately kept:
/// it must outlive every test in the binary, including ones running in
/// parallel, and the OS reclaims it as an ordinary temp directory.
fn stub_path() -> &'static Path {
    static DIR: OnceLock<PathBuf> = OnceLock::new();
    DIR.get_or_init(|| {
        let dir = tempfile::Builder::new()
            .prefix("filecraft-stub-providers-")
            .tempdir()
            .unwrap()
            .keep();
        // `agy`: writes the file it was asked to write, the happy path.
        write_stub(
            &dir,
            "agy",
            r#"for arg in "$@"; do prompt="$arg"; done
path=$(printf '%s\n' "$prompt" | grep -A1 'this exact path:' | tail -1)
printf '# Summary\n\nwritten by the stub provider\n' > "$path"
"#,
        );
        // `claude`: prints the summary instead of writing it.
        write_stub(
            &dir,
            "claude",
            "sleep 1\nprintf '# Summary\\n\\nfrom stdout\\n'\n",
        );
        // `codex`: fails, and says why on stderr.
        write_stub(
            &dir,
            "codex",
            "printf 'progress: contacting provider\\n'\nprintf 'codex: not logged in\\n' >&2\nexit 1\n",
        );
        // `grok`: never finishes on its own.
        write_stub(&dir, "grok", "sleep 600\n");
        // `kimi`: succeeds silently, writing nothing at all.
        write_stub(&dir, "kimi", "exit 0\n");

        let previous = std::env::var("PATH").unwrap_or_default();
        std::env::set_var("PATH", format!("{}:{previous}", dir.display()));
        dir
    })
}

fn write_stub(dir: &Path, name: &str, body: &str) {
    let path = dir.join(name);
    std::fs::write(&path, format!("#!/bin/sh\n{body}")).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
}

/// A two-file summary request rooted in a fresh fixture directory.
fn spec_in(tmp: &tempfile::TempDir, provider: Provider) -> JobSpec {
    let first = tmp.path().join("report.pdf");
    std::fs::write(&first, "%PDF-1.4").unwrap();
    let second = tmp.path().join("notes.md");
    std::fs::write(&second, "# notes").unwrap();
    let output = output_path_with(&first, "20260829-101500", &|p| p.exists());
    JobSpec::new(provider, vec![first, second], output).unwrap()
}

/// Poll until the run reports something, or the patience runs out.
fn wait_for(job: &mut Box<dyn Job>) -> Outcome {
    let deadline = Instant::now() + PATIENCE;
    loop {
        if let Some(outcome) = job.poll() {
            return outcome;
        }
        assert!(
            Instant::now() < deadline,
            "the stub run never reported a result"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn a_provider_that_writes_the_file_reports_that_file() {
    stub_path();
    let tmp = tempfile::tempdir().unwrap();
    let spec = spec_in(&tmp, Provider::Ag);
    let mut job = ProcessRunner.start(&spec).unwrap();

    assert_eq!(wait_for(&mut job), Outcome::Written(spec.output.clone()));
    let written = std::fs::read_to_string(&spec.output).unwrap();
    assert!(
        written.contains("written by the stub provider"),
        "{written}"
    );
    // The sources are untouched: a summary only ever adds a file.
    assert_eq!(std::fs::read_to_string(&spec.files[1]).unwrap(), "# notes");
}

#[test]
fn a_provider_that_only_prints_has_its_stdout_saved_as_the_summary() {
    stub_path();
    let tmp = tempfile::tempdir().unwrap();
    let spec = spec_in(&tmp, Provider::Cc);
    let mut job = ProcessRunner.start(&spec).unwrap();

    assert_eq!(wait_for(&mut job), Outcome::Written(spec.output.clone()));
    let written = std::fs::read_to_string(&spec.output).unwrap();
    assert!(written.contains("from stdout"), "{written}");
}

#[test]
fn stdout_fallback_never_overwrites_a_file_created_during_the_run() {
    stub_path();
    let tmp = tempfile::tempdir().unwrap();
    let spec = spec_in(&tmp, Provider::Cc);
    let mut job = ProcessRunner.start(&spec).unwrap();
    std::fs::File::create(&spec.output).unwrap();

    let outcome = wait_for(&mut job);
    assert!(matches!(outcome, Outcome::Failed(_)), "{outcome:?}");
    assert_eq!(std::fs::metadata(&spec.output).unwrap().len(), 0);
}

#[test]
fn a_failing_provider_does_not_save_its_stdout_as_a_summary() {
    stub_path();
    let tmp = tempfile::tempdir().unwrap();
    let spec = spec_in(&tmp, Provider::Co);
    let mut job = ProcessRunner.start(&spec).unwrap();

    assert_eq!(
        wait_for(&mut job),
        Outcome::Failed("codex: not logged in".to_string())
    );
    assert!(!spec.output.exists(), "a failed run must leave no summary");
}

#[test]
fn a_silent_provider_is_a_failure_not_an_empty_summary() {
    stub_path();
    let tmp = tempfile::tempdir().unwrap();
    let spec = spec_in(&tmp, Provider::Ki);
    let mut job = ProcessRunner.start(&spec).unwrap();

    assert_eq!(
        wait_for(&mut job),
        Outcome::Failed("the provider wrote nothing".to_string())
    );
    assert!(!spec.output.exists());
}

#[test]
fn a_long_run_stays_in_flight_and_terminate_ends_it() {
    stub_path();
    let tmp = tempfile::tempdir().unwrap();
    let spec = spec_in(&tmp, Provider::Gk);
    let mut job = ProcessRunner.start(&spec).unwrap();

    // Still going: this is what keeps the TUI answering keys.
    std::thread::sleep(Duration::from_millis(200));
    assert_eq!(
        job.poll(),
        None,
        "a sleeping provider must still be running"
    );

    // This is what `y` at the quit prompt does.
    job.terminate();
    let outcome = wait_for(&mut job);
    assert!(
        matches!(outcome, Outcome::Failed(_)),
        "a terminated run has no summary, got {outcome:?}"
    );
    assert!(!spec.output.exists());
}

#[test]
fn a_run_that_cannot_be_spawned_is_a_plain_error_and_never_a_job() {
    stub_path();
    let tmp = tempfile::tempdir().unwrap();
    let mut spec = spec_in(&tmp, Provider::Ag);
    // The same failure an uninstalled provider produces, reached without
    // touching this process's `$PATH` - which every other test shares.
    spec.cwd = tmp.path().join("gone");

    let error = ProcessRunner
        .start(&spec)
        .err()
        .expect("a spawn that cannot happen is not a running job");
    assert!(error.starts_with("could not run 'agy'"), "{error}");
}
