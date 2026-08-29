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
        // It finds the path in the prompt, so this also proves the prompt
        // arrived whole and named the output.
        write_stub(
            &dir,
            "agy",
            r#"path=$(grep -A1 'exact absolute path:' "$PROMPT_FILE" | tail -1)
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
        // `grok`: never finishes on its own. `exec` so the shell is
        // *replaced* by the sleep rather than parenting it: `terminate`
        // kills the process Filecraft spawned, and only an `exec`ed body
        // guarantees that is the one holding the pipes open.
        write_stub(&dir, "grok", "exec sleep 600\n");
        // `kimi`: succeeds silently, writing nothing at all.
        write_stub(&dir, "kimi", "exit 0\n");

        let previous = std::env::var("PATH").unwrap_or_default();
        std::env::set_var("PATH", format!("{}:{previous}", dir.display()));
        dir
    })
}

/// Where a stub leaves the flags it was handed, one per line, and the
/// prompt, verbatim. Both are written atomically and the prompt last, so
/// a test can poll for the prompt file and then read a settled pair.
const FLAGS_FILE: &str = ".stub-flags";
const PROMPT_FILE: &str = ".stub-prompt";

/// The first half of every stub: record the argv it was handed.
///
/// The program's own name leads, so the recording is the whole command
/// line and not just its tail - `$PATH` resolving the wrong program would
/// show up here. Every provider takes the prompt as its final argument,
/// so everything between is a flag.
const RECORD: &str = r#"FLAGS_FILE=.stub-flags
PROMPT_FILE=.stub-prompt
basename "$0" > "$FLAGS_FILE.tmp"
n=$#
i=1
for arg in "$@"; do
  if [ "$i" -lt "$n" ]; then
    printf '%s
' "$arg" >> "$FLAGS_FILE.tmp"
  else
    printf '%s' "$arg" > "$PROMPT_FILE.tmp"
  fi
  i=$((i + 1))
done
mv "$FLAGS_FILE.tmp" "$FLAGS_FILE"
mv "$PROMPT_FILE.tmp" "$PROMPT_FILE"
last=""
prev=""
for arg in "$@"; do prev="$last"; last="$arg"; done
"#;

/// The second half: refuse the argv the real CLI refuses.
///
/// This is the bug that shipped - a prompt appended as a bare trailing
/// word - reproduced in a stub, so every test in this file is also a test
/// that the flags are right. The `agy` and `kimi` messages are the real
/// ones, taken from the installed CLIs.
fn contract(name: &str) -> &'static str {
    match name {
        "agy" => {
            r#"if [ "$prev" != "-p" ]; then
  printf 'Error: unexpected argument "%s".
' "$last" >&2
  printf 'Prompts are read only from -p/--print, -i/--prompt-interactive, or stdin, so this argument would have been ignored.
' >&2
  exit 1
fi
"#
        }
        "claude" => {
            r#"if [ "$prev" != "-p" ]; then
  printf 'claude: a prompt without --print opens an interactive session
' >&2
  exit 1
fi
"#
        }
        // `codex exec` is the non-interactive subcommand, and neither
        // `-a` nor `-q` exists under it. Its `-p` is `--profile`.
        "codex" => {
            r#"if [ "$1" != "exec" ]; then
  printf 'codex: a bare prompt opens the interactive TUI
' >&2
  exit 1
fi
for arg in "$@"; do
  case "$arg" in
    -a|--ask-for-approval|-q)
      printf "error: unexpected argument '%s' found
" "$arg" >&2
      exit 1
      ;;
  esac
done
"#
        }
        "grok" => {
            r#"if [ "$prev" != "-p" ]; then
  printf 'grok: a bare prompt opens the interactive TUI
' >&2
  exit 1
fi
"#
        }
        // `kimi`'s prompt mode carries its own permissions and refuses to
        // be combined with a yolo flag.
        "kimi" => {
            r#"for arg in "$@"; do
  case "$arg" in
    --yolo|-y|--auto)
      printf 'error: Cannot combine --prompt with %s.
' "$arg" >&2
      exit 1
      ;;
  esac
done
if [ "$prev" != "-p" ]; then
  printf 'kimi: a bare prompt opens an interactive session
' >&2
  exit 1
fi
"#
        }
        other => panic!("no argument contract written for the stub {other}"),
    }
}

