# Project agent memory

This file is the project's committed home for project-intrinsic agent knowledge: build, test, release, architecture, and sharp-edge notes that should travel with the code.

- Add durable project-specific notes here as they are discovered through real work.

## Stack

Rust 2021 library plus a `filecraft` binary. TUI is ratatui 0.29. The library in `src/lib.rs` has no terminal event loop; `src/main.rs` owns TTY detection and the interactive screen.

- `cargo test` is the full local test command (module tests plus `tests/cli.rs`).
- `cargo run -- --list [DIR]` is the non-TTY listing path.
- `src/bearings.rs` owns every pure display computation (ladder, rail,
  scroll margin, relative time, speakable status, width padding,
  sanitizing, hanging-indent wrapping). Put new rendering arithmetic
  there, not in `ui.rs`, so it stays testable without a TTY.
- The reader (`l` on a text/Markdown file) splits the same way:
  `src/markdown.rs` classifies lines and wraps them to a column budget,
  `src/pager.rs` owns scroll/search/position, `ui.rs` only turns a
  `markdown::Span` into a ratatui span. A `DocLine`'s marker stays a
  `markdown::Marker` until layout, so one parsed document draws correctly
  in either character set - never bake glyphs into parsed state.
- `pager::FRAME_ROWS`/`FRAME_COLS` are what `App::pager_rows`/`pager_cols`
  subtract from the mirrored viewport; they must match the reader block's
  borders plus padding in `ui::draw_pager` or scrolling and drawing
  disagree about what a row is.
- The `:move` folder picker splits the same way: `src/picker.rs` owns
  folder listing, cursor, descend/ascend, and the dest path;
  `ui.rs` only draws it. `picker::FRAME_ROWS` must match the popup's
  borders plus dest header in `ui::draw_picker`. `l`/Right descend;
  Enter/`m` select into the existing confirm flow; `q`/Esc cancel.
  `:move <path>` still skips the picker. The picker does not change
  `NavState::cwd` - cancelling lands on the same listing row.
- Screens are asserted as golden frames in `src/ui.rs` tests via ratatui's
  `TestBackend` at 80x24, 100x30, 132x40, and 60x20. A wide character owns
  two cells and only the first carries the symbol; dump a buffer with the
  `buffer_text` helper rather than reading cells one per column.
- `App::viewport_rows`/`viewport_cols`/`glyphs` are mirrored from the
  terminal by `main.rs` every frame. Key handling fits the ladder to those
  same numbers, which is what makes every digit drawn a key that works.
  Mirror geometry through `App::set_viewport`, never by assigning the
  fields: it is also where an open reader's offset is re-clamped, so a
  resize cannot leave `top_line`, the position footer, and `n`/`N`
  reading an offset the screen no longer has.
- `Pager::rows` memoizes the laid-out document on `(width, glyphs)`; a
  frame and a keypress each ask for it several times. Text the app itself
  writes reaches the reader through `markdown::DocLine::body`/`meta`,
  which is where tabs and control characters are cleaned - every column
  budget downstream assumes that has already happened.
- Removal is `src/trash.rs` and only that: `d` / `:delete` / `:trash`
  move the selected entry into the macOS Trash through `NSFileManager`
  behind the `Trasher` seam, so the confirmation flow is tested against a
  fixture directory, never the real `~/.Trash`. Unrecoverable deletion
  stays forbidden - no `remove_file`, `remove_dir`, `remove_dir_all`, or
  `unlink` in shipped code, and `filecraft_never_calls_a_permanent_removal`
  in `src/trash.rs` scans the source (recursively) for exactly that.
  `check_trashable` runs inside `Trasher::trash` itself, so the `..`
  refusal cannot be skipped by a new implementation or call site.
  `rm`/`del`/`rmdir` stay unknown commands on purpose. Widening removal
  past move-to-Trash needs an explicit product decision.
- A trash is confirmed by `y`/`Y` only. Enter still answers a move or a
  rename, and `PendingOp::needs_explicit_yes` is what draws the line -
  `d` is a page-scroll in the reader and Enter activates a row in browse,
  so the two are one slip apart.
- One operating locus: the listing. Chrome above it is read-only and must
  never become a second selectable/operable pane - that ambiguity is what
  the confirmation flow's safety argument rests on.
- Every browse key must stay read-only, and `d` is the one that is
  allowed to *arm* an operation without performing it;
  `no_browse_key_ever_mutates_the_filesystem` in `src/app.rs` enforces
  both halves mechanically - the tree is unchanged after every key, the
  fixture Trash stays empty, and only `d` may leave `pending` set. `?`
  help, the README keyboard table, and `help_lines()` ship in the same
  change as any key change - the reader's, folder picker's, and
  confirmation prompt's keys included.
- `agent` is a disabled seam (`src/agent.rs`, contract in
  `docs/agent-seam.md`). It stays disabled: do not enable autonomous
  changes or let anything scan a tree "for context".
