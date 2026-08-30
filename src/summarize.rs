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

use std::io::{Read, Seek, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::i18n::Lang;
use crate::session;
use crate::stream;

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
///
/// Each line is the CLI's **non-interactive** form. That is not a detail:
/// none of these tools take a prompt as a bare trailing word, and a
/// summary run has no terminal to answer questions on. See
/// [`Provider::prompt_flag`] for how the prompt is actually handed over.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Provider {
    /// `agy --dangerously-skip-permissions -p <prompt>` - the default.
    Ag,
    /// `claude --dangerously-skip-permissions -p <prompt>`
    Cc,
    /// `codex exec -s workspace-write --skip-git-repo-check <prompt>`
    Co,
    /// `grok --always-approve -p <prompt>`
    Gk,
    /// `kimi -p <prompt>`
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

    /// The fixed command line - program first, flags after, and *no*
    /// prompt. Never built from user input.
    ///
    /// Each line is the form that runs headless and answers no questions:
    ///
    /// - `agy` / `claude`: their own skip-permissions flag, then `--print`.
    /// - `codex`: the `exec` subcommand, which is the non-interactive
    ///   one, then the two grants the run needs spelled out on the line
    ///   itself. `-s workspace-write` is the write grant: `codex exec`
    ///   takes its sandbox from the user's `config.toml` when the flag is
    ///   absent, so a summary that is only ever written *beside its
    ///   sources* must ask for it here rather than hope for it.
    ///   `--skip-git-repo-check` is required because a folder of
    ///   documents is usually not a git repository. A named
    ///   `-p`/`--profile` would carry both, and did, but it names a
    ///   `$CODEX_HOME/<name>.config.toml` that exists only on the machine
    ///   it was written on: every other user would get a config error
    ///   instead of a summary.
    /// - `grok`: `--always-approve`, then `--single`.
    /// - `kimi`: nothing but the program. `kimi` *refuses* to combine
    ///   `--yolo` or `--auto` with `--prompt` ("Cannot combine --prompt
    ///   with --yolo"); its prompt mode carries its own permissions.
    pub fn base_argv(self) -> Vec<String> {
        let words: &[&str] = match self {
            Provider::Ag => &["agy", "--dangerously-skip-permissions"],
            Provider::Cc => &["claude", "--dangerously-skip-permissions"],
            Provider::Co => &[
                "codex",
                "exec",
                "-s",
                "workspace-write",
                "--skip-git-repo-check",
            ],
            Provider::Gk => &["grok", "--always-approve"],
            Provider::Ki => &["kimi"],
        };
        words.iter().map(|w| (*w).to_string()).collect()
    }

    /// The flag that hands this provider its prompt, or `None` where the
    /// prompt is a plain positional argument.
    ///
    /// This is the whole point of the table. A prompt appended as a bare
    /// trailing word is not "the prompt" to any of these CLIs - `agy`
    /// rejects it outright ("Prompts are read only from -p/--print,
    /// -i/--prompt-interactive, or stdin"), and the rest would open an
    /// interactive session a background job can never answer. The one
    /// exception is `codex exec`, whose prompt genuinely is positional.
    pub fn prompt_flag(self) -> Option<&'static str> {
        match self {
            // `-p` is `--print`: run one prompt and print the response.
            Provider::Ag | Provider::Cc => Some("-p"),
            // `codex exec [OPTIONS] [PROMPT]` - positional. Its own
            // `-p` is `--profile`, so a prompt flag here would name a
            // config file rather than hand over the prompt.
            Provider::Co => None,
            // `-p` is `--single`: single-turn, print to stdout, exit.
            Provider::Gk => Some("-p"),
            // `-p` is `--prompt`: run one prompt non-interactively.
            Provider::Ki => Some("-p"),
        }
    }

    /// The command line that actually runs: the fixed line, the prompt
    /// flag where the CLI needs one, then `prompt` as one single argument.
    ///
    /// The prompt is never split, quoted, or interpolated into a string -
    /// it is one `argv` entry handed to `execvp`, and no shell sees it.
    pub fn argv_with_prompt(self, prompt: &str) -> Vec<String> {
        let mut argv = self.base_argv();
        argv.extend(self.prompt_flag().map(str::to_string));
        argv.push(prompt.to_string());
        argv
    }

    /// The fixed line that **reopens** a finished run's session in the
    /// provider's own CLI, without the identifier.
    ///
    /// A second table, and a second set of flags that are not guessable
    /// from the first: `agy` resumes with `--conversation` and has no
    /// `--resume` at all, `codex` resumes through a subcommand rather
    /// than a flag, and `kimi` calls it `--session`. Every line here was
    /// read off the installed CLI's own `--help`, for the same reason
    /// [`Provider::prompt_flag`] was - a plausible-looking flag that the
    /// tool does not have is worse than no advice, because it is advice
    /// the user will follow.
    ///
    /// Filecraft never runs these. They are printed - in the log
    /// viewer's header and in the summary's footer - for the user to run
    /// themselves, outside Filecraft.
    pub fn resume_words(self) -> &'static [&'static str] {
        match self {
            // `--conversation <id>`: resume a previous conversation by ID.
            Provider::Ag => &["agy", "--conversation"],
            // `-r, --resume <session-id>`.
            Provider::Cc => &["claude", "--resume"],
            // A subcommand, not a flag: `codex resume <id>`.
            Provider::Co => &["codex", "resume"],
            // `-r, --resume <SESSION_ID_OR_TITLE>`.
            Provider::Gk => &["grok", "--resume"],
            // `-S, --session [id]`.
            Provider::Ki => &["kimi", "--session"],
        }
    }

    /// The reopen command for one session, as one readable line.
    pub fn resume_command(self, id: &str) -> String {
        format!("{} {id}", self.resume_words().join(" "))
    }

    /// The program the child process will be, for status lines and errors.
    pub fn program(self) -> String {
        self.base_argv()
            .first()
            .cloned()
            .unwrap_or_else(|| self.code().to_string())
    }

    /// The fixed part of the command line as one readable string, for the
    /// menu. The prompt flag and the prompt are appended at run time and
    /// are not shown: the menu answers "which tool runs", and a wrapped
    /// row would answer it worse.
    pub fn command_line(self) -> String {
        self.base_argv().join(" ")
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

/// How far into a menu row its command line starts - `[3] co: ` is
/// eight columns. A row too wide for the dialog is continued under its
/// command line rather than beside the digits, so the continuation can
/// never be read as a sixth provider.
pub const MENU_INDENT: usize = 8;

/// The provider dialog as drawn: one row per provider, the default
/// marked in words so the choice never rests on position or color.
pub fn menu_lines(lang: Lang) -> Vec<String> {
    Provider::ALL
        .iter()
        .map(|provider| {
            let mark = if *provider == Provider::DEFAULT {
                lang.provider_default_mark()
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
    let generic = dir.join(format!("summary-{stamp}.md"));
    if !taken(&generic) {
        return generic;
    }
    for suffix in 2..=usize::MAX {
        let candidate = dir.join(format!("{stem}-summary-{stamp}-{suffix}.md"));
        if !taken(&candidate) {
            return candidate;
        }
    }
    unreachable!("every possible summary filename is already in use")
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
    ///
    /// `None` rather than an error string: there is exactly one way to
    /// fail here, and the caller already has a phrase for it
    /// ([`crate::i18n::Lang::summarize_no_files`]) in the screen's own
    /// language.
    pub fn new(provider: Provider, files: Vec<PathBuf>, output: PathBuf) -> Option<Self> {
        let first = files.first()?;
        let cwd = first.parent().unwrap_or(Path::new(".")).to_path_buf();
        Some(JobSpec {
            provider,
            files,
            output,
            cwd,
        })
    }

    /// The instruction handed to the provider. It names absolute paths and
    /// exactly one file to write, so the run has a finite, stated scope.
    ///
    /// Two things it insists on, both learned from providers that got them
    /// wrong: **read** the listed files rather than guessing from their
    /// names, and **actually create** the output file rather than reporting
    /// that it did. `agy`'s own file-writing tool is restricted to its
    /// artifact directory and it will happily answer "done" having written
    /// nothing, so the prompt names the fallback out loud.
    pub fn prompt(&self) -> String {
        let mut out = String::new();
        out.push_str("Read and summarize the following files.\n\n");
        for file in &self.files {
            out.push_str(&format!("- {}\n", file.display()));
        }
        out.push_str(
            "\nOpen and read every file listed above before you write \
             anything. Summarize what they actually say, not what their \
             names suggest.\n",
        );
        out.push_str(&format!(
            "\nWrite one Markdown summary to this exact absolute path:\n{}\n",
            self.output.display()
        ));
        out.push_str(
            "\nThat path may already exist as an empty placeholder; \
             overwrite it. The file must exist on disk with the summary in \
             it when you finish - if your file-writing tool refuses that \
             path, write it with a shell command instead. Reporting that \
             the summary is written is not the same as writing it.\n\n",
        );
        out.push_str(
            "Give each file its own `##` heading with a few sentences, then \
             end with a `## Together` section covering what the set says as \
             a whole. Do not modify, move, or delete any of the source files \
             - write only the summary file named above. If you cannot write \
             a file at all, print the Markdown summary on stdout instead.\n",
        );
        out
    }

    /// The full command line: the provider's fixed line, its prompt flag,
    /// and the prompt as one argument. See [`Provider::prompt_flag`].
    pub fn argv(&self) -> Vec<String> {
        self.provider.argv_with_prompt(&self.prompt())
    }

    /// The live status the screen shows while this job runs.
    pub fn status_line(&self, lang: Lang) -> String {
        lang.job_status(self.files.len(), &self.provider.program())
    }
}

/// Why a run produced no summary.
///
/// A value, not a sentence, for the same reason [`crate::fsops::FsError`]
/// is one - but here the split matters twice over, because the same
/// failure is said in two places for two audiences. On screen it is a
/// message in the user's language ([`Failure::message`]); in the
/// Markdown file the run reserved it is [`Failure`]'s `Display`, which
/// is always English, because that file outlives the session and is read
/// by whoever the summary is shared with.
///
/// [`Failure::Provider`] is the exception that proves the split: it
/// carries the provider's own last line, which is evidence and is never
/// translated in either place.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Failure {
    /// What the provider itself last said.
    Provider(String),
    /// It exited cleanly having produced nothing at all.
    NoOutput,
    /// It exited without writing a summary.
    NoSummary,
    /// It was terminated at the quit prompt.
    Stopped,
    /// The run went away without saying how it ended.
    NoResult,
    /// The output file could not be claimed before the run started.
    Reserve { path: PathBuf, detail: String },
    /// The provider could not be started at all.
    Spawn { program: String, detail: String },
    /// The summary itself could not be written.
    Write { path: PathBuf, detail: String },
    /// A failure that could not even be written down.
    Unrecorded {
        reason: Box<Failure>,
        path: Option<PathBuf>,
        detail: String,
    },
}

impl Failure {
    /// Why the run failed, in `lang`.
    pub fn message(&self, lang: Lang) -> String {
        match self {
            Failure::Provider(detail) => detail.clone(),
            Failure::NoOutput => lang.provider_wrote_nothing().to_string(),
            Failure::NoSummary => lang.provider_wrote_no_summary().to_string(),
            Failure::Stopped => lang.run_stopped().to_string(),
            Failure::NoResult => lang.run_without_result().to_string(),
            Failure::Reserve { path, detail } => {
                lang.could_not_reserve(&path.display().to_string(), detail)
            }
            Failure::Spawn { program, detail } => lang.could_not_run(program, detail),
            Failure::Write { path, detail } => {
                lang.could_not_write(&path.display().to_string(), detail)
            }
            Failure::Unrecorded {
                reason,
                path,
                detail,
            } => lang.could_not_record(
                &reason.message(lang),
                path.as_ref().map(|p| p.display().to_string()).as_deref(),
                detail,
            ),
        }
    }

    /// This failure, wrapped in the one that stopped it being recorded.
    fn unrecorded(self, path: Option<&Path>, detail: String) -> Failure {
        Failure::Unrecorded {
            reason: Box::new(self),
            path: path.map(Path::to_path_buf),
            detail,
        }
    }
}

impl std::fmt::Display for Failure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message(Lang::En))
    }
}

