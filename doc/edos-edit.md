# edos-edit

The graphical text editor. A window with a file tree, tabs, and one editor pane,
in the shape a person who uses VS Code already knows: click a file, type into it,
Ctrl+S.

It does not replace `edos-vi`. vi is the only editor that works over ssh and on a
serial console, and `sshd` exists so the machine can be worked on remotely, so
both ship. The split is by transport, not by preference: vi owns the PTY,
edos-edit owns the window.

## What it is not

No language server, no completion, no diagnostics, no build integration, no
terminal panel, no split panes, no minimap, no breadcrumb bar, no extensions.
Syntax coloring is at vim's level -- a per-line tokenizer over a keyword table,
not a grammar.

Those are omissions, not deferrals with a plan attached. The one that has a
sketch is the terminal panel: `edos_render::widgets::terminal` already exists and
`edos-terminal` already drives a PTY, so the panel is a later phase rather than a
rewrite.

## The type rule

**Monospaced inside the editor pane, proportional everywhere else.** One
boundary, no exceptions: the document is a character grid and is set in
`Family::Mono`; the tree, the tabs and the status bar are things the program says
*about* the document and are set in `Family::Sans`. Line numbers are inside the
pane and are therefore mono, which is also what makes them right-align on the
digit grid.

Measure with `widgets::text_width` for anything proportional. Inside the pane,
`char_width()` is the cell and multiplying is correct, because that face really
is fixed-advance.

**Draw the pane one character at a time**, at `text_x + col * char_width()`, the
way `widgets::terminal` already draws its grid. Not one call per token run, which
is the obvious optimisation and is wrong here:

`Mono-Regular.ttf` is `unitsPerEm = 1000` with an advance of `600`, so a cell at
`size::BODY` (14) is **8.4px**, while `char_width()` is `text::width("M", mono)`
and ceils to **9**. A run drawn as one string advances the pen by 8.4 per
character while every rectangle in `view.rs` steps by 9, so the text slides left
of its own gutter, caret, selection and indent guides by 0.6px per character --
36px across a 60-column line. It is invisible on the short strings a first test
types, which is what makes it worth writing down.

The advance is exactly 9.0 at `size::TITLE` (15), so setting the pane one step up
the scale would make runs safe. That is not the trade: TITLE is the heading step,
and setting code at a heading size to dodge a rounding error is worse than the
per-character loop the terminal already pays for.

## Palette

The shell is already Ayu Dark. Ayu is a code-editor theme, so the editor does not
get a new palette: it gets the half of the one already in use that the shell has
never had a reason to draw. A directory in the sidebar and a type name in the
code are the same blue on purpose.

Ten of these hues are new to `Theme`; four are values already in it, marked below
so the shared ones are visible rather than accidental.

| Field | Hex | Used for |
| --- | --- | --- |
| `syn_keyword` | `#FF8F40` | `fn`, `if`, `pub`, `return` |
| `syn_string` | `#AAD94C` | string and char literals |
| `syn_number` | `#D2A6FF` | numeric literals, `true`, `false`, `null` (= `entry_special`) |
| `syn_type` | `#59C2FF` | type names, TOML section headers, markdown headings (= `entry_dir`) |
| `syn_function` | `#FFB454` | a name immediately followed by `(` |
| `syn_comment` | `#626A73` | line and block comments |
| `syn_operator` | `#F29668` | `+ - = < > & \| !` and friends |
| `syn_punct` | `#6C7380` | `( ) { } [ ] , ; :` (= `title_text_inactive`) |
| `syn_special` | `#E6B673` | escapes inside a string, `#[attributes]`, `$VAR` |
| `editor_line_highlight` | `#131721` | fill behind the line the cursor is on |
| `editor_gutter` | `#3D4550` | line numbers other than the current one |
| `editor_indent_guide` | `#1E242E` | one hairline per indent level |
| `editor_selection` | `#18324F` | fill behind selected text |
| `editor_change` | `#E6B450` | the change ribbon, and the tab's unsaved dot (= the accent) |

The current line's own number is drawn in `text_primary`, not `editor_gutter`, so
the gutter reads as a scale with one bright mark on it.

