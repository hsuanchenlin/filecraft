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
  Set `FILECRAFT_ASCII` to any non-empty value to draw the screen using
  printable ASCII only, for braille displays, serial terminals, and
  locales where the box-drawing range is unreliable.
- **Reader:** built in and read-only, for Markdown (`.md`, `.markdown`)
  and any other file that looks like text. No external pager is needed
  and none is launched.
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
| `l`, Right | enter the selected directory, or read the selected file |
| `h`, Left, Backspace | parent directory |
| `0`-`9` | jump to that ancestor on the ladder |
| `/` | filter the listing (Esc clears) |
| `:` | command prompt |
| `.` | show/hide dotfiles |
| `r` | refresh listing |
| `M` | message history |
| `?` | help |
| Esc | back out one level (clears an active filter) |
| `q`, Ctrl-C | quit |

Files are never opened automatically. Enter on a file, or the `edit`
command, is the only way into an editor.

Every navigation and orientation key is read-only. Filesystem commands
still go through select -> `:` command -> `y`; opening a file in the
configured editor remains the explicit path for editing file contents.

**Changed in this slice:** `l` on a text or Markdown file now opens the
built-in reader (below) instead of refusing. On a directory it still
enters, and on `../` it still goes up, so the key means one thing: go
in. Nothing about it can change a file.

**Changed in an earlier slice:** `l` **enters** the selected directory
instead of going to the parent, matching vim, ranger, lf, and nnn. Esc
**backs out one level** - it clears an active filter, or closes a pager -
instead of quitting. Quitting is `q` or Ctrl-C.

## Folder picker

`:move` with no path opens a BBS-styled folder picker over the listing.
It lists `./`, `../`, and the child folders of the directory it is
showing - siblings only come into view after going up with `h`.
The header names the destination currently under the cursor. Choosing a
folder (`Enter` or `m`) hands that canonical path to the same `y/n`
confirmation as a typed `:move <path>`. `q` or Esc cancels and returns
to the listing; nothing is moved until `y`.

| Key | Action |
| --- | --- |
| `j` / `k`, Down / Up | move focus |
| `l`, Right | enter the focused folder |
| `h`, Left, Backspace | parent directory |
| `g` / `G` | first / last folder |
| Enter, `m` | choose the focused folder, then confirm |
| `q`, Esc | cancel, back to the listing |

## Reader

`l` (or Right) on a Markdown or plain-text file opens a full-screen,
read-only reader. It never writes, never shells out, and never leaves the
listing: closing it lands on exactly the row it was opened from.

| Key | Action |
| --- | --- |
| `j` / `k`, Down / Up | scroll one line |
| `d` / `u` | scroll half a page |
| `f` / `b`, PgDn / PgUp | scroll a page |
| `g` / `G`, Home / End | top / bottom |
| `/` | find in this file (type, then Enter) |
| `n` / `N` | next / previous match |
| `h`, `q`, Esc | back to the listing |

The frame names the file at the top and says where the view sits at the
bottom in words - `line 42 of 310 · 13%` - the same rule the status row
follows, so the position is never carried by a scroll thumb alone.

Markdown is drawn in the BBS palette: headings, list bullets, blockquotes,
fenced and inline code, and thematic breaks. Every one of them keeps a
textual marker (`#`, a bullet, a quote bar, backticks), so `NO_COLOR` and
`FILECRAFT_ASCII` lose the color and the drawing characters but not the
structure. Lines wrap on real display columns, so wide CJK text reflows
without jitter and a wrapped bullet stays under its own text.

A binary file is refused in a message rather than painted on the screen;
so are broken symlinks and special files. Files are shown up to 1 MiB and
20,000 lines, and a truncated file says so on its last line.

## Bearings

The screen has an orientation half and an operating half, and the
boundary is a rule: **everything above the listing is read-only**. It
cannot be focused, and no key that starts there changes anything. The
listing is the single thing commands act on.

```
╔ ░▒▓ FILECRAFT v0.1.0 ▓▒░ ════════════════════════════════════════════════════╗
║ 0·~ ▸ … ▸ 7·final ▸ 8·assets                              depth 8 · 73 items ║
║│  file_059.txt                                                     0B  1h    ║
║█  file_060.txt                                                     0B  1h    ║
║█> file_061.txt                                                     0B  2d    ║
║ row 61 of 74 · rows 47-61 of 74 · file_061.txt · file · 0B · 2d ago          ║
```

- **Ladder** - the ancestor chain, replacing the raw path line. Digits
  jump to the ancestor they label; `0` is `~` under your home directory
  and `/` elsewhere. Deep paths elide in the middle, so the anchor and
  where you actually are are both always on screen.
- **Rail** - the left gutter shows which slice of the listing is on
  screen. When everything fits there is no thumb.
- **Speakable status** - one row, at a fixed height, describing the whole
  position in words: `row 61 of 74 · rows 47-61 of 74 · file_061.txt ·
  file · 0B · 2d ago`. It is the textual dual of the rail and the
  ladder, so nothing is carried by shape or color alone and "read the
  current line" always works. A narrow row drops trailing segments, but
  never `rows A-B of N` - the rail always has its words.
- **Relative times** - `2d`, `11m`, `1h` in the listing instead of a
  20-column UTC stamp, which needs no timezone and returns those columns
  to the filename. Absolute times stay in `preview`.

## Commands

Typed at the `:` prompt. Parsed directly: no shell, no globbing, no
`$VAR` expansion. Quote names that contain spaces.

| Command | Action |
| --- | --- |
| `cd [path]` | change directory (`~` is home; no argument also goes home) |
| `move [destination]` | move the selected entry (asks `y/n`, never overwrites). No path opens a folder picker; a typed path still goes straight to confirm |
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
editor argv, agent boundary, bearings, reader, folder picker).
`src/bearings.rs` holds the pure orientation arithmetic - ladder, rail,
scroll margin, relative time, speakable status - so all of it is tested
without a TTY; `src/markdown.rs` holds the reader's line classification,
inline emphasis, and width-aware wrapping, and `src/pager.rs` its
scroll, search, and position. `src/picker.rs` holds the move folder
picker's listing, cursor, and destination path. `src/ui.rs` adds
golden-frame tests at 80x24, 100x30, 132x40, and 60x20. `tests/cli.rs`
drives the binary for `--help`/`--list`/non-TTY behavior.

## License

MIT. See [LICENSE](LICENSE).