/// What a finished run meant.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// The summary is on disk at this path.
    Written(PathBuf),
    /// Nothing usable came back; this is what to tell the user.
    Failed(Failure),
}

/// How a finished child is turned into an [`Outcome`], as a pure rule.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Finish {
    /// The provider wrote the file it was asked to write.
    UseWrittenFile,
    /// It printed the summary instead; save stdout as the summary.
    WriteStdout,
    Failed(Failure),
}

/// The rule: the file the provider was asked to write wins, its stdout is
/// the fallback, and a run that produced neither is a failure named after
/// whatever the provider said.
pub fn finish(exit_ok: bool, output_written: bool, stdout: &str, stderr: &str) -> Finish {
    if output_written {
        return Finish::UseWrittenFile;
    }
    if exit_ok && !stdout.trim().is_empty() {
        return Finish::WriteStdout;
    }
    let failure = last_meaningful_line(stderr)
        .or_else(|| last_meaningful_line(stdout))
        .map(Failure::Provider)
        .unwrap_or(if exit_ok {
            Failure::NoOutput
        } else {
            Failure::NoSummary
        });
    Finish::Failed(failure)
}

/// The Markdown a failed run leaves in the file it reserved.
///
/// Always English: the file outlives the session that wrote it, and it
/// is read by whoever the summary was going to be shared with rather
/// than only by the person at the terminal.
pub fn failure_note(reason: &Failure) -> String {
    format!("# Summary failed\n\n{reason}\n")
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
///
/// `stream` is the log the run fills as it goes. The caller owns it and
/// keeps it after the job is dropped, which is what lets the log viewer
/// still be opened over a run that has already finished; a runner that
/// has nothing to say into it simply never appends.
pub trait Runner: Send {
    fn start(&self, spec: &JobSpec, stream: &stream::Handle) -> Result<Box<dyn Job>, Failure>;
}

/// The runner the binary ships: an actual child process.
#[derive(Debug, Default, Clone, Copy)]
pub struct ProcessRunner;

/// The runner Filecraft wires in.
pub fn process_runner() -> Box<dyn Runner> {
    Box::new(ProcessRunner)
}

impl Runner for ProcessRunner {
    fn start(&self, spec: &JobSpec, stream: &stream::Handle) -> Result<Box<dyn Job>, Failure> {
        let argv = spec.argv();
        let mut reservation = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&spec.output)
            .map_err(|e| Failure::Reserve {
                path: spec.output.clone(),
                detail: e.to_string(),
            })?;
        let child = Command::new(&argv[0])
            .args(&argv[1..])
            .current_dir(&spec.cwd)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn();
        let mut child = match child {
            Ok(child) => child,
            Err(e) => {
                let reason = Failure::Spawn {
                    program: argv[0].clone(),
                    detail: e.to_string(),
                };
                // The log gets the English rendering, because the log is
                // a transcript of the run beside the provider's own output.
                stream.append(stream::Origin::Err, &format!("{reason}\n"));
                stream.end();
                write_reserved(&mut reservation, failure_note(&reason).as_bytes()).map_err(
                    |write_error| reason.clone().unrecorded(None, write_error.to_string()),
                )?;
                return Err(reason);
            }
        };

        let stdout = child.stdout.take();
        let stderr = child.stderr.take();
        let shared = Arc::new(Mutex::new(Some(child)));
        // Shared with the worker rather than moved into it: the quit path
        // has to be able to fill the same reservation when the worker is
        // still blocked on a pipe it no longer owns.
        let reservation = Arc::new(Mutex::new(reservation));
        let (tx, rx) = mpsc::channel();

        // Shared for the same reason the reservation is: the quit path
        // and the worker can both reach the end of this one run.
        let signed = Arc::new(AtomicBool::new(false));
        let waiter = Arc::clone(&shared);
        let writer = Arc::clone(&reservation);
        let signer = Arc::clone(&signed);
        let output = spec.output.clone();
        let provider = spec.provider;
        let live = stream.clone();
        let out_live = stream.clone();
        let err_live = stream.clone();
        let worker = std::thread::spawn(move || {
            let out_reader =
                std::thread::spawn(move || drain(stdout, stream::Origin::Out, &out_live));
            let err_reader =
                std::thread::spawn(move || drain(stderr, stream::Origin::Err, &err_live));

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
            // Both pipes are closed: whatever the provider said, it has
            // said. Ending the log here is what commits an unterminated
            // last line and settles the session the footer names.
            live.end();
            let written = non_empty_file(&output);
            let outcome = match finish(exit_ok, written, &stdout_text, &stderr_text) {
                Finish::UseWrittenFile => Outcome::Written(output.clone()),
                Finish::WriteStdout => {
                    match write_reserved(&mut hold(&writer), stdout_text.as_bytes()) {
                        Ok(()) => Outcome::Written(output.clone()),
                        Err(e) => Outcome::Failed(Failure::Write {
                            path: output.clone(),
                            detail: e.to_string(),
                        }),
                    }
                }
                Finish::Failed(reason) => {
                    match write_reserved(&mut hold(&writer), failure_note(&reason).as_bytes()) {
                        Ok(()) => Outcome::Failed(reason),
                        Err(e) => Outcome::Failed(reason.unrecorded(Some(&output), e.to_string())),
                    }
                }
            };
            sign_once(&signer, &output, provider, live.session().as_deref());
            let _ = tx.send(outcome);
        });

        Ok(Box::new(ProcessJob {
            child: shared,
            reservation,
            signed,
            output: spec.output.clone(),
            provider: spec.provider,
            live: stream.clone(),
            rx,
            done: None,
            worker: Some(worker),
        }))
    }
}

