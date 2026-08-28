# Project agent memory

This file is the project's committed home for project-intrinsic agent knowledge: build, test, release, architecture, and sharp-edge notes that should travel with the code.

- Add durable project-specific notes here as they are discovered through real work.

## Stack

Rust 2021 library plus a `filecraft` binary. TUI is ratatui 0.29. The library in `src/lib.rs` has no terminal event loop; `src/main.rs` owns TTY detection and the interactive screen.

- `cargo test` is the full local test command (module tests plus `tests/cli.rs`).
- `cargo run -- --list [DIR]` is the non-TTY listing path.
- `src/bearings.rs` owns every pure display computation (ladder, rail,
  scroll margin, relative time, speakable status, width padding,
  sanitizing). Put new rendering arithmetic there, not in `ui.rs`, so it
  stays testable without a TTY.
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
- `agent` is a disabled seam (`src/agent.rs`, contract in `docs/agent-seam.md`). Do not invoke an LLM, scan a tree for an agent, or enable autonomous changes.
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

## Maintaining this file

Keep this file for knowledge useful to almost every future agent session in this project.
Do not repeat what the codebase already shows; point to the authoritative file or command instead.
Prefer rewriting or pruning existing entries over appending new ones.
When updating this file, preserve this bar for all agents and keep entries concise.