- The AI summarizer is the separate, shipped, fully explicit flow:
  `src/summarize.rs` decides everything (eligible extensions, the fixed
  provider table and its argv, the output path, the prompt, and what a
  finished child meant), `src/multiselect.rs` owns the cross-directory
  file selection, `ui.rs` only draws them. A provider is spawned through
  the `summarize::Runner` seam, so `app.rs` tests script a fake and
  `tests/summarize_process.rs` exercises the real `ProcessRunner`
  against stub programs on a `$PATH` that test binary controls - no AI
  CLI and no network are ever needed. `multiselect::FRAME_ROWS` must
  match the selector popup's borders plus header in `ui::draw_selector`.
  A provider's argv is a fixed table and never assembled from user
  input; `:summarize` deliberately has no path form for the same reason.
  Every line in that table is its CLI's **headless** form, and the prompt
  goes through `Provider::prompt_flag` - a prompt appended as a bare
  trailing word is refused (`agy`: "Prompts are read only from
  -p/--print...") or opens a session a background job cannot answer.
  The flags are per-CLI and not guessable: `codex`'s prompt is positional
  after `exec` (its `-p` is `--profile`), and `kimi` refuses to combine
  `--yolo`/`--auto` with `--prompt`. Check a real `--help` before adding
  or changing a provider - the stubs in `tests/summarize_process.rs`
  reproduce each CLI's refusal, so a wrong flag fails there rather than
  in front of a user. A fixed line must also name nothing that exists on
  one machine only - a `--profile`, a config path - or the provider is
  unusable for everyone but its author;
  `no_provider_line_carries_a_machine_local_value` refuses a value looked
  up in the user's own config (`-p`/`--profile`/`--config`) and any word
  that is a path, wherever in the line it sits, and allows a portable
  mode word such as `codex`'s `-s workspace-write`.
  At most one job runs: `App::job` is the single thing the status row,
  the quit confirmation, and the completion message all refer to.
  `ProcessRunner` atomically reserves the output before spawning. A failed
  or terminated run fills that reservation with a Markdown failure note;
  it is never removed or left empty.
- A summary run is polled, never waited on. `main.rs` ticks with
  `event::poll(JOB_TICK)` while `App::job_active()`, so the TUI keeps
  answering keys and a finished run is reported without a keypress.
  `q` / Ctrl-C with a job alive raises `Mode::ConfirmQuit`, and only
  `y` terminates the child - Enter is not an answer there, same rule as
  a trash. `terminate` runs on the UI thread, so it waits only
  `TERMINATE_GRACE` for the run to wind up: killing the child does not
  close pipes a grandchild inherited, and an unbounded wait leaves the
  TUI frozen in raw mode. Past the grace `terminate` itself finishes the
  job, through `finish` like every other ending - the reservation is an
  `Arc<Mutex<File>>` shared with the worker for exactly that, because the
  app drops the job the moment `terminate` returns. Going through
  `finish` is what keeps the note from overwriting a summary the provider
  had already written; a blocked drain does not mean an unfinished run.
- `no_browse_key_ever_mutates_the_filesystem` and its reader twin also
  assert that no key starts an AI run. `S` may open the selector and
  nothing more; a provider only ever runs after a selection *and* a
  chosen provider.
- `filecraft update` lives in `src/update.rs`. `cli.rs` only parses
  `update` / `--check`; `main.rs` prints the report. Tests inject a
  fake `Host` so detection, command construction, and error mapping
  run without the network. A folder named `update` is `./update`.
  `Host::install_root` is where `cargo install` writes -
  `$CARGO_INSTALL_ROOT`, else `$CARGO_HOME`, else `~/.cargo` - and it
  holds both `bin/` and `.crates.toml`; reading it as plain `CARGO_HOME`
  points the PATH advice at a directory the binary never lands in.
- Installing to `~/.cargo/bin` is only half an install: a macOS zsh does
  not search there, so a success is followed by `command not found`.
  `src/pathcheck.rs` is the pure decision (is this directory on `PATH`,
  which startup file, which line) and `install.sh` is the same decision
  in bash at install time; `filecraft update` reports it through
  `UpdateReport::path_advice`. The two must agree on the line they
  print - `install_script_and_update_advice_agree_on_the_path_line` in
  `tests/cli.rs` is what holds them together. `install.sh` sourced with
  `FILECRAFT_INSTALL_LIB=1` defines its functions and installs nothing,
  which is how `scripts/install_test.sh` unit-tests it; that script runs
  inside `cargo test`, so CI covers the shell half too.

## Releasing

- The version lives in exactly one place: `package.version` in
  `Cargo.toml`. `env!("CARGO_PKG_VERSION")` is what the banner,
  `filecraft --version`, and `update`'s current-vs-latest comparison all
  read - there is no second copy to keep in step.
- `filecraft update` never looks at a tag or a GitHub release. It compares
  against `Cargo.toml` on `main` (`MANIFEST_URL`/`MAIN_REF` in
  `src/update.rs`) and installs from the branch - the argv is
  `cargo install --git <GIT_URL> --locked --force`, with no `--tag`. So
  **bumping the version on `main` is what ships an update**; a release is a
  marker and a changelog for people. Tagging without the bump updates
  nobody, and bumping without a tag updates everybody.
- Releases are cut by hand: `.github/workflows/ci.yml` is test-only and
  builds no artifacts, so nothing tags or publishes on its own.
  `gh-axi release create v<version> --target main --title "Filecraft
  v<version>" --notes-file <file>`. They carry no binaries on purpose -
  both documented installs (`./install.sh`, `cargo install --git`) build
  from source, so an asset would be a third install path to keep honest.

## Maintaining this file

Keep this file for knowledge useful to almost every future agent session in this project.
Do not repeat what the codebase already shows; point to the authoritative file or command instead.
Prefer rewriting or pruning existing entries over appending new ones.
When updating this file, preserve this bar for all agents and keep entries concise.
