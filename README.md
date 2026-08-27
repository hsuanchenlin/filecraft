# Filecraft

A keyboard-first, BBS-style terminal file navigator for macOS.

Filecraft is a practical local-filesystem MVP: one stable full-screen
terminal view, a directory listing with visible focus, a command prompt,
compact keyboard help, and explicit command/result messages. It hands
files to Neovim (or `$EDITOR`) for editing and preview. It is not a
Finder replacement and does not own the Desktop, Open/Save panels,
iCloud, or default file handling.

## Install

Requires a Rust toolchain (1.83 or newer) and a UTF-8 macOS terminal.

From source:

```sh
git clone https://github.com/hsuanchenlin/filecraft.git
cd filecraft
cargo install --path .
```

From git without a local clone:

```sh
cargo install --git https://github.com/hsuanchenlin/filecraft --locked
```

The binary is named `filecraft`. Put Cargo's bin directory on your
`PATH` (typically `~/.cargo/bin`).

## Supported environment

- **OS:** macOS first. The interactive navigator, `cd`/`move`/`rename`,
  `edit`, and `preview` are local-filesystem only and also run on other
  Unix systems. The `open` command uses `/usr/bin/open` and is macOS-only.
- **Terminal:** Terminal.app, iTerm2, Ghostty, kitty, WezTerm, or
  Alacritty. Needs a real TTY, UTF-8 locale, and at least 80x24 cells.
  Color uses the terminal's ANSI palette. Set `NO_COLOR` to any non-empty
  value to disable color; selection (reverse video), kind markers
  (`/`, `@`, `@!`), and message prefixes (`ok:`, `err:`) stay visible.
- **Editor:** `$EDITOR` if set, otherwise `nvim`. `preview` uses a
  read-only Neovim invocation (`nvim -R -M -n`) when `nvim` is on
  `$PATH` and the file looks like text; otherwise a built-in metadata
  and text preview is used.
- **Input:** keyboard only. There is no mouse binding.

Without a TTY, Filecraft prints a static listing of the given directory
(or `.`) and exits. `--list` forces that path even inside a terminal.

```sh
filecraft --help
filecraft --list ~/Documents
filecraft ~/Documents
```

## Keyboard

In browse mode:

| Key | Action |
| --- | --- |
| `j` / `k`, Down / Up | move focus |
| PgUp / PgDn | move focus a page |
| `g` / `G` | first / last entry |
| Enter | enter a directory, or edit the selected file |
| Backspace, `h`, `l`, Left | parent directory |
| Right | enter selected directory |
| `/` | filter the listing (Esc clears) |
| `:` | command prompt |
| `.` | show/hide dotfiles |
| `r` | refresh listing |
| `?` | help |
| `q`, Esc, Ctrl-C | quit |

Files are never opened automatically. Enter on a file, or the `edit`
command, is the only way into an editor.

## Commands

Typed at the `:` prompt. Parsed directly: no shell, no globbing, no
`$VAR` expansion. Quote names that contain spaces.

| Command | Action |
| --- | --- |
| `cd [path]` | change directory (`~` is home; no argument also goes home) |
| `move <destination>` | move the selected entry (asks `y/n`, never overwrites) |
| `rename <new-name>` | rename the selected entry (asks `y/n`, never overwrites) |
| `open` | hand the selected entry to macOS `open` |
| `edit` | edit the selected regular file in `$EDITOR` or `nvim` |
| `preview` | read-only preview (Neovim if available, else built-in) |
| `agent [...]` | future AI seam; disabled in v0 (see [docs/agent-seam.md](docs/agent-seam.md)) |
| `help` | help screen |
| `quit` | leave Filecraft |

There is no delete command in v0, recursive or otherwise.

Move and rename always show the canonical target and require an explicit
`y` (or Enter). `n` or Esc cancels. Permission errors, missing files,
broken symlinks, unreadable directories, spaces, and Unicode names are
reported in the message log and do not abort the session.

## Safety

- Operations stay on the local filesystem. No network, telemetry,
  background daemon, or hidden file index.
- Commands are never evaluated by a shell.
- Moves never overwrite an existing entry and never copy+delete across
  volumes.
- The Filecraft screen is restored after the editor exits.

## Developer setup

```sh
rustc --version   # 1.83+
cargo test
cargo run -- --list .
cargo fmt
cargo clippy --all-targets -- -D warnings
```

Release build:

```sh
cargo build --release
```

The library under `src/` is terminal-free and is the home for
deterministic tests (navigation, parsing, path safety, confirmation,
editor argv, agent boundary). `tests/cli.rs` drives the binary for
`--help`/`--list`/non-TTY behavior.

## License

MIT. See [LICENSE](LICENSE).
