# Filecraft

A keyboard-first, BBS-style terminal file navigator for macOS.

Filecraft is a practical local-filesystem MVP: one stable full-screen
terminal view, a directory listing with visible focus, a command prompt,
compact keyboard help, and explicit command/result messages. It hands
files to Neovim (or `$EDITOR`) for editing and preview, and can hand a
set of documents to an AI CLI you already have for a Markdown summary.
It is not a
Finder replacement and does not own the Desktop, Open/Save panels,
iCloud, or default file handling.

## Install

Requires a Rust toolchain (1.85 or newer) and a UTF-8 macOS terminal.

```sh
git clone https://github.com/hsuanchenlin/filecraft.git
cd filecraft
./install.sh
```

That is the whole install. `install.sh` runs
`cargo install --path . --locked --force`, then checks that the directory
it installed into is on your `PATH` and offers to add it to your shell's
startup file if it is not. Re-running it is safe: the edit is fenced by
markers and made only once.

```
./install.sh            # install, then ask before editing your startup file
./install.sh --yes      # install and edit it without asking
./install.sh --dry-run  # print every change without making one
./install.sh --no-path  # install only; never touch a startup file
./install.sh --link     # also symlink the binary into ~/.local/bin
./install.sh --help
```

### Installing by hand

```sh
cargo install --path .                                             # from a clone
cargo install --git https://github.com/hsuanchenlin/filecraft --locked  # without one
```

Either way the binary lands in Cargo's bin directory, normally
`~/.cargo/bin`. A macOS zsh does not search that directory unless
something puts it there, so a successful install is still followed by:

```
zsh: command not found: filecraft
```

Add the directory to your `PATH` once and the problem is gone:

```sh
echo 'export PATH="$HOME/.cargo/bin:$PATH"' >> ~/.zshrc   # zsh (macOS default)
echo 'export PATH="$HOME/.cargo/bin:$PATH"' >> ~/.bashrc  # bash
echo 'fish_add_path $HOME/.cargo/bin' >> ~/.config/fish/config.fish  # fish
```

Then open a new terminal, or `source` the file you edited. `filecraft`
now works from any directory:

```sh
filecraft              # open the current working directory
filecraft ~/Documents  # open that directory
filecraft --list ~     # static listing (no TUI)
```

A folder named `update` is opened as `filecraft ./update`.

## Update

`filecraft update` installs the latest version. It detects a local git
clone vs a global `cargo install` so you do not have to remember `git
pull` or the install flags.

```sh
filecraft update --check   # current vs latest, no install
filecraft update           # install the latest
```

From a git clone it runs `git pull --ff-only` in that tree and
`cargo install --path <clone> --locked --force`. From a global cargo
install it runs:

```sh
cargo install --git https://github.com/hsuanchenlin/filecraft.git --locked --force
```

Both forms end with a `PATH` self-check. Installing a binary somewhere
the shell never looks is a silent failure, so if the directory holding
`filecraft` is not on your `PATH`, the report says so and prints the
exact line to add and the file to add it to:

```
warning: /Users/you/.cargo/bin is not on your PATH
  until it is, `filecraft` only runs by full path, not by name
  add it:  echo 'export PATH="$HOME/.cargo/bin:$PATH"' >> ~/.zshrc
  then open a new terminal, or run ./install.sh from a filecraft clone to do this for you
```

Running out of a `target/` build tree is reported against the directory
an install would write to, not the build directory.

Checking requires `curl`; installing requires `cargo` and, for a local
clone, `git`. Network failures, missing tools, and permission errors are
reported and do not crash.

## Supported environment

- **OS:** macOS first. The interactive navigator, `cd`/`move`/`rename`,
  `edit`, and `preview` are local-filesystem only and also run on other
  Unix systems. The `open` command uses `/usr/bin/open` and is macOS-only,
  and so is `delete`, which moves the entry to the macOS Trash through
  `NSFileManager`. On other platforms `delete` reports that and does
  nothing.
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
filecraft update --check
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
| `d` | move the selected entry to the Trash (asks `y/n`) |
| `S` | AI summary: pick files, then a provider |
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