`editor_change` is the accent at full strength rather than dimmed. The accent
means "this is the live thing here" everywhere else in the shell -- the focus
ring, the focused window's hairline, the focused taskbar button's underline -- and
a line you changed is exactly that. It is two pixels wide and appears only on
lines you touched, so it does not compete with a control that wants clicking.

## Layout

```
┌───────────────────┬──────────────────────────────────────────────┐
│ EDOS-V2       ⌄   │  main.rs  ●  │  Cargo.toml  ×  │             │  tabs 32px
│  ▾ programs       ├──────────────────────────────────────────────┤
│    ▾ edos-edit    │  1  │ use std::fs;                           │
│        main.rs    │  2  │ ▏                                      │
│        buffer.rs  │  3  │ fn main() {                            │
│      edos-files   │▌ 4  │ ┆   let path = "/etc/motd";            │
│  ▸ doc            │▌ 5  │ ┆   fs::write(path, "hi")?;            │
│    README.md      │  6  │ }                                      │
│                   │     │                                        │
│                   ├──────────────────────────────────────────────┤
│                   │ Find  ┃fs::write              ┃      3 of 17  │  prompt 48px
├───────────────────┴──────────────────────────────────────────────┤
│ main.rs   Rust   Ln 5, Col 24   4 spaces   UTF-8   efs · dev0p2   │  status 24px
└──────────────────────────────────────────────────────────────────┘
  sidebar 224px      ▌ = change ribbon    ┆ = indent guide
```

The prompt bar is one bar, not three. Find, go-to-line and open all use it,
labelled `Find` / `Line` / `Open`, and it exists only while one of them is open.
It cannot live in the status strip: that strip is `space(6)` = 24 tall and a
`TextInput` is `CONTROL_HEIGHT` = 32, so the field would not fit inside it. The
status strip keeps reporting while a prompt is open.

The volume on the right of the status strip is what `edos_lib::mounts` can
actually say -- `Filesystem::name` and `device_label`, which produce `efs` and
`dev0p2`. Lower case, because that is how `mount` and `df` print the same name;
an editor is not the place to start spelling a filesystem differently from the
rest of the system. Nothing in userspace maps a device id to an `sd*` name
either, and this program is not the place to invent one.

Every dimension comes from `metrics`, none is a literal:

| Thing | Value | Why |
| --- | --- | --- |
| sidebar width | `space(56)` = 224 | wide enough for a nested name at body size |
| tab strip height | `CONTROL_HEIGHT` = 32 | a tab is a control, and sits on the shared row rhythm |
| status bar height | `space(6)` = 24 | shorter than a control, because nothing in it is clicked |
| row height (tree, tabs) | `CONTROL_HEIGHT` | one rhythm for every list in the shell |
| editor line height | `text::line_height(Style::mono(BODY))` | from the face, so descenders are not clipped |
| gutter width | measured from the digit count of the last line | a 5-digit file gets a wider gutter; nothing is reserved for digits that do not exist |
| tree indent per level | `space(3)` = 12 | |
| change ribbon | 2px, between gutter and text | |
| prompt bar height | `CONTROL_HEIGHT + space(2) * 2` = 48 | a field with the shell's standard breathing room around it |

The sidebar collapses with Ctrl+B, and drops itself below a window width of 640
the way `edos-files` drops its details pane: squeezing a tree to nothing is worse
than not showing it.

## The change ribbon

The one thing this editor has that a plain text box does not. A two-pixel column
between the gutter and the text, drawn in the accent on every line that differs
from what was read off the disk.

It is sourced from the edit log, not from version control, because this machine
has none: it is the only diff available here. It answers "what did I just do to
this file" before a save, which on a system where a bad `/etc` edit costs a boot
is the question worth answering.

A line carries its own flag, so inserting and deleting lines moves the marks with
them for free. Saving clears every flag, which is the whole meaning of the mark:
it tracks distance from the disk, not from the start of the session.

A `bool` per line cannot tell on its own that an undo has removed that distance,
so the buffer also remembers the log position it was last saved at. Whenever the
log returns to that position the buffer *is* the file on disk, and every flag
clears. That covers the case people actually hit -- undoing back past the last
save -- exactly. Undoing part of the way back still leaves a mark on a line that
now happens to match the disk again, and it is left over-reporting rather than
carrying a second copy of the file to be sure: a ribbon that says "look here" one
line too often is the harmless direction to be wrong in.

