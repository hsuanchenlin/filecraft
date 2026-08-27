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
- Screens are asserted as golden frames in `src/ui.rs` tests via ratatui's
  `TestBackend` at 80x24, 100x30, 132x40, and 60x20. A wide character owns
  two cells and only the first carries the symbol; dump a buffer with the
  `buffer_text` helper rather than reading cells one per column.
- `App::viewport_rows`/`viewport_cols`/`glyphs` are mirrored from the
  terminal by `main.rs` every frame. Key handling fits the ladder to those
  same numbers, which is what makes every digit drawn a key that works.
- v0 has no delete command. Do not add recursive (or any) deletion without an explicit product decision.
- One operating locus: the listing. Chrome above it is read-only and must
  never become a second selectable/operable pane - that ambiguity is what
  the confirmation flow's safety argument rests on.
- Every browse key must stay read-only; `no_browse_key_ever_mutates_the_filesystem`
  in `src/app.rs` enforces it mechanically. `?` help, the README keyboard
  table, and `help_lines()` ship in the same change as any key change -
  the reader's keys included.
- `agent` is a disabled seam (`src/agent.rs`, contract in `docs/agent-seam.md`). Do not invoke an LLM, scan a tree for an agent, or enable autonomous changes.

## Maintaining this file

Keep this file for knowledge useful to almost every future agent session in this project.
Do not repeat what the codebase already shows; point to the authoritative file or command instead.
Prefer rewriting or pruning existing entries over appending new ones.
When updating this file, preserve this bar for all agents and keep entries concise.