Every navigation and orientation key is read-only. `d` is the only browse
key that can lead to a filesystem change, and it changes nothing by
itself: it raises the same `y/n` prompt `:delete` does, and only `y`
moves anything. Every other filesystem operation goes through select ->
`:` command -> `y`; opening a file in the configured editor remains the
explicit path for editing file contents.

`S` is the second browse key that can lead somewhere outside the
listing, and like `d` it does nothing by itself: it opens a file
selector. Nothing is read, no program is started, and no file is written
until files are picked *and* a provider is chosen.

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
It lists `../`, `./`, and the child folders of the directory it is
showing - siblings only come into view after going up with `h`.
The header names the destination currently under the cursor. Choosing a
folder (`Enter` or `m`) hands that canonical path to the same `y/n`
confirmation as a typed `:move <path>`. `q` or Esc cancels and returns
to the listing; nothing is moved until `y`.

| Key | Action |
| --- | --- |
| `j` / `k`, Down / Up | move focus |
| PgUp / PgDn | move focus a page |
| `l`, Right | enter the focused folder |
| `h`, Left, Backspace | parent directory |
| `g` / `G` | first / last folder |
| Enter, `m` | choose the focused folder, then confirm |
| `q`, Esc | cancel, back to the listing |

## AI summary

`S`, `:summarize`, or `:summary` opens a file selector over the listing.
Mark documents with `Space`, walk into other folders with `l` and `h` -
the marks come with you - and confirm with `Enter` or `c`. A provider
dialog follows; the summary then runs in the background while the screen
stays live.

| Key | Action (file selector) |
| --- | --- |
| `j` / `k`, Down / Up | move focus |
| PgUp / PgDn | move focus a page |
| `Space` | mark / unmark the focused file (`[x]`) |
| `l`, Right | enter the focused folder |
| `h`, Left, Backspace | parent directory |
| `g` / `G` | first / last row |
| Enter, `c` | confirm the selection, then pick a provider |
| `q`, Esc | cancel; nothing is run |

Only `.pdf`, `.md`, `.markdown`, and `.txt` files are offered - the
selector lists folders and those documents, and nothing else.

```
┌ summarize: pick a provider ────────────────────────────────────────────────┐
│ 2 files selected                                                           │
│                                                                            │
│ [1] ag: agy --dangerously-skip-permissions  [Default]                      │
│ [2] cc: claude --dangerously-skip-permissions                              │
│ [3] co: codex -p lavish -a on-request                                      │
│ [4] gk: grok --always-approve                                              │
│ [5] ki: kimi --yolo                                                        │
└───────────────────────────────────── 1-5 choose · Enter default · q cancel ┘
```

`1`-`5` run that provider; **Enter alone runs the default, `ag`**; `q`
or Esc cancels. The command lines are a fixed table in
`src/summarize.rs` - nothing you type ever becomes a program name or a
flag, and the provider is spawned directly, never through a shell.

While it runs, the status row carries the job and keeps it even on a
narrow terminal:

```
 [AI: summarizing 3 files with agy] row 1 of 6 · all rows shown · notes.md
```

The screen stays fully usable: navigate, read files, run other commands.
When the run ends, `ok: summary written to <path>` appears in the log
without a keypress, the listing refreshes, and the new file is placed
under the cursor so `l` opens it in the reader.

The summary is written **beside the first file you marked**, as
`<first-stem>-summary.md`. An existing file of that name is never
overwritten: the run falls back to `<first-stem>-summary-<stamp>.md`.
The provider is asked to write that one path and nothing else; if it
prints the summary instead of writing it, its stdout is saved there.

### Quitting with a summary running

`q` and Ctrl-C ask first:

```
 confirm [y]es / [n]o  task in progress: terminate AI summary and quit?
```

`y` kills the child process and leaves. `n` or Esc keeps it running.
Enter is **not** an answer here, for the same reason it is not one for a
delete: the key that raised the prompt sits one slip away.

### What this does and does not do

- Nothing runs until you mark files and choose a provider. Both are
  explicit key presses.
- The provider is a program you already have installed. Filecraft opens
  no network connection of its own, but that program may - the summary
  is the one place Filecraft hands your file paths to something else.