The same fact appears once more, at file granularity, as a dot in place of the
tab's close button. Hovering the tab turns the dot back into the ×, so the
control is always reachable and the state is visible the rest of the time.

## Buffer

```rust
struct Line {
    text: String,
    /// Differs from what was read off the disk.
    changed: bool,
    /// Tokens, once anyone has asked. Cleared when the line is edited.
    tokens: Option<Vec<Token>>,
    /// Whether this line begins inside a block comment.
    opens_in_block: bool,
}
```

A `Vec<Line>`, not a rope. The cost of a line vector is on inserting and deleting
*lines*, which moves pointers rather than text, and this editor is comfortable to
a few megabytes -- past that a rope is the answer and this is the wrong program.

The cursor is `(line, column)` in **characters**, never bytes. `text_input`
carries the same rule and says why: a byte index lands inside a multi-byte
character and the next `String::insert` panics on the boundary assertion. Byte
offsets are derived where they are used.

Files are read as UTF-8 with lossy replacement rather than refused, so a stray
byte in a config file can still be fixed rather than only reported. The status
bar says `UTF-8` for a clean file and `UTF-8 (repaired)` for one that needed it,
and a repaired file writes back what is on screen.

Line endings are detected on open and preserved on save. A file with no trailing
newline keeps it that way; a file with one keeps that too.

A file over 8 MiB is refused, with a status line naming its size, because
`Buffer::open` decodes and splits the whole thing before the window redraws and
there is nothing on screen to say why it stopped responding. Eight is the limit
`edos-files` already applies to previews, for the same reason.

## Undo

An operation log, not snapshots. A snapshot of a 5,000-line file is ~200 KB and a
hundred of them is 20 MB for nothing.

```rust
enum Edit {
    Insert  { at: Position, text: String },
    Delete  { at: Position, text: String },
    Replace { at: Position, old: String, new: String },
}
```

`Insert` and `Delete` are each other's inverse with the variant swapped;
`Replace` inverts by swapping `old` and `new`. So undo is `apply(invert(e))`,
redo is `apply(e)`, and there is one code path rather than two that can drift.

`Replace` exists because typing or pasting over a selection is one action to the
person doing it. Expressed as a `Delete` followed by an `Insert` it would take
two Ctrl+Z presses to put back, and the second one would look like the editor
undoing something the user never did.

Consecutive single-character inserts coalesce into one entry until the run is
broken by a cursor move, a newline, a delete, or a save, which is what makes
Ctrl+Z undo a word rather than a letter.

The log also carries the cursor position from *before* the edit, because an undo
that leaves the cursor somewhere else makes the next keystroke land in the wrong
place.

## Syntax

One tokenizer over a table. Adding a language is a table entry, not code.

```rust
struct Lang {
    name: &'static str,
    exts: &'static [&'static str],
    /// Matched against the whole file name, for files with no extension.
    names: &'static [&'static str],
    keywords: &'static [&'static str],
    /// Names colored as types. Empty means "a leading capital is a type".
    types: &'static [&'static str],
    line_comment: Option<&'static str>,
    block_comment: Option<(&'static str, &'static str)>,
    strings: &'static [char],
    /// What Tab inserts, and how wide an indent guide step is.
    indent: Indent,
    flavour: Flavour,
}

enum Flavour { CLike, Markdown, Plain }
```

Shipping: Rust, C, TOML, JSON, shell, Markdown, plain. `Flavour` exists because
Markdown is not keyword-shaped -- headings, emphasis, code spans and links need
their own small pass -- and pretending otherwise would put the wrong colour on
every line of `doc/`. That pass draws from the same table as everything else:
heading `syn_type`, code span `syn_string`, link text `syn_function`, link target
`syn_special`, emphasis `syn_keyword`. No colour enters the program that is not
already in the palette above.

Tokenizing is per line and cached on the line. An edit clears that line's cache;
if the edit changes whether the line *ends* inside a block comment, the clear
walks forward until the carried state matches again, which is what keeps a `/*`
typed at the top of a file from leaving the rest of it colored as code.

## Keys

