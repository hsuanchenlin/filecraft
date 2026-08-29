//! The AI multi-file summarizer: which files qualify, which provider runs,
//! where the summary lands, and the seam the background job runs behind.
//!
//! Every decision in this module is a pure function of data already in
//! memory - eligibility, the provider table, the output path, the prompt,
//! and what a finished child process *meant*. Only [`ProcessRunner`] ever
//! spawns anything, and [`App`](crate::app::App) reaches it through the
//! [`Runner`] trait, so the whole flow is testable without an AI CLI on
//! `$PATH` and without a network.
//!
//! This is **not** the [`agent`](crate::agent) seam, which stays disabled:
//! nothing here runs unless the user selects files, picks a provider, and
//! the summarizer is handed an explicit, finite list of paths.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Extensions the summarizer accepts. Anything else is refused in words
/// rather than handed to a model that cannot read it.
pub const SUMMARIZABLE: [&str; 4] = ["pdf", "md", "markdown", "txt"];

/// The extension list as the UI says it, so the screen and the rule can
/// never drift apart.
pub fn summarizable_note() -> String {
    let names: Vec<String> = SUMMARIZABLE.iter().map(|e| format!(".{e}")).collect();
    names.join(" ")
}

/// Whether `path` is a file the summarizer will accept. Case-insensitive:
/// `NOTES.MD` is Markdown.
pub fn is_summarizable(path: &Path) -> bool {
    path.extension()
        .map(|ext| ext.to_string_lossy().to_lowercase())
        .is_some_and(|ext| SUMMARIZABLE.contains(&ext.as_str()))
}

/// One AI CLI the summary can be run through.
///
/// The table is fixed and the argv is written out here rather than
/// assembled from user input: a provider is chosen by pressing a digit,
/// and nothing the user types ever becomes a program name or a flag.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Provider {
    /// `agy --dangerously-skip-permissions` - the default.
    Ag,
    /// `claude --dangerously-skip-permissions`
    Cc,
    /// `codex -p lavish -a on-request`
    Co,
    /// `grok --always-approve`
    Gk,
    /// `kimi --yolo`
    Ki,
}

impl Provider {
    /// Every provider, in menu order. The digit that selects one is its
    /// position here plus one, so the drawn menu and the keys agree.
    pub const ALL: [Provider; 5] = [
        Provider::Ag,
        Provider::Cc,
        Provider::Co,
        Provider::Gk,
        Provider::Ki,
    ];

    /// What Enter alone chooses.
    pub const DEFAULT: Provider = Provider::Ag;

    /// The two-letter code shown in the menu.
    pub fn code(self) -> &'static str {
        match self {
            Provider::Ag => "ag",
            Provider::Cc => "cc",
            Provider::Co => "co",
            Provider::Gk => "gk",
            Provider::Ki => "ki",
        }
    }

    /// The command line, program first. Never built from user input.
    pub fn argv(self) -> Vec<String> {
        let words: &[&str] = match self {
            Provider::Ag => &["agy", "--dangerously-skip-permissions"],
            Provider::Cc => &["claude", "--dangerously-skip-permissions"],
            Provider::Co => &["codex", "-p", "lavish", "-a", "on-request"],
            Provider::Gk => &["grok", "--always-approve"],
            Provider::Ki => &["kimi", "--yolo"],
        };
        words.iter().map(|w| (*w).to_string()).collect()
    }

    /// The program the child process will be, for status lines and errors.
    pub fn program(self) -> String {
        self.argv()
            .first()
            .cloned()
            .unwrap_or_else(|| self.code().to_string())
    }

    /// The command line as one readable string, for the menu.
    pub fn command_line(self) -> String {
        self.argv().join(" ")
    }

    /// The digit that selects this provider.
    pub fn digit(self) -> char {
        let index = Provider::ALL
            .iter()
            .position(|p| *p == self)
            .expect("every provider is in ALL");
        char::from(b'1' + index as u8)
    }

    /// The provider a digit selects, if any.
    pub fn from_digit(c: char) -> Option<Provider> {
        let index = c.to_digit(10)?.checked_sub(1)? as usize;
        Provider::ALL.get(index).copied()
    }
}

/// Resolve a keypress at the provider dialog. `None` is Enter, which
/// takes the default - the one choice that needs no reading.
pub fn resolve(digit: Option<char>) -> Option<Provider> {
    match digit {
        None => Some(Provider::DEFAULT),
        Some(c) => Provider::from_digit(c),
    }
}