- Source files are never modified: a summary only ever adds a file.
- This is not the `agent` seam, which stays disabled - see
  [docs/agent-seam.md](docs/agent-seam.md).

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
| `delete`, `trash` | move the selected entry to the macOS Trash (asks `y/n`; recoverable) |
| `open` | hand the selected entry to macOS `open` |
| `edit` | edit the selected regular file in `$EDITOR` or `nvim` |
| `preview` | read-only preview (Neovim if available, else built-in) |
| `summarize`, `summary` | AI summary of files you pick (same as `S`) |
| `agent [...]` | future AI seam; disabled in v0 (see [docs/agent-seam.md](docs/agent-seam.md)) |
| `help` | help screen |
| `quit` | leave Filecraft |

Move, rename, and delete always show what they are about to do and
require an explicit `y`; Enter also answers a move or a rename, but never
a delete. `n`, `q`, or Esc cancels and nothing is touched. Permission
errors, missing files, broken symlinks, unreadable directories, spaces,
and Unicode names are reported in the message log and do not abort the
session.

## Deleting

`d`, `:delete`, and `:trash` are the same operation: the selected entry
is **moved to the macOS Trash**, whole, to be recovered from there.
Filecraft never unlinks a file and never removes a directory tree - there
is no unrecoverable deletion anywhere in it, and a test asserts that
mechanically over the source (`filecraft_never_calls_a_permanent_removal`).

```
 confirm [y]es / [n]o  trash 'notes.md'
```

- `y` moves it to the Trash and re-reads the listing. Enter does **not**:
  `d` is a page-scroll in the reader and Enter activates a row in browse,
  so a delete is answered with the letter and nothing else.
- `n`, `q`, or Esc cancels; nothing is changed.
- Any other key leaves the prompt up and says which keys answer it.
- `../` is refused with an error before any prompt is raised - it names
  the directory you are standing under, not an entry.
- A directory goes to the Trash whole, contents intact. Filecraft does not
  walk it, so there is nothing to half-finish.
- The move goes through `NSFileManager`'s `trashItemAtURL:`, not Finder
  scripting: it needs no Automation permission and cannot silently fail
  for the want of one. Items trashed this way are not always offered
  Finder's "Put Back"; the entry is intact in the Trash either way, and
  dragging it out restores it.

`rm`, `del`, and `rmdir` are deliberately **not** commands. They promise
POSIX removal, and answering them with a trash would be as surprising as
answering them with a deletion.

## Safety

- Operations stay on the local filesystem. No telemetry, background
  daemon, or hidden file index. Filecraft itself opens a network
  connection only in `filecraft update`, and only to fetch and install
  this repository.
- `summarize` is the one command that hands your data to another
  program: an AI CLI you already have, over files you marked yourself,
  and that program may use the network. It runs only after an explicit
  selection and an explicit provider choice, and its command line comes
  from a fixed table - never from anything you type.
- Commands are never evaluated by a shell.
- Moves never overwrite an existing entry and never copy+delete across
  volumes.
- Deletion is a move to the system Trash and is always recoverable. No
  code path in the shipped binary calls `remove_file`, `remove_dir`, or
  `remove_dir_all`.
- A summary never overwrites a file: it writes a new `.md`, falling back
  to a stamped name when the preferred one is taken.
- The Filecraft screen is restored after the editor exits.

## Developer setup

```sh
rustc --version   # 1.85+
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
editor argv, agent boundary, bearings, reader, folder picker,
move-to-Trash).
`src/bearings.rs` holds the pure orientation arithmetic - ladder, rail,
scroll margin, relative time, speakable status - so all of it is tested
without a TTY; `src/markdown.rs` holds the reader's line classification,
inline emphasis, and width-aware wrapping, and `src/pager.rs` its
scroll, search, and position. `src/picker.rs` holds the move folder
picker's listing, cursor, and destination path. `src/trash.rs` holds the
move-to-Trash operation behind a `Trasher` seam, so the confirmation flow
is tested against a fixture directory instead of the real `~/.Trash`.
`src/update.rs` holds `filecraft update`. `src/ui.rs` adds
golden-frame tests at 80x24, 100x30, 132x40, and 60x20. `tests/cli.rs`
drives the binary for `--help`/`--list`/`update`/non-TTY behavior.

## License

MIT. See [LICENSE](LICENSE).