fn write_stub(dir: &Path, name: &str, body: &str) {
    let path = dir.join(name);
    std::fs::write(
        &path,
        format!("#!/bin/sh\n{RECORD}{}{body}", contract(name)),
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
}

/// The flags and the prompt a stub recorded, once both have settled.
fn recorded_argv(cwd: &Path) -> (Vec<String>, String) {
    let deadline = Instant::now() + PATIENCE;
    let prompt_file = cwd.join(PROMPT_FILE);
    loop {
        if let Ok(prompt) = std::fs::read_to_string(&prompt_file) {
            let flags = std::fs::read_to_string(cwd.join(FLAGS_FILE)).unwrap();
            return (flags.lines().map(str::to_string).collect(), prompt);
        }
        assert!(
            Instant::now() < deadline,
            "the stub never recorded the argv it was handed"
        );
        std::thread::sleep(Duration::from_millis(20));
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

/// The regression this file exists for: every provider must be handed
/// its prompt through the flag its own CLI requires, as one argument.
///
/// The stubs refuse a bare trailing prompt exactly as the real CLIs do,
/// so a run that reaches its body has already proved its flags parse.
#[test]
fn every_provider_is_handed_its_prompt_through_the_flag_its_cli_requires() {
    stub_path();
    let expected: [(Provider, &[&str]); 5] = [
        (
            Provider::Ag,
            &["agy", "--dangerously-skip-permissions", "-p"],
        ),
        (
            Provider::Cc,
            &["claude", "--dangerously-skip-permissions", "-p"],
        ),
        (
            Provider::Co,
            &["codex", "exec", "-p", "lavish", "--skip-git-repo-check"],
        ),
        (Provider::Gk, &["grok", "--always-approve", "-p"]),
        (Provider::Ki, &["kimi", "-p"]),
    ];

    for (provider, argv) in expected {
        let tmp = tempfile::tempdir().unwrap();
        let spec = spec_in(&tmp, provider);
        let mut job = ProcessRunner.start(&spec).unwrap();

        // Recorded before the stub's body, so `grok`'s endless sleep is
        // read the same way as a run that finishes on its own.
        let (flags, prompt) = recorded_argv(&spec.cwd);
        job.terminate();

        // `flags[0]` is the program: the child was spawned by name, so
        // this is what `$PATH` resolved and what a failure would name.
        assert_eq!(
            flags,
            argv.iter().map(|w| (*w).to_string()).collect::<Vec<_>>(),
            "{} was handed the wrong command line",
            provider.code()
        );
        assert_eq!(flags[0], provider.program());
        // One argument, whole - newlines, quotes and all.
        assert_eq!(prompt, spec.prompt(), "{} prompt", provider.code());
        assert!(prompt.contains(&spec.output.display().to_string()));
    }
}

/// The failure the captain hit, reproduced against the stub that refuses
/// it: a prompt appended as a bare trailing word is not a prompt.
#[test]
fn a_bare_trailing_prompt_is_refused_the_way_the_real_cli_refuses_it() {
    stub_path();
    let tmp = tempfile::tempdir().unwrap();
    let spec = spec_in(&tmp, Provider::Ag);

    // What the summarizer used to build: the fixed line, then the prompt.
    let mut argv = Provider::Ag.base_argv();
    argv.push(spec.prompt());
    let refusal = std::process::Command::new(&argv[0])
        .args(&argv[1..])
        .current_dir(&spec.cwd)
        .output()
        .unwrap();
    assert!(!refusal.status.success());
    assert!(
        String::from_utf8_lossy(&refusal.stderr).contains("Prompts are read only from -p/--print"),
        "{}",
        String::from_utf8_lossy(&refusal.stderr)
    );

    // What it builds now: the same line with `-p` before the prompt.
    assert_eq!(spec.argv(), Provider::Ag.argv_with_prompt(&spec.prompt()));
    let mut job = ProcessRunner.start(&spec).unwrap();
    assert_eq!(wait_for(&mut job), Outcome::Written(spec.output.clone()));
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
fn an_existing_output_prevents_the_provider_from_starting() {
    stub_path();
    let tmp = tempfile::tempdir().unwrap();
    let spec = spec_in(&tmp, Provider::Cc);
    std::fs::write(&spec.output, "keep me").unwrap();

    let error = ProcessRunner.start(&spec).err().unwrap();
    assert!(error.starts_with("could not reserve"), "{error}");
    assert_eq!(std::fs::read_to_string(&spec.output).unwrap(), "keep me");
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
    let artifact = std::fs::read_to_string(&spec.output).unwrap();
    assert!(artifact.contains("Summary failed"), "{artifact}");
    assert!(artifact.contains("codex: not logged in"), "{artifact}");
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
    let artifact = std::fs::read_to_string(&spec.output).unwrap();
    assert!(
        artifact.contains("the provider wrote nothing"),
        "{artifact}"
    );
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
    let artifact = std::fs::read_to_string(&spec.output).unwrap();
    assert!(
        artifact.contains("the provider exited without writing a summary"),
        "{artifact}"
    );
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
    let artifact = std::fs::read_to_string(&spec.output).unwrap();
    assert!(artifact.contains(&error), "{artifact}");
}