/// The provider dialog as drawn: one row per provider, the default
/// marked in words so the choice never rests on position or color.
pub fn menu_lines() -> Vec<String> {
    Provider::ALL
        .iter()
        .map(|provider| {
            let mark = if *provider == Provider::DEFAULT {
                "  [Default]"
            } else {
                ""
            };
            format!(
                "[{}] {}: {}{mark}",
                provider.digit(),
                provider.code(),
                provider.command_line()
            )
        })
        .collect()
}

/// `YYYYmmdd-HHMMSS` in UTC - a name-safe stamp for the fallback output
/// file. Seconds resolution, because two summaries a minute apart must
/// not collide.
pub fn stamp(time: SystemTime) -> String {
    let Ok(since_epoch) = time.duration_since(UNIX_EPOCH) else {
        return "19700101-000000".to_string();
    };
    let secs = since_epoch.as_secs() as i64;
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let (hour, minute, second) = (rem / 3600, (rem % 3600) / 60, rem % 60);

    // Howard Hinnant's civil-from-days, the same algorithm `preview` uses
    // for its absolute stamps.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if month <= 2 { y + 1 } else { y };
    format!("{year:04}{month:02}{day:02}-{hour:02}{minute:02}{second:02}")
}

/// Where the summary lands: beside the **first** selected file, named
/// after it. A name already in use is never overwritten - Filecraft does
/// not overwrite anywhere else either - so the stamped form is the
/// fallback.
///
/// `taken` decides whether a candidate is already in use, so the
/// collision rule is testable without a filesystem.
pub fn output_path_with(first: &Path, stamp: &str, taken: &dyn Fn(&Path) -> bool) -> PathBuf {
    let dir = first.parent().unwrap_or(Path::new(".")).to_path_buf();
    let stem = first
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "files".to_string());
    let preferred = dir.join(format!("{stem}-summary.md"));
    if !taken(&preferred) {
        return preferred;
    }
    let stamped = dir.join(format!("{stem}-summary-{stamp}.md"));
    if !taken(&stamped) {
        return stamped;
    }
    // Third time is a different second or a directory nobody can write
    // to; either way the caller reports the real error.
    dir.join(format!("summary-{stamp}.md"))
}

/// [`output_path_with`] against the real filesystem.
pub fn output_path(first: &Path, stamp: &str) -> PathBuf {
    output_path_with(first, stamp, &|path| {
        std::fs::symlink_metadata(path).is_ok()
    })
}

/// Everything a summary run needs, decided before anything is spawned.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JobSpec {
    pub provider: Provider,
    /// The selected files, in the order they were selected. The first one
    /// decides where the summary is written.
    pub files: Vec<PathBuf>,
    /// The Markdown file the provider is asked to write.
    pub output: PathBuf,
    /// Working directory for the child - the first file's directory.
    pub cwd: PathBuf,
}

impl JobSpec {
    /// Build a spec for `files`, refusing an empty selection.
    pub fn new(provider: Provider, files: Vec<PathBuf>, output: PathBuf) -> Result<Self, String> {
        let Some(first) = files.first() else {
            return Err("no files selected".to_string());
        };
        let cwd = first.parent().unwrap_or(Path::new(".")).to_path_buf();
        Ok(JobSpec {
            provider,
            files,
            output,
            cwd,
        })
    }

    /// The instruction handed to the provider. It names absolute paths and
    /// exactly one file to write, so the run has a finite, stated scope.
    pub fn prompt(&self) -> String {
        let mut out = String::new();
        out.push_str("Read and summarize the following files.\n\n");
        for file in &self.files {
            out.push_str(&format!("- {}\n", file.display()));
        }
        out.push_str(&format!(
            "\nWrite one Markdown summary to this exact path:\n{}\n\n",
            self.output.display()
        ));
        out.push_str(
            "Give each file its own `##` heading with a few sentences, then \
             end with a `## Together` section covering what the set says as \
             a whole. Do not modify, move, or delete any of the source files \
             - write only the summary file named above. If you cannot write \
             a file, print the Markdown summary on stdout instead.\n",
        );
        out
    }

    /// The full command line: the provider's fixed argv with the prompt as
    /// the final positional argument.
    pub fn argv(&self) -> Vec<String> {
        let mut argv = self.provider.argv();
        argv.push(self.prompt());
        argv
    }

    /// The live status the screen shows while this job runs.
    pub fn status_line(&self) -> String {
        let count = self.files.len();
        let unit = if count == 1 { "file" } else { "files" };
        format!(
            "[AI: summarizing {count} {unit} with {}]",
            self.provider.program()
        )
    }
}