/// The reservation, borrowed for as long as one write takes. A lock
/// poisoned by a panicking writer still hands back the file: the quit
/// path is the last chance to say what happened, and a panic there would
/// leave the terminal in raw mode.
fn hold(reservation: &Mutex<std::fs::File>) -> std::sync::MutexGuard<'_, std::fs::File> {
    reservation
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn write_reserved(file: &mut std::fs::File, content: &[u8]) -> std::io::Result<()> {
    file.set_len(0)?;
    file.rewind()?;
    file.write_all(content)?;
    file.flush()
}

/// Read a captured pipe to the end, losing nothing to encoding, and hand
/// every chunk to the live log on the way past.
///
/// Read in chunks rather than to the end, because the log is watched
/// while the run is going: `read_to_end` would show the whole run at
/// once, the moment it no longer mattered. The full text is still
/// returned, because [`finish`] needs the whole of stdout - the log
/// forgets its oldest lines and a summary written from it would be
/// missing its beginning.
fn drain(pipe: Option<impl Read>, origin: stream::Origin, live: &stream::Handle) -> String {
    let Some(mut pipe) = pipe else {
        return String::new();
    };
    let mut text = String::new();
    let mut buf = [0u8; 8 * 1024];
    // A chunk boundary can split a multi-byte character; the tail waits
    // for the rest of itself rather than becoming a replacement char.
    // Everything else `stream::decode` consumes, so a byte that is not
    // UTF-8 at all cannot stall the run's whole remaining output.
    let mut tail: Vec<u8> = Vec::new();
    loop {
        match pipe.read(&mut buf) {
            Ok(0) => break,
            Ok(read) => {
                tail.extend_from_slice(&buf[..read]);
                let chunk = stream::decode(&mut tail);
                text.push_str(&chunk);
                live.append(origin, &chunk);
            }
            Err(_) => break,
        }
    }
    if !tail.is_empty() {
        let rest = String::from_utf8_lossy(&tail).into_owned();
        text.push_str(&rest);
        live.append(origin, &rest);
    }
    text
}