VS Code's bindings where VS Code has one, because the point is that no one has to
learn this editor.

| Key | Does |
| --- | --- |
| Ctrl+S | Save |
| Ctrl+O | Open the path in the status bar's prompt |
| Ctrl+N | New file |
| Ctrl+W | Close tab, refusing once if it has unsaved changes |
| Ctrl+Tab | Next tab |
| Ctrl+B | Show or hide the sidebar |
| Ctrl+F | Find; Enter and Shift+Enter walk the matches |
| Ctrl+G | Go to line |
| Ctrl+Z / Ctrl+Y | Undo / redo |
| Ctrl+A | Select all |
| Ctrl+C / X / V | Copy, cut, paste through the kernel clipboard |
| Ctrl+Home / End | Top and bottom of the file |
| Arrows, Home, End, PgUp, PgDn | Move; with Shift, extend the selection |
| Tab | Insert the language's indent |
| Esc | Dismiss the find bar or the prompt |

Two traps that already cost a session on `edos-files`, and apply unchanged here:

- **Window key events carry `pc_keyboard` KeyCode values.** Not PS/2 scancodes,
  and Character events do not arrive. Translate with
  `edos_lib::keymap::{map_keycode, update_modifiers}`, which is also what honours
  the runtime `/etc/keymap` layout.
- **A shortcut that opens a field must not also be typed into it.** Capture the
  mode *before* dispatching the key, and never return early from a KeyPress that
  a focused field still needs.

## Find

The find bar is a real `edos_render::widgets::TextInput` in a `WidgetContainer`,
not a hand-rolled field. Being its third consumer is deliberate: writing the
second one found two defects that had been in that widget since it was written,
both invisible to the first.

Matches are literal and case-insensitive, highlighted across the visible range,
with the current one in the accent. The bar reports `3 of 17` or `No matches`;
"no matches" is a count, not an error, so it is not drawn in the warning colour.

## Files

```
programs/edos-edit/
  src/main.rs      event loop, key dispatch, commands, app state
  src/buffer.rs    Line, Buffer, Cursor, Selection, the edit log
  src/syntax.rs    the Lang table, the tokenizer, TokenKind -> colour
  src/tree.rs      the sidebar's model: lazy expansion, one node per row
  src/view.rs      geometry as pure functions, and the drawing that reads them
```

`view.rs` holds the geometry as pure functions that both the drawing and the
hit-testing call, so what the pointer hits and what the eye sees come from one
description. This is what `edos-files` does and the reason its rows and its
inline rename field land on each other exactly.

The editor pane is not a `Widget`. The trait is built for controls a container
owns and focuses, and the document belongs to the app -- save, undo and find all
need it. The rule that everything drawn goes through `edos_render` is satisfied by
drawing through `text`, `theme` and `widgets`' primitives, which it does.

In `edos_render`: the fourteen theme fields above, and `icons::DOCUMENT` for the
taskbar menu and the tree's leaf rows.

The tree's expand and collapse chevrons are `icons::MINIMIZE` and
`icons::CHEVRON_RIGHT`, not the characters `▾` and `▸`. `Sans-Regular.ttf` has
no U+25BE or U+25B8, and `font::glyph` returns `None` for a character the face
does not carry, so setting them as text draws nothing at all and leaves a tree
whose rows cannot be told apart.

## Reaching it

- Applications menu: a `Row` in `edos-taskbar`'s `ROWS`, labelled "Editor".
- Desktop right-click menu, in `edos-wm`, beside the entry for `edos-files`.
- `edos-files` opens a text file in `/bin/edos-edit` on double-click, beside the
  existing `.bmp`/`.svg` route to `imgview`.
- `edos-edit [path]` from a shell. No path opens an empty buffer rooted at the
  working directory.

## Verifying it

By driving the guest, which is what caught three of `edos-files`' bugs:
`make run-headless`, then `scripts/edos-vm click` to focus and
`scripts/edos-vm type` to edit, reading `scripts/edos-vm shot` between steps.
Read `doc/vm-control.md` first.

The check that matters is not that the window draws. It is that a file edited in
the guest and saved reads back byte-for-byte as intended **on a later boot** --
the page cache returns what was never written, so a same-boot read proves
nothing.
