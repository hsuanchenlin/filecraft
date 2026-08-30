//! Filecraft - a keyboard-first, BBS-style terminal file navigator.
//!
//! The crate is split into a pure, deterministic core and a thin terminal
//! shell:
//!
//! - [`bearings`] computes the read-only orientation chrome (ladder, rail,
//!   speakable status, relative time) from state already in memory.
//! - [`command`] parses BBS command lines (never shell-evaluated).
//! - [`i18n`] is the language Filecraft speaks and every phrase it
//!   says: resolution from the user's setting and the system locale,
//!   and the per-language catalog every other module reads.
//! - [`config`] is the user's own settings file - today just the
//!   language - read and rewritten without disturbing anything else
//!   in it.
//! - [`fsops`] canonicalizes/validates paths and performs safe move/rename.
//! - [`trash`] is the only removal Filecraft has: a recoverable move into
//!   the system Trash, behind a seam so the flow is testable.
//! - [`nav`] models directory listings and cursor/filter state.
//! - [`owner`] resolves a uid/gid to the name the system knows it by.
//! - [`columns`] is the listing's shape: which columns are shown, how
//!   wide each is in the language on screen, and what each says about an
//!   entry.
//! - [`editor`] constructs editor/preview invocations (`$EDITOR`, `nvim`).
//! - [`preview`] builds the built-in metadata/text preview and reads
//!   files for the reader.
//! - [`markdown`] turns Markdown or plain text into styled, wrapped
//!   display rows.
//! - [`pager`] is the read-only full-screen reader: scroll, search, and
//!   the position it reports.
//! - [`stream`] buffers a running provider's stdout and stderr into
//!   whole, numbered, terminal-safe lines.
//! - [`session`] recognizes the session a provider announces and writes
//!   the footer that says how to reopen it.
//! - [`joblog`] is the live log viewer over a summary run: the pane, its
//!   header, and the follow-the-newest-output rule.
//! - [`picker`] is the folder-only destination picker for `:move` with
//!   no path: cursor, descend, ascend, and the dest path it reports.
//! - [`multiselect`] is the cross-directory multi-file selector behind
//!   `:summarize`: folders and summarizable documents, and the ordered
//!   set of files Space builds up.
//! - [`summarize`] decides everything about an AI summary run - which
//!   files qualify, which provider, where the summary lands, what the
//!   prompt says - and owns the seam the background job runs behind.
//! - [`agent`] is the disabled-by-default future AI-agent seam.
//! - [`app`] is the state machine: keys in, [`app::Effect`]s out.
//! - [`cli`] parses argv for the binary.
//! - [`pathcheck`] decides whether a shell can find the binary at all,
//!   and what to add to which startup file when it cannot.
//! - [`update`] is `filecraft update` / `filecraft update --check`.
//! - [`ui`] renders the ratatui screen; `main.rs` owns the event loop.
//!
//! Everything above `ui` is free of terminal I/O so behavior is testable
//! without a TTY. [`update`] talks to `git`/`cargo`/`curl` through a
//! [`update::Host`] seam so those paths are tested without the network,
//! and [`summarize`] spawns its AI provider through a
//! [`summarize::Runner`] seam so the whole summary flow is tested
//! without one installed.

pub mod agent;
pub mod app;
pub mod bearings;
pub mod cli;
pub mod columns;
pub mod command;
pub mod config;
pub mod editor;
pub mod fsops;
pub mod i18n;
pub mod joblog;
pub mod markdown;
pub mod multiselect;
pub mod nav;
pub mod owner;
pub mod pager;
pub mod pathcheck;
pub mod picker;
pub mod preview;
pub mod session;
pub mod stream;
pub mod summarize;
pub mod trash;
pub mod ui;
pub mod update;