/// Append the run's provenance to the Markdown it produced: which
/// provider wrote it, which session it belongs to, and the command that
/// reopens that session in the provider's own CLI.
///
/// Every ending gets one - a written summary, a summary saved from
/// stdout, and a failure note - because the session is exactly what a
/// failed run is worth reopening for. A run whose provider never
/// announced a session still says which provider it was.
///
/// Exactly one, though, and `signed` is what holds that: two threads can
/// reach the end of the same run. Past [`TERMINATE_GRACE`] the UI thread
/// finishes the job itself in [`ProcessJob::record_stopped`], while the
/// worker is merely detached rather than stopped - when the drain threads
/// it is blocked on finally unblock, it re-evaluates the very same ending
/// and arrives here too. Whichever gets here first signs; the other finds
/// the run already signed and leaves the file alone.
///
/// Best effort by design: the summary is the point, and a footer that
/// could not be appended must never turn a finished run into a failure.
/// A footer that did not land leaves the run unsigned, so the thread
/// arriving second still has its turn.
fn sign_once(signed: &AtomicBool, output: &Path, provider: Provider, session: Option<&str>) {
    if signed.swap(true, Ordering::SeqCst) {
        return;
    }
    let resume = session.map(|id| provider.resume_command(id));
    let footer = session::footer(&provider.program(), session, resume.as_deref());
    if append_footer(output, &footer).is_err() {
        signed.store(false, Ordering::SeqCst);
    }
}

/// Append `footer` as its own paragraph, whatever the file ended with.
fn append_footer(path: &Path, footer: &str) -> std::io::Result<()> {
    let mut file = std::fs::OpenOptions::new()
        .read(true)
        .append(true)
        .open(path)?;
    let mut lead = String::new();
    let len = file.metadata()?.len();
    if len > 0 {
        let mut last = [0u8; 1];
        file.seek(std::io::SeekFrom::End(-1))?;
        file.read_exact(&mut last)?;
        if last[0] != b'\n' {
            lead.push('\n');
        }
        lead.push('\n');
    }
    // Opened for append: the write lands at the end whatever the read
    // above left the cursor on.
    file.write_all(format!("{lead}{footer}\n").as_bytes())?;
    file.flush()
}

/// Whether the provider actually produced a summary at `path`. An empty
/// file is not a summary.
fn non_empty_file(path: &Path) -> bool {
    std::fs::metadata(path)
        .map(|m| m.len() > 0)
        .unwrap_or(false)
}

