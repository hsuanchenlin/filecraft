# Slice 1 - "Bearings"

Release note for the Locus navigation direction's first slice. The design
argument behind it is the `filecraft-own-navigator-followup` scout report;
this file records what actually shipped and what changed for users.

## What it adds

All of it is **orientation chrome around the same single listing**. There
is still exactly one operating position - the cursor's entry in `cwd` -
and every command still acts on that and nothing else.

| Element | Answers | Cost |
| --- | --- | --- |
| **Ladder** - numbered, jumpable ancestor chain replacing the raw path line | *where am I, and how do I get back up?* | pure render + digit keys reusing `NavState::change_dir` |
| **Rail** - proportional position gutter on the listing | *how big is here, and where am I in it?* | pure render |
| **Speakable status** - the whole position in words, on a fixed row | *what is selected, and what is off screen?* | pure render |
| **Relative times** - `2d`, `11m`, `1h` in place of a UTC stamp | *how fresh is this?* | pure render |

**No new filesystem reads.** Every element is a function of state already
in memory: the current path, the listing snapshot, the cursor, and the
viewport geometry. Nothing here stats a file, walks a tree, or spawns a
process.

## Changed keys

Two documented bindings changed, on an explicit captain decision:

| Key | Before | Now |
| --- | --- | --- |
| `l` | went to the **parent** directory | **enters** the selected directory, like Right and Enter |
| Esc (browse mode) | **quit** the application | **backs out one level**: clears an active filter, otherwise does nothing |

Quitting is `q` and Ctrl-C. `h`, Left, and Backspace still go up. The
`?` help and the README keyboard table ship with the change, and both
behaviors have regression tests.

New keys, all previously unbound: `0`-`9` jump to that ancestor on the
ladder, and `M` opens the message history.

## Fixed

- The hint row no longer truncates mid-word at the documented 80x24
  minimum; it drops whole hints instead, and the frame is asserted at
  80x24, 100x30, 132x40, and 60x20.
- A deep path no longer clips its own identity. The ladder middle-elides,
  so the anchor and the current directory both survive.
- The 100-line message ring is reachable with `M`; before, 97 of 100
  messages were unreachable state.
- A filter that matches nothing now says `(no entries match 'x')` in the
  listing and `filter 'x': 0 of N match` in the status. The `..` row
  always passes the filter, so before this the screen showed a bare `../`
  and claimed `[1/1]`.
- The listing keeps a three-row scroll margin, so descending shows what is
  coming instead of pinning the cursor to the bottom edge.

## Accessibility

- Every graphic has a textual dual: rail ↔ `rows A-B of N`, ladder ↔
  `depth N`, kind ↔ `/ @ @!`, level ↔ `ok: err:`. Nothing new is carried
  by color or shape alone, and this is asserted under `NO_COLOR`.
- `FILECRAFT_ASCII=1` draws the whole screen in printable ASCII. A test
  asserts no character outside U+0020-U+007E reaches the frame.
- Wide (CJK) characters still keep the columns aligned, asserted by
  comparing the size column's cell position on an ASCII row and a CJK row.

## Unchanged, deliberately

No delete. No overwrite. No second operating pane. `selected_operand`
still rejects `..`, move and rename still route through the confirmation
prompt with the canonical destination, no new `Effect` variant and no new
process-spawn site exist, and the agent seam is still inert. A property
test drives every printable key plus every non-character key against a
fixture and asserts the tree - names, sizes, and mtimes - is untouched.

## Not in this slice

Lookahead (the bounded peek pane), directory previews, trail and marks,
local-time timestamps, `:map`, `:find`, and anything resembling a Miller
column UI. Those are later design candidates, not authorized work.
