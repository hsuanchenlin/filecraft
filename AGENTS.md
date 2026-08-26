# Project agent memory

This file is the project's committed home for project-intrinsic agent knowledge: build, test, release, architecture, and sharp-edge notes that should travel with the code.

- Add durable project-specific notes here as they are discovered through real work.

## Stack

Rust 2021 library plus a `filecraft` binary. TUI is ratatui 0.29. The library in `src/lib.rs` has no terminal event loop; `src/main.rs` owns TTY detection and the interactive screen.

- `cargo test` is the full local test command (module tests plus `tests/cli.rs`).
- `cargo run -- --list [DIR]` is the non-TTY listing path.
- v0 has no delete command. Do not add recursive (or any) deletion without an explicit product decision.
- `agent` is a disabled seam (`src/agent.rs`, contract in `docs/agent-seam.md`). Do not invoke an LLM, scan a tree for an agent, or enable autonomous changes.

## Maintaining this file

Keep this file for knowledge useful to almost every future agent session in this project.
Do not repeat what the codebase already shows; point to the authoritative file or command instead.
Prefer rewriting or pruning existing entries over appending new ones.
When updating this file, preserve this bar for all agents and keep entries concise.