/// How long [`ProcessJob::terminate`] waits for a killed run to wind
/// itself up before it finishes the job off itself.
///
/// Killing the child does not close its pipes: anything it spawned
/// inherited them, and the drain threads block until the *last* writer
/// closes. `terminate` runs on the UI thread from the quit prompt, so
/// that wait has to be bounded or a grandchild the summarizer never knew
/// about can hold the terminal in raw mode for as long as it likes.
const TERMINATE_GRACE: Duration = Duration::from_millis(500);

/// What a run stopped at the quit prompt is called in its own summary
/// file, when it did not wind up inside [`TERMINATE_GRACE`]. The English
/// rendering of [`Failure::Stopped`], because that is what goes into the
/// file.
pub const STOPPED_REASON: &str = "the summary run was stopped before it could finish";

struct ProcessJob {
    child: Arc<Mutex<Option<Child>>>,
    /// Shared with the worker. Whichever of the two finishes the run
    /// writes through it, so the reserved file is never left empty.
    reservation: Arc<Mutex<std::fs::File>>,
    /// Whether the run's footer has been appended. Shared with the worker
    /// so the run is signed once however it ends - see [`sign_once`].
    signed: Arc<AtomicBool>,
    output: PathBuf,
    provider: Provider,
    /// The live log, so a run stopped at the quit prompt still signs its
    /// note with the session it opened.
    live: stream::Handle,
    rx: Receiver<Outcome>,
    done: Option<Outcome>,
    worker: Option<std::thread::JoinHandle<()>>,
}

impl ProcessJob {
    /// The run is over as far as the app is concerned, but the worker is
    /// still blocked on a pipe the killed child no longer owns. Finish
    /// the job here: the app drops it the moment `terminate` returns, and
    /// a detached worker dies with the process.
    ///
    /// Through [`finish`], because the precedence is the same one every
    /// other ending obeys - the file the provider was asked to write
    /// wins. A provider can have written its summary and still leave the
    /// drain blocked, and the note would truncate what it wrote.
    fn record_stopped(&self) -> Outcome {
        let sign_it = || {
            sign_once(
                &self.signed,
                &self.output,
                self.provider,
                self.live.session().as_deref(),
            )
        };
        let reason = match finish(false, non_empty_file(&self.output), "", "") {
            Finish::UseWrittenFile => {
                sign_it();
                return Outcome::Written(self.output.clone());
            }
            Finish::WriteStdout | Finish::Failed(_) => Failure::Stopped,
        };
        let written = write_reserved(
            &mut hold(&self.reservation),
            failure_note(&reason).as_bytes(),
        );
        sign_it();
        match written {
            Ok(()) => Outcome::Failed(reason),
            Err(e) => Outcome::Failed(reason.unrecorded(Some(&self.output), e.to_string())),
        }
    }
}

impl Job for ProcessJob {
    fn poll(&mut self) -> Option<Outcome> {
        if let Some(done) = self.done.take() {
            return Some(done);
        }
        match self.rx.try_recv() {
            Ok(outcome) => Some(outcome),
            Err(TryRecvError::Empty) => None,
            Err(TryRecvError::Disconnected) => Some(Outcome::Failed(Failure::NoResult)),
        }
    }