/// What a finished run meant.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// The summary is on disk at this path.
    Written(PathBuf),
    /// Nothing usable came back; this is what to tell the user.
    Failed(String),
}

/// How a finished child is turned into an [`Outcome`], as a pure rule.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Finish {
    /// The provider wrote the file it was asked to write.
    UseWrittenFile,
    /// It printed the summary instead; save stdout as the summary.
    WriteStdout,
    Failed(String),
}

/// The rule: the file the provider was asked to write wins, its stdout is
/// the fallback, and a run that produced neither is a failure named after
/// whatever the provider said.
pub fn finish(exit_ok: bool, output_written: bool, stdout: &str, stderr: &str) -> Finish {
    if output_written {
        return Finish::UseWrittenFile;
    }
    if !stdout.trim().is_empty() {
        return Finish::WriteStdout;
    }
    let detail = last_meaningful_line(stderr)
        .or_else(|| last_meaningful_line(stdout))
        .unwrap_or_else(|| {
            if exit_ok {
                "the provider wrote nothing".to_string()
            } else {
                "the provider exited without writing a summary".to_string()
            }
        });
    Finish::Failed(detail)
}

/// The last non-blank line of a stream, trimmed and bounded - enough to
/// name a failure on one message row without pasting a whole backtrace.
fn last_meaningful_line(text: &str) -> Option<String> {
    let line = text.lines().rev().find(|line| !line.trim().is_empty())?;
    let line = line.trim();
    let mut short: String = line.chars().take(160).collect();
    if line.chars().count() > 160 {
        short.push('…');
    }
    Some(short)
}

/// A summary run in flight.
///
/// Polled from the event loop, never waited on: the TUI keeps drawing and
/// answering keys for the whole run.
pub trait Job: Send {
    /// Non-blocking. `None` while the run is still going.
    fn poll(&mut self) -> Option<Outcome>;
    /// Stop the run. Called when the user confirms a quit.
    fn terminate(&mut self);
}

/// How a [`JobSpec`] becomes a running [`Job`]. The seam: the shipped
/// binary spawns a process, tests hand back a scripted job.
pub trait Runner: Send {
    fn start(&self, spec: &JobSpec) -> Result<Box<dyn Job>, String>;
    /// What this runner is, for the message log.
    fn description(&self) -> String {
        "a child process".to_string()
    }
}

/// The runner the binary ships: an actual child process.
#[derive(Debug, Default, Clone, Copy)]
pub struct ProcessRunner;

/// The runner Filecraft wires in.
pub fn process_runner() -> Box<dyn Runner> {
    Box::new(ProcessRunner)
}

impl Runner for ProcessRunner {
    fn start(&self, spec: &JobSpec) -> Result<Box<dyn Job>, String> {
        let argv = spec.argv();
        let mut child = Command::new(&argv[0])
            .args(&argv[1..])
            .current_dir(&spec.cwd)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| format!("could not run '{}': {e}", argv[0]))?;

        let stdout = child.stdout.take();
        let stderr = child.stderr.take();
        let shared = Arc::new(Mutex::new(Some(child)));
        let (tx, rx) = mpsc::channel();

        let waiter = Arc::clone(&shared);
        let output = spec.output.clone();
        std::thread::spawn(move || {
            let out_reader = std::thread::spawn(move || drain(stdout));
            let err_reader = std::thread::spawn(move || drain(stderr));

            // Polled rather than waited on, so `terminate` can take the
            // same lock and kill the child mid-run.
            let exit_ok = loop {
                let status = {
                    let mut guard = waiter.lock().expect("job mutex");
                    match guard.as_mut() {
                        Some(child) => child.try_wait(),
                        // `terminate` took the child; the run is over.
                        None => break false,
                    }
                };
                match status {
                    Ok(Some(status)) => break status.success(),
                    Ok(None) => std::thread::sleep(Duration::from_millis(100)),
                    Err(_) => break false,
                }
            };

            let stdout_text = out_reader.join().unwrap_or_default();
            let stderr_text = err_reader.join().unwrap_or_default();
            let written = non_empty_file(&output);
            let outcome = match finish(exit_ok, written, &stdout_text, &stderr_text) {
                Finish::UseWrittenFile => Outcome::Written(output),
                Finish::WriteStdout => match std::fs::write(&output, stdout_text.as_bytes()) {
                    Ok(()) => Outcome::Written(output),
                    Err(e) => Outcome::Failed(format!("could not write {}: {e}", output.display())),
                },
                Finish::Failed(reason) => Outcome::Failed(reason),
            };
            let _ = tx.send(outcome);
        });

