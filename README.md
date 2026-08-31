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
  Unix systems. The `open` command - and `l` on a file the reader cannot
  draw, which is the same operation - uses `/usr/bin/open` and is
  macOS-only, and so is `delete`, which moves the entry to the macOS
  Trash through `NSFileManager`. On other platforms both report that and
  do nothing.
- **Terminal:** Terminal.app, iTerm2, Ghostty, kitty, WezTerm, or
  Alacritty. Needs a real TTY, UTF-8 locale, and at least 80x24 cells.
  Color uses the terminal's ANSI palette. Set `NO_COLOR` to any non-empty
  value to disable color; selection (reverse video), kind markers
  (`/`, `@`, `@!`), and message prefixes (`ok:`, `err:`) stay visible.
  Set `FILECRAFT_ASCII` to any non-empty value to draw the screen using
  printable ASCII only, for braille displays, serial terminals, and
  locales where the box-drawing range is unreliable.
- **Language:** English and Traditional Chinese (繁體中文). See
  [Language](#language) below.
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

## Language

Filecraft speaks **English** (`en`) and **Traditional Chinese**
(`zh-TW`, 繁體中文). Everything on screen is translated: the ladder and
status rows, the listing, every dialog and confirmation, the reader and
the log viewer, the help screen, and `filecraft --help`.

The language is resolved once at startup, in the order you can predict -
what you said, then what your system said, then English:

| Order | Source | Example |
| --- | --- | --- |
| 1 | `FILECRAFT_LANG` | `FILECRAFT_LANG=zh-TW filecraft` |
| 2 | `language` in `~/.config/filecraft/config.toml` | `language = "zh-TW"` |
| 3 | `LC_ALL`, then `LC_MESSAGES`, then `LANG` | `LANG=zh_TW.UTF-8` |
| 4 | English | nothing set |

A value naming a language Filecraft does not have is skipped rather than
fatal, so a `LANG` of `fr_FR.UTF-8` falls through to English exactly as
an unset one would. `zh_TW`, `zh_HK`, `zh_MO`, and `zh-Hant` all select
Traditional Chinese. `zh_CN`, `zh_SG`, and `zh-Hans` deliberately do
**not**: Simplified Chinese is a different written language, and
answering it with Traditional characters would be a wrong answer rather
than an approximate one. Ask for it explicitly with `:lang zh` if you
want it anyway.

Change it from inside Filecraft at the `:` prompt:

```
:lang            report the current language
:lang zh         switch to Traditional Chinese
:lang en         switch to English
:language zh-TW  the long name and the full code both work
```

The switch takes effect on the next frame and is written to
`~/.config/filecraft/config.toml` (or
`$XDG_CONFIG_HOME/filecraft/config.toml`) so
the next session starts in it - the same file the
[`[columns]` table](#remembering-them) lives in. Only the `language` key
is touched - comments, blank lines, the `[columns]` table, and any other
key in the file are preserved. If
the preference could not be written down, Filecraft says so: the session
still switches, and you know the next one will not.

The message log is a record of what was said, so lines written before a
switch stay in the language they were written in; everything said after
it is in the new one. Each line is named after the operation it came
from - `move:`, `delete:`, `summarize:` in English, `移動:`, `刪除:`,
`摘要:` in Traditional Chinese. What you *type* at the `:` prompt is
always the English command; the help screen's COMMANDS block is the list.

Two things stay in English on purpose. A path, a file name, a flag, and
whatever the operating system or the AI provider itself said are
evidence, not prose, and are passed through untouched under a translated
prefix. And anything Filecraft writes to a *file* - the prompt handed to
a provider, the session footer on a finished summary, the note left when
a run fails - stays stable, because that file outlives the session and is
read by whoever the summary is shared with.

Traditional Chinese is measured, not counted: a Han character owns two
terminal cells, so every listing column and its header, the preview's
label column, every padded status segment, and every hint row are fitted
in display columns. `NO_COLOR` and `FILECRAFT_ASCII` work exactly as they do in
English - `FILECRAFT_ASCII` governs the characters Filecraft *draws*
(borders, rails, bullets), not the language it writes in.

## Keyboard

In browse mode:

| Key | Action |
| --- | --- |
| `j` / `k`, Down / Up | move focus |
| PgUp / PgDn | move focus a page |
| `g` / `G` | first / last entry |
| Enter | enter a directory, or edit the selected file |
| `l`, Right | enter the selected directory, read text, or hand a safe binary document or media file to the macOS default application |
| `h`, Left, Backspace | parent directory |
| `0`-`9` | jump to that ancestor on the ladder |
| `d` | move the selected entry to the Trash (asks `y/n`) |
| `S` | AI summary: pick files, then a provider |
| `/` | filter the listing (Esc clears) |
| `:` | command prompt |
| `.` | show/hide dotfiles |
| `r` | refresh listing |
| `M` | message history |
| `L` | live log of the AI run - also after it has finished |
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

`l` on a PDF, an image, or a video starts the macOS default application
for it, and that is still read-only: Filecraft hands `/usr/bin/open` the
path and nothing else, never writes to the file, and never changes its
permissions. The program is spawned detached, so the Filecraft screen
stays exactly where it was and the event loop keeps answering keys - no
terminal is handed away, the way `edit` and `preview` hand one away.

`S` is the second browse key that can lead somewhere outside the
listing, and like `d` it does nothing by itself: it opens a file
selector. Nothing is read, no program is started, and no file is written
until files are picked *and* a provider is chosen.

**Changed in this slice:** `l` on a PDF, raster image, audio, video, or
non-executable file whose bytes are not text now opens it in the macOS default
application instead of refusing it in words. Text and Markdown still
open in the built-in reader (below), a directory is still entered, and
`../` still goes up - so the key still means one thing: show me this.
Nothing about it can change a file.

**Changed in an earlier slice:** `l` on a text or Markdown file opens the
built-in reader (below) instead of refusing.

**Changed in an earlier slice:** `l` **enters** the selected directory
instead of going to the parent, matching vim, ranger, lf, and nnn. Esc
**backs out one level** - it clears an active filter, or closes a pager -
instead of quitting. Quitting is `q` or Ctrl-C.

## Columns

The listing is a table, and which columns it holds is yours to choose.
Seven are available:

| Column | Header | What it says |
| --- | --- | --- |
| `name` | `NAME` / `名稱` | the entry's name, with the `/ @ @!` kind markers |
| `size` | `SIZE` / `大小` | `973B`, `4.2K`, `1.1G`, or `<DIR>` |
| `modified` | `MODIFIED` / `修改時間` | how long ago it was last written - `11m`, `2d` |
| `created` | `CREATED` / `建立時間` | how long ago it was created (macOS birth time) |
| `kind` | `KIND` / `種類` | `Directory`, `Markdown`, `Rust`, `PDF`, `Image`, … |
| `permissions` | `PERMISSIONS` / `權限` | the `ls -l` mode string, `-rw-r--r--` |
| `owner` | `OWNER` / `擁有者` | `user:group`, by name where the system knows one |

The default is `name`, `size`, `modified` with the header row on - the
listing Filecraft has always drawn, now with its columns named.

A **BBS column header** sits directly above the rows: the column names,
and a rule under them in the same character set the rest of the frame
uses. It is chrome, like the ladder above it - it cannot be focused, and
nothing in it can be operated on. It costs the listing two rows, and
paging and the scroll margin count the rows that actually hold entries,
so what `PgDn` moves by is what you can see.

```
║   NAME                                    SIZE MODIFIED CREATED KIND      ║
║───────────────────────────────────────────────────────────────────────────║
║│> ../                                    <DIR>                  Directory ║
║│  projects/                              <DIR> 1h       3d      Directory ║
║│  Cargo.toml                                9B 1h       3d      TOML      ║
║│  一份很長的中文檔案名稱.md                 1B 1h       3d      Markdown  ║
```

**The name column is the one that stretches.** Every other column has a
width its language declares, and the name takes what is left. When the
terminal is too narrow to hold them all, whole columns are dropped
rather than the name being squeezed into nothing - `owner` first, then
`permissions`, `created`, `kind`, and `modified` last. `name` and `size`
are never dropped, so even an absurdly narrow terminal is still a file
listing.

Every width is in **display cells, never characters**. `修改時間` is the
same eight columns `MODIFIED` is, and `種類` is four where `KIND` is
four, because a Han character owns two cells - so a translated header
can never push a row past the border. `NO_COLOR` and `FILECRAFT_ASCII`
apply here as everywhere: the header is bold rather than colored, and
its rule is drawn with `-` in ASCII mode.

### Changing them

`:columns` with no list opens a picker over the listing:

```
┌ listing columns ───────────────────────────────────────────────────────────┐
│ name is always shown; a narrow terminal drops the rest from the bottom up  │
│ > [x] NAME (name)                                                          │
│   [x] SIZE (size)                                                          │
│   [x] MODIFIED (modified)                                                  │
│   [ ] CREATED (created)                                                    │
│   [ ] KIND (kind)                                                          │
│   [ ] PERMISSIONS (permissions)                                            │
│   [ ] OWNER (owner)                                                        │
│   [x] column header row                                                    │
└──────────────────────── Space toggle · j/k move · Enter/c apply · q cancel ┘
```

| Key | Action (column picker) |
| --- | --- |
| `j` / `k`, Down / Up | move focus |
| PgUp / PgDn | move focus a page |
| `g` / `G` | first / last row |
| `Space` | turn the focused column on or off |
| Enter, `c` | apply, and remember the choice |
| `q`, Esc | cancel; the listing is unchanged |

It edits a copy, so cancelling really does leave the listing as it was.
The name row is listed but never turns off, and the last row is the
header switch itself, so everything `:columns` governs is in one place.

Or say it outright:

```
:columns                              open the picker
:columns name,size,modified,created   set them, in this order
:cols name size kind                  commas and spaces both separate
:set columns=name,size,kind,owner     the same thing again
:header off                           hide the column header row
:header                               report whether it is drawn
:set header=on                        the same thing again
```

A word naming no column is refused and nothing changes. A list that
leaves `name` out gets it back at the front: a listing of sizes with no
names is not a file listing.

### Remembering them

The choice is written to `~/.config/filecraft/config.toml` (or
`$XDG_CONFIG_HOME/filecraft/config.toml`) under a `[columns]` table:

```toml
language = "zh-TW"

[columns]
visible = ["name", "size", "modified", "created", "kind"]
header = true
```

Only those two keys are touched - comments, blank lines, and any other
key in the file are preserved, exactly as `:lang` preserves them. A word
naming a column this version does not have is skipped rather than fatal,
so a file written by a later version still starts this one. If the
preference could not be written down, Filecraft says so: the session
still has the new columns, and you know the next one will not.

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
│ [1] ag: agy --dangerously-skip-permissions  [Default]                      │
│ [2] cc: claude --dangerously-skip-permissions                              │
│ [3] co: codex exec -s workspace-write --skip-git-repo-check                │
│ [4] gk: grok --always-approve                                              │
│ [5] ki: kimi                                                               │
└───────────────────────────────────── 1-5 choose · Enter default · q cancel ┘
```

`1`-`5` run that provider; **Enter alone runs the default, `ag`**; `q`
or Esc cancels. The command lines are a fixed table in
`src/summarize.rs` - nothing you type ever becomes a program name or a
flag, and the provider is spawned directly, never through a shell.

Each row shows the fixed part of the line. The prompt is appended as one
further argument, through whichever flag that CLI reads a prompt from:

| provider | how the prompt is handed over |
| --- | --- |
| `ag` | `-p` (`--print`) |
| `cc` | `-p` (`--print`) |
| `co` | positional, after `exec` - `codex`'s own `-p` is `--profile` |
| `gk` | `-p` (`--single`) |
| `ki` | `-p` (`--prompt`) |

That flag is not decoration. A summary run has no terminal to answer
questions on, so every line is its CLI's headless form: a prompt passed
as a bare trailing word is either refused outright (`agy` answers
`Prompts are read only from -p/--print, -i/--prompt-interactive, or
stdin`) or opens an interactive session nothing can answer. `kimi` is
listed bare because it *refuses* to combine a yolo flag with `--prompt`;
its prompt mode carries its own permissions.

Each line runs on any machine that has the CLI installed: it names no
profile, no config file, and nothing else that would only exist where it
was written. A flag that takes a value is fine when the value is a mode
every install understands, which is why `codex` spells both of its
grants out - `-s workspace-write`, because `codex exec` otherwise takes
its sandbox from your own `config.toml` and a summary is written beside
its sources, and `--skip-git-repo-check`, because a folder of documents
is usually not a git repository.

While it runs, the status row carries the job and keeps it even on a
narrow terminal:

```
 [AI: summarizing 3 files with agy] row 1 of 6 · all rows shown · notes.md
```

The screen stays fully usable: navigate, read files, run other commands.
When the run ends, `ok: summary written to <path>` appears in the log
without a keypress and the new file is placed under the cursor, so `l`
opens it in the reader. Because the summary lands beside the *first* file
you marked, that folder may not be the one you are looking at; when it is
not, the listing moves there and says so - `listing moved to <path>`.

The summary is written **beside the first file you marked**, as
`<first-stem>-summary.md`. An existing file of that name is never
overwritten: the run falls back to `<first-stem>-summary-<stamp>.md`.
The provider is asked to write that one path and nothing else; if it
exits cleanly having printed the summary instead of writing it, that
stdout is saved there.
Filecraft reserves the path before starting the provider. If the run ends
with that file still empty - it failed, or you terminated it - the
reservation is filled with a short Markdown failure note carrying the same
reason shown in the message log. A summary the provider had already
written is never replaced by a note.

### Watching the run

`L`, `:log`, or `:job` opens the run's own output over the listing -
stdout and stderr as the provider prints them, not in one lump at the
end. It keeps working after the run has finished, which is usually when
you want it.

```
┌ job log: codex ────────────────────────────────────────────────────────────┐
│ codex · thinking · 42 lines                                                │
│ session 01a04eef-d4a6-7232-831f-e8faf5c42241 · resume: codex resume 01a0…  │
│    39 | reading /docs/report.pdf                                           │
│    40 | reading /docs/notes.md                                             │
│    41 ! warning: large attachment                                          │
│    42 | writing /docs/report-summary.md                                    │
└──────────────────────────────────────────────────── line 39 of 42 · 100% ──┘
```

| Key | Action (log viewer) |
| --- | --- |
| `j` / `k`, Down / Up | scroll one line |
| `d` / `u` | scroll half a page |
| `f` / `b`, PgDn / PgUp | scroll a page |
| `g` / `G`, Home / End | top / bottom |
| `/` | find in the log (type, then Enter) |
| `n` / `N` | next / previous match |
| `h`, `q`, Esc | back to the listing - **the run keeps going** |

New output pulls the view down while you are at the bottom, and stops
the moment you scroll up, so you can read something without fighting the
stream. `G` starts it following again. Every line carries its number and
which stream it came from: `|` is stdout, `!` is stderr. The log keeps
the most recent 4000 lines and says so when it has dropped any.

The first header row says what the run is doing - `waiting for output`,
`thinking`, `streaming`, `finished`. The second names the session the
provider announced and the command that reopens it. `codex exec` prints
`session id: <uuid>` in its banner; a provider that announces nothing
says `session: not reported`, rather than offering a command that would
not work.

### Reopening the session in the provider

Every summary is signed with the run that produced it - a written
summary, a summary saved from stdout, and a failure note alike:

```markdown
> Provider: codex | Session: 01a04eef-… | Resume with: codex resume 01a04eef-…
```

Filecraft never runs that command. It is printed so you can pick the
conversation back up in the CLI itself, which is where you can ask it
follow-up questions. The reopen flag is per-CLI and is *not* uniform -
only two of the five call it `--resume`:

| provider | reopen a session |
| --- | --- |
| `ag` | `agy --conversation <id>` |
| `cc` | `claude --resume <id>` |
| `co` | `codex resume <id>` (a subcommand, not a flag) |
| `gk` | `grok --resume <id>` |
| `ki` | `kimi --session <id>` |

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
- The log viewer only reads. Closing it never stops a run, no key in it
  starts or resumes one, and the resume command is printed for you to
  run - never run by Filecraft.
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

A file the reader cannot draw is not refused: it is handed to the macOS
default application instead (see [Opening in the desktop](#opening-in-the-desktop)).
Broken symlinks and special files are still refused in a message. Files
are shown up to 1 MiB and 20,000 lines, and a truncated file says so on
its last line.

## Opening in the desktop

`l` (or Right) on a file the reader cannot draw hands it to
`/usr/bin/open`, which starts whatever application macOS has registered
for that format - Preview for a PDF or an image, QuickTime for a video.
`:open` is the same operation typed out,
on any entry.

Two rules decide, in this order:

1. **The name, for safe formats that are never text.** `pdf`, raster images
   (`png jpg jpeg gif webp heic bmp tiff ico`), audio
   (`mp3 wav flac aac m4a ogg aiff`), video
   (`mp4 mov mkv avi webm m4v`) go straight to the desktop
   without the file being opened at all. This is the same extension
   table the [`kind` column](#columns) draws from. Deciding on the name
   matters: a small PDF can carry no NUL byte in its first 8 KiB, and
   sniffing alone would have called it text and painted it as mojibake.
2. **The bytes, for everything else.** Any other file is read, and if
   what comes back is not text it goes the same way. That is the only
   answer available for an extension Filecraft does not know.

Everything that is text - Markdown, plain text, source, config, an
unknown extension holding readable bytes, or an executable script - still
opens in the reader. SVG stays in the reader because it is text. Archives,
known binary kinds, and executable binary files are refused only when they
would otherwise reach the desktop because their default handlers can extract
files, mount volumes, or run code. The refusal points to the explicit
`:open` command when that is what the user intends.

The application is spawned **detached**: Filecraft does not wait for it,
does not give up the terminal, and does not redraw around it. The
message log says `open: opened 'report.pdf' with the macOS default
application` and the listing stays on the same row. Nothing is written:
the path is handed over and that is all.

`open` is macOS-only. On other platforms `l` on such a file reports that,
in the screen language, and does nothing - the same refusal `:open` has
always given there.

## Bearings

The screen has an orientation half and an operating half, and the
boundary is a rule: **everything above the listing is read-only**. It
cannot be focused, and no key that starts there changes anything. The
listing is the single thing commands act on.

```
╔ ░▒▓ FILECRAFT v0.1.0 ▓▒░ ════════════════════════════════════════════════════╗
║ 0·~ ▸ … ▸ 7·final ▸ 8·assets                              depth 8 · 73 items ║
║   NAME                                                          SIZE MODIFIED║
║──────────────────────────────────────────────────────────────────────────────║
║│  file_059.txt                                                     0B 1h     ║
║█  file_060.txt                                                     0B 1h     ║
║█> file_061.txt                                                     0B 2d     ║
║ row 61 of 74 · rows 47-61 of 74 · file_061.txt · file · 0B · 2d ago          ║
```

- **Column header** - the names of the columns the listing is drawing,
  and a rule under them. Read-only chrome, like the ladder; which
  columns it names is yours to choose ([Columns](#columns)), and
  `:header off` takes the two rows back.
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
  to the filename. Absolute times stay in `preview`. In Traditional
  Chinese the same column reads `2天前`, `11分鐘前`, `1小時前`, and it is
  two cells wider because a Han character is two cells wide.

The same row in Traditional Chinese:

```
║ 0·~ ▸ … ▸ 4·docs ▸ 5·notes                                  階層 5 · 74 個項目 ║
║   名稱                                                          大小 修改時間 ║
║───────────────────────────────────────────────────────────────────────────────║
║█> file_061.txt                                                    0B 2天前    ║
║ 第 61 列，共 74 列 · 第 47-61 列，共 74 列 · file_061.txt · 檔案 · 0B · 2天前  ║
```

## Commands

Typed at the `:` prompt. Parsed directly: no shell, no globbing, no
`$VAR` expansion. Quote names that contain spaces.

| Command | Action |
| --- | --- |
| `cd [path]` | change directory (`~` is home; no argument also goes home) |
| `move [destination]` | move the selected entry (asks `y/n`, never overwrites). No path opens a folder picker; a typed path still goes straight to confirm |
| `rename <new-name>` | rename the selected entry (asks `y/n`, never overwrites) |
| `delete`, `trash` | move the selected entry to the macOS Trash (asks `y/n`; recoverable) |
| `open` | hand the selected entry to macOS `open` - the same thing `l` does on a PDF or an image |
| `edit` | edit the selected regular file in `$EDITOR` or `nvim` |
| `preview` | read-only preview (Neovim if available, else built-in) |
| `summarize`, `summary` | AI summary of files you pick (same as `S`) |
| `log`, `job` | the AI run's own output and session (same as `L`) |
| `agent [...]` | future AI seam; disabled in v0 (see [docs/agent-seam.md](docs/agent-seam.md)) |
| `lang [en\|zh]`, `language` | screen language; no code reports the current one. The choice is saved for next time |
| `columns [list]`, `cols` | [listing columns](#columns); no list opens the picker. The choice is saved for next time |
| `header on\|off` | the column header row above the listing; no word reports whether it is drawn |
| `set <key>=<value>` | `columns=<list>` or `header=on\|off` - a second spelling of the two above |
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
- `l` on a PDF, image, or video starts the macOS default application for
  it and nothing more: `/usr/bin/open` is given the path, the file is
  never written, and its permissions are never changed.

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