    fn terminate(&mut self) {
        {
            let mut guard = self.child.lock().expect("job mutex");
            if let Some(mut child) = guard.take() {
                let _ = child.kill();
                let _ = child.wait();
            }
        }
        self.live.append(
            stream::Origin::Err,
            &format!("filecraft: {STOPPED_REASON}\n"),
        );
        // Bounded on purpose: see `TERMINATE_GRACE`. Past the grace the
        // handle is dropped rather than joined, which detaches the worker
        // and its two drain threads; the reservation they share is filled
        // here first, so the run still ends in a note on disk and a
        // failure the job reports.
        if let Some(worker) = self.worker.take() {
            let deadline = Instant::now() + TERMINATE_GRACE;
            while !worker.is_finished() {
                if Instant::now() >= deadline {
                    // The drain threads are still blocked on a pipe a
                    // grandchild holds; nothing more is coming into the
                    // log that this app will ever see.
                    self.live.end();
                    let stopped = self.record_stopped();
                    self.done = Some(stopped);
                    return;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            let _ = worker.join();
        }
        self.live.end();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(files: &[&str]) -> JobSpec {
        let files: Vec<PathBuf> = files.iter().map(PathBuf::from).collect();
        let output = output_path_with(&files[0], "20260829-101500", &|_| false);
        JobSpec::new(Provider::DEFAULT, files, output).expect("a spec over at least one file")
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
            (
                "co",
                "codex exec -s workspace-write --skip-git-repo-check",
                '3',
            ),
            ("gk", "grok --always-approve", '4'),
            ("ki", "kimi", '5'),
        ];
        for (provider, (code, line, digit)) in Provider::ALL.iter().zip(expected) {
            assert_eq!(provider.code(), code);
            assert_eq!(provider.command_line(), line);
            assert_eq!(provider.digit(), digit);
            assert_eq!(Provider::from_digit(digit), Some(*provider));
        }
    }

    /// The bug this table exists to prevent: a prompt appended as a bare
    /// trailing word. `agy` answers that with "Prompts are read only from
    /// -p/--print, -i/--prompt-interactive, or stdin", and the others open
    /// an interactive session no background job can answer.
    #[test]
    fn every_provider_takes_the_prompt_through_its_non_interactive_flag() {
        let expected: [(Provider, &[&str]); 5] = [
            (
                Provider::Ag,
                &["agy", "--dangerously-skip-permissions", "-p", "PROMPT"],
            ),
            (
                Provider::Cc,
                &["claude", "--dangerously-skip-permissions", "-p", "PROMPT"],
            ),
            (
                Provider::Co,
                &[
                    "codex",
                    "exec",
                    "-s",
                    "workspace-write",
                    "--skip-git-repo-check",
                    "PROMPT",
                ],
            ),
            (Provider::Gk, &["grok", "--always-approve", "-p", "PROMPT"]),
            (Provider::Ki, &["kimi", "-p", "PROMPT"]),
        ];
        for (provider, argv) in expected {
            assert_eq!(
                provider.argv_with_prompt("PROMPT"),
                argv.iter().map(|w| (*w).to_string()).collect::<Vec<_>>(),
                "{} argv",
                provider.code()
            );
        }
    }

    /// A flag whose value is a name looked up in the user's own
    /// configuration rather than a word the CLI understands by itself.
    /// The value is portable-looking and still machine-local, which is
    /// why the flag, not the value, is what gives it away.
    fn selects_a_configured_name(flag: &str) -> bool {
        matches!(
            flag,
            "-p" | "--profile" | "-c" | "--config" | "--config-file" | "--settings"
        )
    }

    /// A value that names a place instead of a mode: absolute, home
    /// relative, environment expanded, or any other path.
    fn names_a_place(value: &str) -> bool {
        value.starts_with('/')
            || value.starts_with('~')
            || value.starts_with('.')
            || value.contains('$')
            || value.contains('/')
    }

    /// The fixed line has to run on any machine that has the CLI
    /// installed, so no word in it may name something only one machine
    /// has. `codex exec -p lavish` was exactly that: it needed a
    /// `$CODEX_HOME/lavish.config.toml` no other user has, and codex
    /// exits with a config error rather than writing a summary.
    ///
    /// A flag that takes a value is fine - `-s workspace-write` is a
    /// mode every install understands. What is refused is a value looked
    /// up in the user's configuration, and *any* word that is a path,
    /// wherever in the line it sits: the program, a subcommand, a flag's
    /// value, or a bare trailing argument.
    fn machine_local_words(argv: &[String]) -> Vec<String> {
        let mut found = Vec::new();
        let mut expects_a_value: Option<&str> = None;
        for word in argv {
            if let Some((flag, value)) = word.split_once('=').filter(|_| word.starts_with('-')) {
                expects_a_value = None;
                if selects_a_configured_name(flag) || names_a_place(value) {
                    found.push(word.clone());
                }
            } else if word.starts_with('-') {
                expects_a_value = Some(word);
            } else {
                match expects_a_value.take() {
                    Some(flag) if selects_a_configured_name(flag) => {
                        found.push(format!("{flag} {word}"));
                    }
                    _ if names_a_place(word) => found.push(word.clone()),
                    _ => {}
                }
            }
        }
        found
    }

    /// The reopen table, read off each installed CLI's own `--help`.
    ///
    /// Not guessable from the run table and not uniform: only two of the
    /// five call it `--resume`. This is the same lesson `prompt_flag`
    /// carries, and it matters more here, because the line is printed as
    /// advice the user will type themselves.
    #[test]
    fn every_provider_names_the_reopen_command_its_own_cli_has() {
        let expected = [
            (Provider::Ag, "agy --conversation ID"),
            (Provider::Cc, "claude --resume ID"),
            (Provider::Co, "codex resume ID"),
            (Provider::Gk, "grok --resume ID"),
            (Provider::Ki, "kimi --session ID"),
        ];
        for (provider, line) in expected {
            assert_eq!(provider.resume_command("ID"), line);
            // It reopens the same program the run was, and carries the
            // identifier exactly once, as the last word.
            assert_eq!(provider.resume_words()[0], provider.program());
            assert!(line.ends_with(" ID"));
            assert_eq!(line.matches("ID").count(), 1);
        }
    }

    /// The reopen line is advice for another machine's shell as much as
    /// this one's, so it is held to the same rule the run line is.
    #[test]
    fn no_reopen_line_carries_a_machine_local_value() {
        for provider in Provider::ALL {
            let words: Vec<String> = provider
                .resume_words()
                .iter()
                .map(|w| (*w).to_string())
                .collect();
            let offenders = machine_local_words(&words);
            assert!(
                offenders.is_empty(),
                "{}: {offenders:?} exist on one machine only",
                provider.code()
            );
        }
    }

    /// Nothing a provider printed reaches the footer unchecked: the
    /// identifier has already been through [`crate::session::is_id`], and
    /// a run without one says so rather than printing half a command.
    #[test]
    fn the_footer_is_written_from_the_same_table_the_header_reads() {
        let footer = session::footer(
            &Provider::Ag.program(),
            Some("abc-123456"),
            Some(&Provider::Ag.resume_command("abc-123456")),
        );
        assert_eq!(
            footer,
            "> Provider: agy | Session: abc-123456 | \
             Resume with: agy --conversation abc-123456"
        );
    }

    /// A pipe that hands over exactly the chunks it was given, so a test
    /// can split a character across two reads the way a real one does.
    /// It records how much of the log had arrived before each read, which
    /// is the difference between output that streams and output that only
    /// lands once the pipe closes.
    struct Chunks {
        chunks: std::collections::VecDeque<Vec<u8>>,
        live: stream::Handle,
        held: Arc<Mutex<Vec<usize>>>,
    }

    impl Read for Chunks {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            let total = self.live.state(Instant::now()).total;
            self.held.lock().unwrap().push(total);
            let Some(chunk) = self.chunks.pop_front() else {
                return Ok(0);
            };
            let taken = chunk.len().min(buf.len());
            buf[..taken].copy_from_slice(&chunk[..taken]);
            if taken < chunk.len() {
                self.chunks.push_front(chunk[taken..].to_vec());
            }
            Ok(taken)
        }
    }

    /// The bug this guards: one byte that is not UTF-8 stalling the live
    /// log for the whole rest of the run, so everything the provider
    /// still had to say only appears once the pipe closes - by which time
    /// the point of watching it has gone. A character genuinely split
    /// across two reads still has to be joined rather than replaced.
    #[test]
    fn output_after_a_byte_that_is_not_utf8_still_reaches_the_live_log() {
        let live = stream::Handle::new();
        let held = Arc::new(Mutex::new(Vec::new()));
        let pipe = Chunks {
            chunks: vec![
                b"reading the files\n".to_vec(),
                vec![0xff],
                b"still going\n".to_vec(),
                vec![0xe2, 0x94],
                vec![0x80],
                b" done\n".to_vec(),
            ]
            .into(),
            live: live.clone(),
            held: Arc::clone(&held),
        };
        let text = drain(Some(pipe), stream::Origin::Out, &live);

        // Every line was in the log before the pipe reached its end.
        assert_eq!(held.lock().unwrap().last().copied(), Some(3));
        let lines: Vec<String> = live
            .snapshot_since(0)
            .expect("a log to have been filled")
            .lines
            .iter()
            .map(|line| line.text.clone())
            .collect();
        assert_eq!(
            lines,
            vec![
                "reading the files".to_string(),
                "\u{FFFD}still going".to_string(),
                "\u{2500} done".to_string(),
            ]
        );
        // And what `finish` would save as the summary is what a lossy
        // read of the whole pipe would have given it.
        assert_eq!(
            text,
            "reading the files\n\u{FFFD}still going\n\u{2500} done\n"
        );
    }

    #[test]
    fn a_run_is_signed_once_however_many_threads_reach_its_ending() {
        let tmp = tempfile::tempdir().unwrap();
        let output = tmp.path().join("notes-summary.md");
        std::fs::write(&output, "# Summary\n\nwritten by the provider\n").unwrap();
        let signed = AtomicBool::new(false);
        for _ in 0..3 {
            sign_once(&signed, &output, Provider::Ag, Some("abc-123456"));
        }
        let text = std::fs::read_to_string(&output).unwrap();
        assert_eq!(text.matches("> Provider: agy").count(), 1, "{text}");
        assert!(
            text.ends_with("Resume with: agy --conversation abc-123456\n"),
            "{text}"
        );
    }

    #[test]
    fn no_provider_line_carries_a_machine_local_value() {
        for provider in Provider::ALL {
            let offenders = machine_local_words(&provider.base_argv());
            assert!(
                offenders.is_empty(),
                "{}: {offenders:?} exist on one machine only",
                provider.code()
            );
        }
    }

    /// The guard has to be able to fail, and to fail only on the thing it
    /// is about: a name resolved out of the user's own configuration or a
    /// path, wherever it sits - never a portable mode word.
    #[test]
    fn the_machine_local_guard_reads_words_and_not_positions() {
        let line = |words: &[&str]| -> Vec<String> {
            machine_local_words(&words.iter().map(|w| (*w).to_string()).collect::<Vec<_>>())
        };

        // The bug this guard is named after, and the shapes around it.
        assert_eq!(
            line(&["codex", "exec", "-p", "lavish", "--skip-git-repo-check"]),
            vec!["-p lavish".to_string()]
        );
        assert_eq!(
            line(&["codex", "--config=/Users/me/x.toml"]),
            vec!["--config=/Users/me/x.toml".to_string()]
        );
        // A path is refused wherever it sits, including where no flag is
        // waiting for it: trailing, leading, or as the program itself.
        assert_eq!(
            line(&[
                "codex",
                "exec",
                "-s",
                "workspace-write",
                "/Users/me/notes.toml"
            ]),
            vec!["/Users/me/notes.toml".to_string()]
        );
        assert_eq!(
            line(&["tool", "~/.config/tool.toml"]),
            vec!["~/.config/tool.toml".to_string()]
        );
        assert_eq!(
            line(&["/opt/homebrew/bin/codex", "exec"]),
            vec!["/opt/homebrew/bin/codex".to_string()]
        );

        // Portable constants stay portable.
        for portable in [
            &[
                "codex",
                "exec",
                "-s",
                "workspace-write",
                "--skip-git-repo-check",
            ][..],
            &["agy", "--dangerously-skip-permissions"][..],
            &["some-cli", "-m", "gpt-5", "--color=never"][..],
        ] {
            assert!(line(portable).is_empty(), "{portable:?} is portable");
        }
    }

    /// `codex exec` is the one provider whose prompt is positional - its
    /// `-p` is `--profile`. Every other provider must name a flag, or its
    /// prompt is silently the wrong kind of argument.
    #[test]
    fn only_codex_takes_a_positional_prompt() {
        for provider in Provider::ALL {
            match provider {
                Provider::Co => assert_eq!(provider.prompt_flag(), None),
                other => assert_eq!(other.prompt_flag(), Some("-p"), "{}", other.code()),
            }
        }
    }

    /// Whatever the flags, the prompt stays exactly one argument: never
    /// split on whitespace, never quoted into a string a shell would read.
    #[test]
    fn the_prompt_is_always_one_single_argument() {
        let prompt = "read /a b.md\nand write 'it' \"there\"; rm -rf /";
        for provider in Provider::ALL {
            let argv = provider.argv_with_prompt(prompt);
            assert_eq!(argv.last().map(String::as_str), Some(prompt));
            assert_eq!(
                argv.iter().filter(|arg| arg.contains(prompt)).count(),
                1,
                "{} must carry the prompt once",
                provider.code()
            );
            assert!(
                argv.starts_with(&provider.base_argv()),
                "{} must keep its fixed line first",
                provider.code()
            );
        }
    }

    /// The base line never carries a prompt of its own, and the program is
    /// always its first word - the status row and every error say so.
    #[test]
    fn the_fixed_line_is_a_program_and_flags_only() {
        for provider in Provider::ALL {
            let base = provider.base_argv();
            assert_eq!(provider.program(), base[0]);
            assert_eq!(provider.command_line(), base.join(" "));
            for word in &base[1..] {
                assert!(
                    word.starts_with('-') || !word.contains(' '),
                    "{} has a suspicious fixed word {word:?}",
                    provider.code()
                );
            }
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

    /// Every row's command line starts at the same column, and that is
    /// the column a continuation is indented to.
    #[test]
    fn every_menu_row_starts_its_command_at_the_indent() {
        for (line, provider) in menu_lines(Lang::En).iter().zip(Provider::ALL) {
            let prefix = format!("[{}] {}: ", provider.digit(), provider.code());
            assert_eq!(prefix.chars().count(), MENU_INDENT, "{prefix:?}");
            assert!(line.starts_with(&prefix), "{line:?}");
            assert!(line[MENU_INDENT..].starts_with(&provider.command_line()));
        }
    }

    #[test]
    fn the_menu_marks_exactly_one_default() {
        let lines = menu_lines(Lang::En);
        assert_eq!(lines.len(), 5);
        assert_eq!(
            lines[0],
            "[1] ag: agy --dangerously-skip-permissions  [Default]"
        );
        assert_eq!(
            lines[2],
            "[3] co: codex exec -s workspace-write --skip-git-repo-check"
        );
        assert_eq!(lines[4], "[5] ki: kimi");
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
        let earlier_taken = |p: &Path| {
            matches!(
                p.file_name().and_then(|name| name.to_str()),
                Some(
                    "report-summary.md"
                        | "report-summary-20260829-101500.md"
                        | "summary-20260829-101500.md"
                        | "report-summary-20260829-101500-2.md"
                )
            )
        };
        assert_eq!(
            output_path_with(&first, "20260829-101500", &earlier_taken),
            PathBuf::from("/docs/report-summary-20260829-101500-3.md")
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
            None
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

    /// A provider that skims filenames, or that answers "done" without
    /// writing anything, is the failure this wording exists to prevent.
    #[test]
    fn the_prompt_says_to_read_the_files_and_to_actually_write_the_output() {
        let prompt = spec(&["/docs/one.pdf", "/docs/two.md"]).prompt();
        assert!(prompt.contains("Open and read every file listed above"));
        assert!(prompt.contains("not what their names suggest"));
        assert!(prompt.contains("exact absolute path"));
        assert!(prompt.contains("must exist on disk"));
        assert!(prompt.contains("write it with a shell command instead"));
        assert!(prompt.contains("stdout"));
    }

    #[test]
    fn argv_is_the_fixed_provider_line_the_prompt_flag_and_the_prompt() {
        let spec = spec(&["/docs/one.pdf"]);
        let argv = spec.argv();
        assert_eq!(
            argv,
            vec![
                "agy".to_string(),
                "--dangerously-skip-permissions".to_string(),
                "-p".to_string(),
                spec.prompt(),
            ]
        );
    }

    /// Every provider's spec argv, whole - the flags and the one prompt.
    #[test]
    fn a_spec_builds_the_right_argv_for_every_provider() {
        for provider in Provider::ALL {
            let mut spec = spec(&["/docs/one.pdf", "/docs/two.md"]);
            spec.provider = provider;
            let argv = spec.argv();
            assert_eq!(argv, provider.argv_with_prompt(&spec.prompt()));
            assert_eq!(argv[0], provider.program());
            assert_eq!(argv.last(), Some(&spec.prompt()));
            assert_eq!(
                argv.len(),
                provider.base_argv().len() + usize::from(provider.prompt_flag().is_some()) + 1,
                "{} argv length",
                provider.code()
            );
        }
    }

    #[test]
    fn the_status_line_counts_the_files_and_names_the_program() {
        assert_eq!(
            spec(&["/docs/one.pdf", "/docs/two.md", "/docs/three.txt"]).status_line(Lang::En),
            "[AI: summarizing 3 files with agy]"
        );
        assert_eq!(
            spec(&["/docs/one.pdf"]).status_line(Lang::En),
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
    fn stdout_from_a_failed_provider_is_a_diagnostic() {
        assert_eq!(
            finish(false, false, "agy: request failed\n", ""),
            Finish::Failed(Failure::Provider("agy: request failed".to_string()))
        );
    }

    #[test]
    fn a_failure_note_carries_the_reported_reason() {
        assert_eq!(
            failure_note(&Failure::Provider("agy: request failed".to_string())),
            "# Summary failed\n\nagy: request failed\n"
        );
        // The note in the file is always English, whatever the screen is
        // saying: the file outlives the session that wrote it.
        assert_eq!(
            failure_note(&Failure::NoOutput),
            "# Summary failed\n\nthe provider wrote nothing\n"
        );
    }

    #[test]
    fn a_run_with_nothing_to_show_reports_what_the_provider_said() {
        assert_eq!(
            finish(false, false, "", "agy: not logged in\n"),
            Finish::Failed(Failure::Provider("agy: not logged in".to_string()))
        );
        assert_eq!(
            finish(false, false, "   \n", "  \n"),
            Finish::Failed(Failure::NoSummary)
        );
        assert_eq!(
            finish(true, false, "", ""),
            Finish::Failed(Failure::NoOutput)
        );
    }

    #[test]
    fn a_long_failure_line_is_bounded() {
        let noisy = "x".repeat(500);
        let Finish::Failed(Failure::Provider(detail)) = finish(false, false, "", &noisy) else {
            panic!("expected a failure naming what the provider said");
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