        Ok(Box::new(ProcessJob {
            child: shared,
            rx,
            done: None,
        }))
    }

    fn description(&self) -> String {
        "a child process".to_string()
    }
}

/// Read a captured pipe to the end, losing nothing to encoding.
fn drain(pipe: Option<impl Read>) -> String {
    let Some(mut pipe) = pipe else {
        return String::new();
    };
    let mut buf = Vec::new();
    let _ = pipe.read_to_end(&mut buf);
    String::from_utf8_lossy(&buf).into_owned()
}

/// Whether the provider actually produced a summary at `path`. An empty
/// file is not a summary.
fn non_empty_file(path: &Path) -> bool {
    std::fs::metadata(path)
        .map(|m| m.len() > 0)
        .unwrap_or(false)
}

struct ProcessJob {
    child: Arc<Mutex<Option<Child>>>,
    rx: Receiver<Outcome>,
    done: Option<Outcome>,
}

impl Job for ProcessJob {
    fn poll(&mut self) -> Option<Outcome> {
        if let Some(done) = self.done.take() {
            return Some(done);
        }
        match self.rx.try_recv() {
            Ok(outcome) => Some(outcome),
            Err(TryRecvError::Empty) => None,
            Err(TryRecvError::Disconnected) => Some(Outcome::Failed(
                "the summary run ended without a result".to_string(),
            )),
        }
    }

