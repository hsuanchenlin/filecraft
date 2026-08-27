//! Filecraft - a keyboard-first, BBS-style terminal file navigator.
//!
//! The crate is split into a pure, deterministic core and a thin terminal
//! shell:
//!
//! - [`bearings`] computes the read-only orientation chrome (ladder, rail,
//!   speakable status, relative time) from state already in memory.
//! - [`command`] parses BBS command lines (never shell-evaluated).
//! - [`fsops`] canonicalizes/validates paths and performs safe move/rename.
//! - [`nav`] models directory listings and cursor/filter state.
//! - [`editor`] constructs editor/preview invocations (`$EDITOR`, `nvim`).
//! - [`preview`] builds the built-in metadata/text preview and reads
//!   files for the reader.
//! - [`markdown`] turns Markdown or plain text into styled, wrapped
//!   display rows.
//! - [`pager`] is the read-only full-screen reader: scroll, search, and
//!   the position it reports.
//! - [`agent`] is the disabled-by-default future AI-agent seam.
//! - [`app`] is the state machine: keys in, [`app::Effect`]s out.
//! - [`cli`] parses argv for the binary.
//! - [`ui`] renders the ratatui screen; `main.rs` owns the event loop.
//!
//! Everything above `ui` is free of terminal I/O so behavior is testable
//! without a TTY.

pub mod agent;
pub mod app;
pub mod bearings;
pub mod cli;
pub mod command;
pub mod editor;
pub mod fsops;
pub mod markdown;
pub mod nav;
pub mod pager;
pub mod preview;
pub mod ui;