    fn terminate(&mut self) {
        let mut guard = self.child.lock().expect("job mutex");
        if let Some(mut child) = guard.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(files: &[&str]) -> JobSpec {
        let files: Vec<PathBuf> = files.iter().map(PathBuf::from).collect();
        let output = output_path_with(&files[0], "20260829-101500", &|_| false);
        JobSpec::new(Provider::DEFAULT, files, output).unwrap()
    }

    #[test]
    fn only_pdf_and_text_documents_qualify() {
        for good in ["a.pdf", "a.md", "a.markdown", "a.txt", "A.PDF", "b.Md"] {
            assert!(is_summarizable(Path::new(good)), "{good} should qualify");
        }
        for bad in ["a.png", "a.rs", "a", "a.mdx", "a.txt.zip", ".md"] {
            assert!(!is_summarizable(Path::new(bad)), "{bad} must not qualify");
        }
    }

    #[test]
    fn provider_table_matches_the_menu_keys() {
        assert_eq!(Provider::DEFAULT, Provider::Ag);
        let expected = [
            ("ag", "agy --dangerously-skip-permissions", '1'),
            ("cc", "claude --dangerously-skip-permissions", '2'),
            ("co", "codex -p lavish -a on-request", '3'),
            ("gk", "grok --always-approve", '4'),
            ("ki", "kimi --yolo", '5'),
        ];
        for (provider, (code, line, digit)) in Provider::ALL.iter().zip(expected) {
            assert_eq!(provider.code(), code);
            assert_eq!(provider.command_line(), line);
            assert_eq!(provider.digit(), digit);
            assert_eq!(Provider::from_digit(digit), Some(*provider));
        }
    }

    #[test]
    fn enter_alone_resolves_to_the_default_provider() {
        assert_eq!(resolve(None), Some(Provider::Ag));
        assert_eq!(resolve(Some('1')), Some(Provider::Ag));
        assert_eq!(resolve(Some('5')), Some(Provider::Ki));
        assert_eq!(resolve(Some('0')), None);
        assert_eq!(resolve(Some('6')), None);
        assert_eq!(resolve(Some('x')), None);
    }

    #[test]
    fn the_menu_marks_exactly_one_default() {
        let lines = menu_lines();
        assert_eq!(lines.len(), 5);
        assert_eq!(
            lines[0],
            "[1] ag: agy --dangerously-skip-permissions  [Default]"
        );
        assert_eq!(
            lines.iter().filter(|l| l.contains("[Default]")).count(),
            1,
            "exactly one row may be marked the default"
        );
    }

    #[test]
    fn summary_lands_beside_the_first_selected_file() {
        let first = PathBuf::from("/docs/a/report.pdf");
        let path = output_path_with(&first, "20260829-101500", &|_| false);
        assert_eq!(path, PathBuf::from("/docs/a/report-summary.md"));
    }

    #[test]
    fn an_existing_summary_is_never_overwritten() {
        let first = PathBuf::from("/docs/report.pdf");
        let taken = |p: &Path| p == Path::new("/docs/report-summary.md");
        assert_eq!(
            output_path_with(&first, "20260829-101500", &taken),
            PathBuf::from("/docs/report-summary-20260829-101500.md")
        );
        let both_taken = |p: &Path| p.to_string_lossy().starts_with("/docs/report-summary");
        assert_eq!(
            output_path_with(&first, "20260829-101500", &both_taken),
            PathBuf::from("/docs/summary-20260829-101500.md")
        );
    }

    #[test]
    fn a_nameless_first_file_still_gets_an_output_path() {
        let path = output_path_with(Path::new("/docs/.md"), "20260829-101500", &|_| false);
        assert_eq!(path, PathBuf::from("/docs/.md-summary.md"));
    }

    #[test]
    fn stamp_is_utc_and_name_safe() {
        assert_eq!(stamp(UNIX_EPOCH), "19700101-000000");
        let t = UNIX_EPOCH + Duration::from_secs(946_684_800 + 3661);
        assert_eq!(stamp(t), "20000101-010101");
        assert!(!stamp(SystemTime::now()).contains(std::path::MAIN_SEPARATOR));
    }

    #[test]
    fn a_spec_needs_at_least_one_file() {
        assert_eq!(
            JobSpec::new(Provider::Ag, vec![], PathBuf::from("/x/s.md")),
            Err("no files selected".to_string())
        );
    }

    #[test]
    fn the_child_runs_in_the_first_files_directory() {
        let spec = spec(&["/docs/a/one.pdf", "/other/two.md"]);
        assert_eq!(spec.cwd, PathBuf::from("/docs/a"));
        assert_eq!(spec.output, PathBuf::from("/docs/a/one-summary.md"));
    }

    #[test]
    fn the_prompt_names_every_file_and_exactly_one_output() {
        let spec = spec(&["/docs/one.pdf", "/docs/two.md", "/elsewhere/three.txt"]);
        let prompt = spec.prompt();
        for file in ["/docs/one.pdf", "/docs/two.md", "/elsewhere/three.txt"] {
            assert!(prompt.contains(file), "prompt must name {file}");
        }
        assert!(prompt.contains("/docs/one-summary.md"));
        assert!(prompt.contains("Do not modify"));
    }

    #[test]
    fn argv_is_the_fixed_provider_line_plus_the_prompt() {
        let spec = spec(&["/docs/one.pdf"]);
        let argv = spec.argv();
        assert_eq!(argv[0], "agy");
        assert_eq!(argv[1], "--dangerously-skip-permissions");
        assert_eq!(argv.len(), 3);
        assert_eq!(argv[2], spec.prompt());
    }

    #[test]
    fn the_status_line_counts_the_files_and_names_the_program() {
        assert_eq!(
            spec(&["/docs/one.pdf", "/docs/two.md", "/docs/three.txt"]).status_line(),
            "[AI: summarizing 3 files with agy]"
        );
        assert_eq!(
            spec(&["/docs/one.pdf"]).status_line(),
            "[AI: summarizing 1 file with agy]"
        );
    }

    #[test]
    fn a_written_file_wins_over_everything_else() {
        assert_eq!(finish(true, true, "", ""), Finish::UseWrittenFile);
        assert_eq!(
            finish(false, true, "chatter", "boom"),
            Finish::UseWrittenFile
        );
    }

    #[test]
    fn stdout_is_the_fallback_summary() {
        assert_eq!(finish(true, false, "# Summary\n", ""), Finish::WriteStdout);
    }

    #[test]
    fn a_run_with_nothing_to_show_reports_what_the_provider_said() {
        assert_eq!(
            finish(false, false, "", "agy: not logged in\n"),
            Finish::Failed("agy: not logged in".to_string())
        );
        assert_eq!(
            finish(false, false, "   \n", "  \n"),
            Finish::Failed("the provider exited without writing a summary".to_string())
        );
        assert_eq!(
            finish(true, false, "", ""),
            Finish::Failed("the provider wrote nothing".to_string())
        );
    }

    #[test]
    fn a_long_failure_line_is_bounded() {
        let noisy = "x".repeat(500);
        let Finish::Failed(detail) = finish(false, false, "", &noisy) else {
            panic!("expected a failure");
        };
        assert_eq!(detail.chars().count(), 161);
        assert!(detail.ends_with('…'));
    }

    #[test]
    fn the_extension_note_lists_every_accepted_extension() {
        let note = summarizable_note();
        for ext in SUMMARIZABLE {
            assert!(note.contains(&format!(".{ext}")), "note must list .{ext}");
        }
    }
}
