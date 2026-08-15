# A browser for EDOS

`programs/edos-web`: what to build, in what order, and why almost all of the
hard parts are already in the tree.

## The finding this starts from

Driving the guest as a user on 2026-08-15: it fetched `https://edos.edgl.dev`
over TLS with `wget`, wrote the HTML to `/tmp/p.html`, and **had nothing that
could display it**. The applications menu is five entries — Terminal, Files,
Editor, Packages, Widgets — four of them developer tools.

That is the largest user-facing gap in the system, and it is a smaller gap than
it looks.

## What already exists

None of this needs building:

| Piece | Where |
|---|---|
| HTTPS fetch | `programs/edos_http` — HTTP/1.1 over `rustls`, verified against a real site from inside the guest |
| 2D rasteriser | `tiny-skia`, already vendored via `resvg`: paths, fills, strokes, gradients, blending |
| Glyphs | `fontdue` + `ttf-parser` in `edos_render::font`, proportional faces at arbitrary sizes, one text blitter |
| Images | `resvg`/`usvg` for SVG, `png`/`image-webp`/`zune-jpeg` for raster, all wired into `imgview` and the file manager's thumbnails too |
| Window, input, scrolling | `edos_render`'s widgets and `window.rs` |

Missing: **an HTML parser, a CSS subset, and layout.** That is the whole list.

## Stages, each usable on its own

### Stage 1 — a reader

Fetch, parse with `html5ever` (pure Rust, wants only `std`), walk the DOM, lay
out headings, paragraphs, lists, links and images as blocks, scroll, follow a
link. No CSS, no JS.

Done when EDOS can read its own documentation at `edos.edgl.dev`. That is the
demo, and it is a real one: the site is Starlight-generated HTML with no
hand-holding for us.

Where it stands: fetch, parse and the block list are in `doc.rs`; the text
rendering is in `text.rs` behind `-d`; the window, its toolbar and its scrolling
are in `ui.rs`, the layout that feeds it is in `view.rs`, and the loading of a
page is in `net.rs`. Driving the guest renders `edos.edgl.dev` in a window with
its headings at the right sizes, its links underlined and its lists marked.

The stage is closed. What it took beyond the list above:

- **A click follows a link**, hit-tested against the fragment boxes, with a
  history stack behind Back and a forward stack behind it, each holding the
  document it parsed rather than a URL to fetch again. **The space between two
  words of one link is part of it**: a fragment is a word, so hit-testing only
  the words leaves a dead gap between every pair of them, and the reader sees
  one underlined phrase and a click that did nothing.
- **A fragment is a scroll, not a fetch.** Every heading link and every table
  of contents on a documentation site names a place in the page it is already
  on. `doc.rs` records the `id` of the element that opened each block, `view.rs`
  records where each one landed, and `ui.rs` compares the target's page against
  the address on screen: same page, scroll; anything else, load. A link to the
  page's own address carrying no fragment is a reload, which is what a browser
  does with one.
- **An address bar that can be typed into**, which is `edos_render`'s
  `TextInput` in the toolbar beside Back, Forward and Reload. Ctrl+L empties it
  and takes the caret, since a field with no selection can either replace what
  it holds or keep it, and replacing is what the shortcut is reached for;
  clicking into it keeps the address, which is what editing one asks for.

### Loading is not a pause

`edos_http` is blocking and a page is not one fetch: the document, then every
stylesheet and image it refers to, each with its own connection. Done between
two frames that left the window unable to redraw, scroll or close for as long
as the slowest server took.

`net.rs` runs a load on a thread of its own and posts what it is doing back to
the window, which is why `doc.rs`'s shared parts are `Arc` and its subresource
cache is a `Mutex`: a whole `Document` crosses the boundary when the page is
built. One load at a time, and a second abandons the first by ticket rather
than by stopping the thread, because a thread inside a TLS handshake cannot be
interrupted -- it finishes into a channel nobody is listening to.

**The page gives way to the loading view the moment the load starts.** Leaving
the old page up until the new one is ready reads as a click that was ignored.
What stands in its place says what is on the wire right now, how many
subresources have arrived and how long it has been, with an indeterminate band
rather than a bar: nothing knows how many resources a page refers to until it
has been parsed, and parsing needs the document that is still on its way. Esc
and the toolbar's stop button abandon it and leave the page that was there.

### Reader mode, and the reason for it

`position: fixed` and `position: sticky` are not implemented, so a sidebar a
real browser pins beside the article lands above it in the flow: a Starlight
page opened at its top is a screen and a half of navigation links before the
first paragraph. That is measured, not supposed -- `edos.edgl.dev/introduction/`
is 72 blocks whole and 42 from `<main>` alone.

So the window lays out the `<main>` the page marked, and `m` or the toolbar's
document button switches to the whole document. The walk enters the chain of
ancestors down to `<main>` rather than starting there, so the cascade keeps the
ancestors a selector matches against and the custom properties `:root` declares
-- starting at `<main>` drops a modern page's whole palette. `<head>` is walked
whatever else is skipped, since the title is in it.

`-a` does the same for `-d`.

One CSS limit was worth answering in the cascade rather than in layout: a box
declared 1x1 with its overflow hidden is the visually-hidden idiom every
accessible site uses to carry text for a screen reader alone, and nothing here
clips, so every heading on the site read "What runs today" followed by "Section
titled What runs today". A box that small can show nothing whatever its
overflow says, so the size answers it.

### Stage 2 — a CSS subset

Colours, font families and sizes, margins, padding, borders — the box model.
Turns "readable" into "looks close to right".

Where it stands: `css.rs` is the cascade. It reads every `<style>` element,
every `<link rel=stylesheet>` the document fetches, and every `style=`
attribute; matches selectors that are comma-separated chains of
tag/`.class`/`#id`/`*`/`[attr]`/`:nth-child()`/`:not()` compounds joined by the
descendant, child (`>`) or sibling (`+`, `~`) combinators; orders them by real specificity with the inline
attribute winning; and computes `color`, `font-size` (px, pt, em, rem, `%` and
the absolute keywords), `font-weight`, `font-style`, `font-family`'s
monospace-or-not, `text-decoration` (`underline`, `line-through`, `overline`
and `none`, in the shorthand or in `text-decoration-line`), `text-align` (with
`justify` set flush left, since the blitter cannot stretch a line), `line-height` (a factor, a
length or `normal`), `text-transform`, `text-indent`, `white-space`,
`word-break`/`overflow-wrap`, `letter-spacing`/`word-spacing`,
`list-style-type` (and the type keyword in the
`list-style` shorthand), all four margins,
the measure a box asks for with `width`/`max-width`/`min-width`, the height it asks for
with `height`/`min-height`/`max-height`, the box it paints for
itself with `background-color`, `padding` and `border` (both shorthands and the
per-edge longhands, on a block or on a run), `display` and `visibility`.
`doc.rs` carries the computed style onto every `Run` and every
`Block`, and `view.rs` lets it override the plan the tag alone implies — where
it says nothing, the reader typography stands.

The pieces below exist because a stylesheet written this decade is unreadable
without them, and each is small:

- **Custom properties and `var()`.** `Vars` is the scope, an `Rc<BTreeMap>`
  shared down the tree and cloned only by an element that declares something.
  Substitution happens after the cascade, not as each declaration is read,
  which is what the spec means by resolving at computed-value time: a `--x`
  written by any rule matching the element is in scope for every declaration on
  it, whatever the source order or specificity. A `var()` naming nothing falls
  back to its second argument, and with no fallback the whole declaration is
  dropped, leaving the inherited value — never the property's initial one.
  Cycles stop at a depth of 8.
- **Attribute selectors.** `[attr]`, `[attr=v]` and the four substring forms
  plus `~=` and `|=`, with the `i` flag folding the value. They count at the
  class level of specificity. A quoted value may hold a space or a `>`, so the
  selector is tokenized with bracket depth and quote state rather than split on
  whitespace.
- **`:root`.** Rewritten to `html`, because in an HTML document that is what it
  is and nothing else, and it is where a sheet declares its palette.
- **Structural pseudo-classes.** `:nth-child`, `:nth-last-child`,
  `:nth-of-type` and `:nth-last-of-type` with the full `An+B` microsyntax
  (`odd`, `even`, a bare integer, `-n+3`), plus `:first-child`, `:last-child`,
  `:only-child` and their `-of-type` forms. `doc.rs` gives every element a
  `Siblings`: its 1-based position among its element siblings, how many there
  are, and the same pair counted over the siblings sharing its tag. An element
  the walk itself skips — a `<style>` — still counts, since `:nth-child` asks
  about the document rather than about what was rendered. They count at the
  class level of specificity, and a `(...)` is opaque to the selector
  tokenizer so `:nth-child(2n + 1)` stays one compound.
- **Sibling combinators.** `+` and `~`. A selector is matched right to left
  along two axes at once: the ancestors still open above the subject, and the
  siblings standing before it. A sibling combinator moves along the second and
  leaves the first alone, since siblings share every ancestor, so `div > h2 ~ p`
  searches the *parent's* row once the `>` step has landed on it. `doc.rs`
  hands every element the whole row of its element siblings in one `Rc`, which
  costs one copy per parent rather than a growing prefix per child; the entries
  in that row carry an empty row of their own, since a chain like `p + p + p`
  keeps walking the subject's row and never asks a sibling for its siblings.
- **Logical combinators.** `:not()`, `:is()` and `:where()` over a
  comma-separated list of compounds. The test is the same one in all three —
  does any argument match — with `:not()` inverting the answer. Specificity is
  the heaviest argument's, not a sum and not a class each: `:where()` weighs
  nothing at all, which is the whole reason a sheet reaches for it. An argument
  carrying a combinator is not read, and dropping the rule is the safe answer
  rather than the conservative-looking one — a `:not()` matched without its
  argument matches *more* elements, not fewer, so guessing would restyle the
  document. Nesting stops at `MAX_SELECTOR_NESTING`, since the parser recurses
  through the argument. The selector list is split on commas outside brackets,
  parentheses and quotes, which a plain `split(',')` got wrong for
  `[title="a,b"]` as well.
- **`display` names the box.** The element decides what box it opens only
  where the page said nothing. `block`, `inline` and `list-item` are honoured
  as themselves; every layout mode this cannot lay out is reduced to the outer
  box it is (`flex`, `grid`, `table` and the table-part values block;
  `inline-block`, `inline-flex` and `table-cell` inline). `contents` is inline
  too, which is the right answer here rather than an approximation: it asks for
  the box to be dropped and the children kept, and an inline box in a flat
  inline model opens nothing. A two-keyword value is answered by the first
  keyword that names something, which is the outer type in every ordering
  css-display-3 §2 allows, and an unrecognised keyword leaves the declaration
  invalid so the element keeps its own box. The marker follows `list-item`
  rather than the `<li>` tag, so `li { display: block }` loses its bullet the
  way a browser does, and a `<span>` may gain one.
- **`visibility` is not `display`.** `display: none` is `Computed::hidden` and
  drops the element and its subtree in `doc.rs` before a block opens;
  `visibility: hidden` is `Computed::invisible`, which lays the box out exactly
  as it would have been and paints none of it, per css-display-3 §3. It is read
  where `view.rs` emits rather than where it walks: a `Fragment` and a `Line`
  each carry `hidden`, the draw pass steps over them, `link_at` steps over a
  hidden fragment so a hidden link cannot be clicked, and a block whose plan is
  invisible pushes no `Decor`. Unlike `hidden` it inherits, so a child setting
  `visibility: visible` inside a hidden parent is painted and is the only thing
  that parent draws. `collapse` is answered as `hidden`, which is what it means
  outside a table.
- **`min-width` floors the measure, and the floor wins.** `width` and
  `max-width` fold into one `Computed::measure` because neither can widen a
  box, so both are a `min`. `min-width` is the opposite direction and cannot
  join them: it lands on its own field, does not inherit, and `view.rs` applies
  it after the measure has narrowed the container, per css-sizing-3 §5.1. The
  column still bounds it — there is no horizontal scroll, so a floor wider than
  the window would put the box somewhere nothing can reach.
- **A declared height sizes the box the way the measure sizes its width.**
  `height`, `min-height` and `max-height` land on `Computed` and reach
  `view::block_height`, which is applied once the block's content, padding and
  bottom border have been advanced over: the declared height replaces that
  content height, `max-height` caps it and `min-height` floors it, in the order
  css-sizing-3 §5.4 gives, so a box asked for both keeps the floor. Like the
  measure it sizes the *border* box, matching the `box-sizing: border-box`
  behaviour the width already has. Content taller than the box is left
  overflowing and drawn: nothing here clips, so `overflow: visible` is the only
  answer this can give honestly. Unlike the measure these do not inherit —
  there is no flat-block-list argument for passing a wrapper's height to its
  paragraphs the way there is for its column. A percentage resolves against the
  containing block's height, which a flowed column never has, so it behaves as
  `auto` per css-sizing-3 §5.1; `Computed::absolute` is the length parser that
  refuses one, since the shared `parse_length` deliberately reads `%` as a
  fraction of the em for margins and padding.
- **A background belongs to a run as well as to a block.** A block's
  `background-color` is one `view::Decor` spanning every line it produced; a
  colour a `<span>` or a `<mark>` sets is painted per fragment instead, over
  the text's own height rather than the line's, so a highlight inside an airy
  paragraph does not become a tall block. It runs through the spaces between
  the words it covers by the same rule a `text-decoration` does — a fragment
  reaching the next one when that one carries the same colour and baseline
  shift. `Computed::inherit` resets `background`, so the only way a run can
  carry the colour of the block around it is by being that block's own text;
  `view::words` drops it there rather than painting the block's box a second
  time behind every word. `<mark>` is the one element with a UA background
  (yellow behind black), applied only where the page set neither, since a
  highlight under an inherited page colour is unreadable exactly where it is
  meant to stand out.
- **`@layer` bodies are parsed**, since a modern stylesheet puts nearly all of
  itself inside one and skipping them drops the sheet. Layer *order* is not
  honoured — rules keep their document order — which differs from a real
  cascade only where two layers set the same property on the same element.
- **`@media` is answered from the window.** `css::media_matches` reads a query
  list: media types, `and`, `not`, `only`, comma alternatives, `width` and
  `height` in both the `min-`/`max-` and the range forms (`(40em < width <
  80em)`, either operand first), and `orientation`. A `<link media=...>` goes
  through the same evaluator, so a print sheet is never even fetched. Anything
  it cannot answer — an unknown feature, a viewport-relative unit, the boolean
  form — is false rather than a guess, and `not` does not turn one of those into
  a match.

  A media query changes the cascade, so a resized window needs the cascade run
  again. `Document` keeps what that costs: its own bytes, its base URL, and a
  cache of every subresource it fetched, misses included. `Document::reflow`
  hands them back to the same build at the new viewport, so a re-cascade is one
  parse and no network.

  It rebuilds only when an answer actually moved. `Stylesheet` records every
  `@media` prelude it read and every `<link media>` it tested, matched or not,
  and `MediaQueries::differ` re-answers them at the new size: a document that
  writes no query, or a resize inside one breakpoint, keeps the blocks it has
  and only reflows its lines. `programs/edos-web/src/ui.rs` says `edos-web: ~
  WxH - N blocks` on stdout when a rebuild happens, which is how a headless run
  tells a re-cascade from a line-break reflow.

**How the text is set follows two more properties.** `text-transform` recases at
word boundaries rather than at the first character of a run, so `read-only` is
set `Read-Only` and `it's` is set `It's`; a run glued mid-word to the one before
it is left alone, since the letter `capitalize` would raise there is inside the
word the reader sees. `text-indent` moves only the first line of a block, and a
negative one — a hanging indent — resolves to zero, because at the page edge
there is no margin for the line to hang over. Both inherit, which is what carries
them from the wrapper that declares them to the paragraphs inside it.

**`letter-spacing` and `word-spacing` are the one pair of lengths that keep a
negative sign.** A negative margin or padding has nowhere to go in a flat block
list, so `parse_length` floors those at zero; a page tightening a display
heading by `-0.02em` means exactly what it wrote, so the spacing properties go
through `parse_signed_length` instead and carry an `i32`. The tracking is added
to every character's advance, the last one included, and it reaches the space
between two words as well — a space is a character — where `word-spacing` is
added on top of it. It is applied inside `edos_render`'s blitter
(`text::draw_tracked`, `text::width_tracked`) rather than by drawing each
character at a position of its own, so the sub-pixel advances accumulate the
way an untracked run's do and the measured width stays the width drawn: an
underline or a link hit-test would otherwise drift a pixel per character.

**`white-space` decides two independent things**, and the layout is written
around that split rather than around the `<pre>` element: whether the source's
spaces and newlines survive (`keeps_spaces`, `keeps_newlines`) and whether a
line may be broken to fit the column (`wraps`). `<pre>` is nothing but the UA
default `white-space: pre` applied in `doc.rs` after the cascade, so an author
rule on the same box overrides it and `pre-wrap`, `pre-line` and `nowrap` reach
any element at all. There is one line breaker: `view.rs::words` counts the
separators instead of discarding them, carrying on each word how many spaces
and how many breaks stand before it, and `flow` turns a break into a line
wherever it stands and a kept run of spaces into that many space widths. A tab
is a fixed four spaces, since a real tab stop is measured from the start of the
line and a word carrying its own leading gap cannot see one.

That is also what makes `<br>` a break rather than a space. A collapsing box
turns its own newlines into spaces while parsing, so any newline still standing
when the breaker runs came either from `<br>` or from a box that keeps them —
and a block whose whitespace is kept is trimmed only where HTML itself ignores
it, the newline after the start tag and the closing tag's own indentation.

**A word is unbreakable unless the page says otherwise.** By default a word too
wide for the column is set on a line of its own and allowed to run past the
edge, because a URL cut across two lines reads worse than a ragged right. Two
properties change that and `css::Wrap` is the pair resolved into the one answer
`flow` needs: `overflow-wrap: break-word` cuts only as a last resort, when even
an empty line would not hold the word, and `word-break: break-all` fills the
line to the column edge and cuts wherever that falls. `word-break` wins where a
page sets both, since it asks for the cut in strictly more cases.

**The element does not decide a list's marker.** `list-style-type` does, and
`ul` and `ol` only supply the value the UA stylesheet would: `decimal` for an
ordered list, and a bullet that varies with nesting depth for an unordered one —
disc, then circle, then square, per the HTML Standard's rendering rules. So
every list counts its items, ordered or not, because a `ul` given
`list-style-type: lower-roman` needs the position too. `css::ListStyle::marker`
writes the counter in the style's own alphabet, falling back to decimal outside
the style's range the way CSS Counter Styles §5 asks — which is what a Roman
numeral past 3999 gets. `none` keeps the item's indent and drops the marker,
which is what a page styling a navigation list means by it.

**A page's own measure is honoured.** `width` and `max-width` resolve to
`Computed::measure`, and `margin: 0 auto` sets `Computed::center`, which
`view.rs` reads as "lay this block out in `measure` pixels and centre what is
left of the column". A horizontal rule is drawn to the same box, so the `<hr>`
of a narrow column stops where the column does.

Both fields **inherit, unlike the properties they come from**, and that is the
whole reason they work: the block list is flat, so the `<div>` or `<body>` that
carries the measure is not a box any later stage sees. Inheriting it is how a
wrapper reaches the paragraphs inside it. Three consequences follow:

- **Neither can widen a box.** A measure is min-ed with the one already in
  force, and the column is min-ed over that, since there is no horizontal
  scroll. A child that asks to be wider than its container is clamped to it,
  which is where this parts company with a real browser's overflow.
- **A percentage is of the containing block**, resolved during the cascade
  against the parent's measure, falling back to the window width at the root.
  This is the one place a percentage is not the font-size percentage
  `parse_length` computes.
- **`auto` on either horizontal margin centres**, rather than only the pair
  doing so. A page that writes one means the pair; a box pushed to one side by
  a single `auto` is not a layout anything here can express anyway.

A margin-right *length* is a different thing from `auto` and is kept as one, in
`Computed::margin_right`. **The order it is applied in is the whole of it.** The
measure and the inherited `center` settle the container the block sits in
first; only then does the right margin come off that container's right edge.
Taking it out of the column before centring instead reads as a narrower box to
centre, which slides a block with a right margin *left* of the very neighbours
it shares a column with — the opposite of what the property asks for. Unlike
`measure`, both horizontal margins reset on `inherit()`: a box's own margin is
not its children's.

Three deliberate limits:

- **`@supports` is skipped whole**, body included: the feature set it asks
  about is not one this can answer for, and a rule set applied on a guess is
  worse than one lost.
- **A selector using anything not implemented is dropped, not approximated.**
  `a:hover` refuses to parse, because a selector matched loosely applies far
  too widely; the failure mode of dropping one is a style that does not appear,
  which is what an unimplemented property does anyway. A dangling combinator --
  leading, trailing or doubled -- is dropped by the same rule, as is an
  unclosed `[`.
- **A subresource is fetched once per window, not once per page.** The cache
  belongs to the loader rather than to the document: every page of a site links
  the same stylesheets, and fetching them again per navigation cost around
  100 KB and a TLS handshake each on `edos.edgl.dev`. It is emptied whole once
  it holds more than `doc::CACHE_BUDGET`, since nothing here records when an
  entry was last used and a miss is a refetch rather than a wrong answer.
- **The transfer is gzipped where the server offers it.** `edos_http` sends
  `Accept-Encoding: gzip` and inflates on the way to the sink, bounded by the
  same `max_body` the wire is, because a few kilobytes of gzip expand to as
  much as the sender likes. `grab` turns it off for a package, which is already
  compressed.
- **The connection is kept and reused.** The whole homepage -- the document,
  two stylesheets and four images -- is one TCP connection and one TLS
  handshake now, measured in the guest against seven before. The pool is in
  `edos_http` and is one per process rather than per thread, since a page is
  loaded on a thread of its own and a thread-local pool would be empty on every
  navigation. A pooled connection the far end has closed cannot be told from a
  live one, so a request that went out on a reused connection is retried once
  on a fresh one; that path is the one worth testing deliberately, and
  `scripts/` has no server that closes on demand -- the one used here was a
  throwaway.
- **At most `doc::MAX_SHEETS` external sheets are fetched, and each one
  serially**, on the thread that is about to lay the page out. A page linking
  more than six is linking print and font sheets; a browser that fetched all of
  them would spend the page's load time on styles nothing reads. The serial
  fetch is the same blocking-`edos_http` problem the window already has, and it
  is now on the load path twice over.

**Images are decoded and drawn.** An `<img>` whose bytes arrive and decode
becomes a `BlockKind::Image` block carrying an `Rc<Picture>`, which is either a
raster or a still-parsed SVG tree; `view.rs` rasterises it at layout time, so a
vector picture is re-rendered when the column changes rather than magnified.
Four rules:

- **PNG, WebP, JPEG, BMP and SVG decode**, sniffed from the bytes rather than
  taken from the URL, since a server that names a WebP `.png` is a server. The
  three that are not ours are crates -- `png`, `image-webp`, `zune-jpeg` --
  behind `edos_render`'s `raster` feature, all pure Rust and all building for
  this target unpatched. They are not optional in practice: every one of the 17
  pictures on `edos.edgl.dev` is a WebP, so a browser without them shows a
  page of alt text where the screenshots are. Anything that still fails to
  decode falls back to the `[alt]` text, which is also what a failed fetch
  gives, so the page reads the same either way.
- **A picture is a block**, since the block list has no inline box. An image
  mid-sentence breaks the sentence in two and the text after it resumes in the
  block it interrupted.
- **Shrunk to the column, never enlarged**, and capped at `view::MAX_IMAGE_H`.
  A hero image that has to be scrolled past to reach the first paragraph reads
  as a page that failed to load.
- **At most `doc::MAX_IMAGES` are fetched, serially**, on the same terms and
  the same load path as the stylesheets. The budget is spent on the attempt,
  not the success.

A locally-read document's base is its own path under `http://localhost`, and
`load` reads that host back off the filesystem. So a page opened from disk
resolves what it refers to relative to itself: `/share/icons/edos.svg`, a
sibling `style.css` and `../icons/edos.svg` all work, and only the network is
out of reach.

`Url::join` is RFC 3986 §5.2 whole: `.` and `..` are resolved, climbing past
the root is absorbed rather than an error, a fragment is stripped before the
reference is read, an empty or query-only reference keeps the base's path, and
a reference naming a scheme this client cannot fetch — `mailto:`,
`javascript:` — is an error, so `doc.rs` drops the link instead of turning it
into a nonsense HTTP request. Its 34 RFC §5.4 examples, and
`css.rs`'s cascade tests, run on the host under `make host-tests` — see
`doc/WORKING-NOTES.md` for the two mechanisms that takes and the stale-binary
trap in the second one.

`assets/welcome.html` is installed at `/share/web/welcome.html` and exercises
exactly this subset, which makes it the page to open when a style stops being
honoured.

### Stage 3 — real layout

`taffy` is a pure-Rust flexbox and grid engine with no libc dependency, which is
the constraint that actually decides things here: `usvg`'s text feature is
already disabled in `edos_render::image` because `fontdb` and `rustybuzz` want a
libc this target has not got. Check any new crate against that before planning
around it.

### JavaScript is out of scope

Stated here so it is not left as an implied someday. A DOM without scripting
renders most documentation and most articles, which is the point of this
program.

## The crates, checked against a real build

Both build for `x86_64-unknown-edos` under `cargo +edos`, from source, with no
patching and no feature surgery:

| Crate | Version | Result |
|---|---|---|
| `html5ever` | 0.39 | builds |
| `markup5ever_rcdom` | 0.39.0-unofficial | builds |
| `taffy` | 0.9 | builds |

Two things about the versions are worth knowing before reaching for them.
`markup5ever_rcdom` is published only as a prerelease -- every version carries
an `+unofficial` build tag -- so a plain `"0.39"` requirement finds no candidate
and cargo's error names the fix (`"0.39.0-unofficial"`). And its html5ever
requirement is exact enough that pairing it with an older `html5ever` compiles
*both* major versions into the binary; keep the two pinned to the same one.

`RcDom` exposes the root as the `document` field. `TreeSink::get_document`
reaches the same node but needs the trait imported, which is a dependency on
html5ever's internals for nothing.

`resvg` building was the precedent that said real crates can be pulled in; the
ones that fail are the ones reaching for a libc, which is why `usvg`'s text
feature is off in `edos_render::image`. Neither of these three does.

## What it exercises in the kernel

By the ranking `doc/PROGRAMS.md` uses, this is the strongest candidate in the
tree:

- **The largest memory consumer EDOS has had.** The first program holding a big
  allocated graph plus decoded images, so the first real test of the allocator
  and the page cache under a GUI.
- **The network stack inside a GUI event loop.** Which is where the open item
  about `edos_http` moving to a non-blocking connect stops being theoretical: a
  browser that freezes its window during a DNS lookup is visibly broken in a way
  a CLI fetch is not.
- **Text layout at scale.** `edos_render`'s blitter has never been asked for a
  page of proportional text at several sizes with inline images.

## Porting one instead

Not either/or — they answer different questions. Writing `edos-web` is likely
*less* work than porting a browser and produces something that fits the system;
it will not render the real web well. Porting gets real-world HTML and CSS
compatibility without writing a layout engine.

If that is wanted later, **NetSurf** is the target: it ships a framebuffer
frontend (no X, no GTK, no GL — a pixel buffer and input events, which is what
EDOS gives a program), carries its own layout and CSS engines, makes JavaScript
optional, and is ~200k lines. It is what gets ported to RISC OS, Haiku and
AmigaOS, which is the company EDOS keeps. It needs libc stage 2, pthreads, and
the `packages/` tree — see `doc/design/dynamic-linking-and-libc.md`.

Firefox is not a candidate and it is worth saying why once: it needs a full
POSIX libc, pthreads, dynamic linking, `AF_UNIX` with descriptor passing for its
multiprocess IPC (EDOS has no `AF_UNIX` at all), OpenGL or Vulkan for WebRender,
and a C++ runtime with exceptions — all before it links. It fails on everything
at once, which makes it a useless probe of what is actually missing.

## The box tree, and taffy

`taffy` was proven to build for this target before any of the browser was
written (the table above says so) and then was not used: `view.rs` positions
boxes itself. That is why there is no `float`, no `flex`, no `grid`, and why
`inline-block` parses and then lays out as plain inline.

**The blocker was the document model, not taffy.** `Document` was a flat
`Vec<Block>`, and a flat list cannot say that three paragraphs share a
container, which is the whole of what flex and grid arrange. `BlockKind::
ListItem` carrying a `depth` number instead of nesting is the same shortage
showing through.

`doc.rs` emits a `Node` tree now -- `Container { css, children }` and
`Leaf(Block)` -- built from a stack of open frames, with an element's inline
content flushed into an anonymous leaf when a box interrupts it, the way CSS
anonymous block boxes work. A container holding exactly one leaf collapses to
that leaf, so the tree carries structure rather than one wrapper per paragraph.
`Document::blocks` is the tree flattened into document order and is still what
the inline engine walks, so nothing downstream changed: `welcome.html` renders
byte-identically against the v0.5.0 ISO, and reports `91 blocks in a tree of
108 boxes 7 deep`.

**What is left, and the shape it takes.** `taffy` lays out boxes and asks a
measure function how big a leaf is; it does not break lines. So `view.rs`'s
inline engine is not replaced, it *becomes* the measure function, and the split
that has to happen inside `Layout::build` is measure-from-emit: today the 463
lines from the block loop to the line push do both at once, positioning a box
and emitting its lines in the same pass. Under taffy the same code runs twice --
once to answer "how tall at width W", once to emit at the position taffy
computed.

Proven on this target before planning around it, with `compute_layout_with_
measure` and a flex row of a fixed 60px box beside measured text: the box lands
at x=0, the text at x=60, and the leaf size comes from the callback. So the
integration is a known quantity; the work is the measure/emit split.

`taffy` does **not** do `float`. That is CSS2 and taffy deliberately omits it,
so float stays unavailable whatever happens here.

Other crates checked against this target at the same time, since the same
question will come up: `cssparser` builds, `selectors` builds -- that is servo's
real selector engine, specificity and `:nth-*` and combinators, most of what
`css.rs` hand-rolled -- and `simplecss` builds and is *already in the lockfile*,
pulled in by `usvg`. `lightningcss` does not build: it wants a `getrandom`
handler registered the way `edos_http::tls` does, which was not taken far enough
to call either way.

### It is done, and what it took

`taffy` arranges the boxes now. Three things had to be true, and each was a
separate bug found by looking at a screenshot rather than at a compiler:

1. **The measure function must report the content's width, not the width it was
   offered.** A flex item sized at whatever space happened to be available
   claims the whole line, so its siblings are pushed onto rows of their own --
   a column wearing the name of a row. `measure_block` lays the block into a
   scratch `Layout` and takes the furthest right edge any fragment, rule, image
   or box reached.
2. **The inline engine had to place words against the box, not the page.**
   Three sites in the line breaker put fragments at `PAGE_PAD + indent`. With
   the boxes moving, the backgrounds travelled and the words stayed behind.
   They read `origin_x` now.
3. **Both axes come from the engine.** The first emit walk threaded a running
   `y` down the tree and used taffy's `x` only, which stacks every box in
   document order however its container asked them to be arranged.

Verified in the guest: `welcome.html`'s flex section puts three items on one row
under `space-between`, and its grid section fills `1fr 2fr 120px` and wraps the
next three onto a second row on the same tracks. The rest of the page is
**pixel-identical** to the v0.5.0 ISO over the whole content region.

`display: inline-block` is still laid out as plain inline, and that is not a
taffy limitation: the inline model here has no concept of a box inside a line,
so an inline-level box would need the line breaker to embed and measure one.
That is the next piece, and it is independent of the box engine.

## `display: inline-block`

An inline-block is an inline-*level* box: the line carries on around it, and it
is laid out as a block inside. The model had no place for that, since a line was
made of text runs and nothing else, so `inline-block` parsed and then laid out
as plain inline.

`Run` carries an optional subtree now. `doc.rs` sets the parent's runs aside
rather than flushing them when such an element opens, and on close puts the
subtree back into the line as one atomic run. `view.rs` turns that into a word
that never splits, and `Layout::place_box` lays the subtree out in its own
coordinates and translates it to where the line put it, re-indexing its links on
the way. The engine is reused rather than reimplemented: a box is
`Layout::build_tree` on a `Node`, which is what the page itself is.

**Four bugs on the way, and the last two are the interesting ones.**

- A boxed run carries no text, and `flush` ended with
  `runs.retain(|r| !r.text.is_empty())`. The whole feature was being deleted one
  line before it reached layout. `trim_edges` would have done the same at the
  ends of a block.
- A block's background is painted at the width the box was *given*, so reading
  the decor back as "content width" answers "as wide as you offered" to every
  question. `measure_block` ignores decor now, which also stopped flex items
  filling their row.
- **`text-align` inherits.** The intrinsic width was measured from a trial
  layout in a very wide column by taking the rightmost `x + width`, and inside a
  centred paragraph that measures the *centring shift*, not the content: every
  box came out as wide as the probe column and so took a line to itself. Each
  line's own extent, `max(x + width) - min(x)`, is what is alignment-independent.
- **`measure_block` reported a width that was neither the content's nor the
  box's.** A leaf wears its own padding, border and margin, and taffy is told
  about none of them: `container_style` deliberately sets only what arranges
  *children*, so a leaf's own insets stay with `lay_block`. Content is placed
  from the box's left edge, so the left inset is inside `x` and the right one
  leaves no fragment behind — the measured width carried one and not the other.
  The engine handed that width back as the leaf's size, `lay_block` subtracted
  both insets from it a second time, and the content re-flowed in a column
  `padding-right` too narrow. The fixture's `padding: 4px 8px` badge lost 8px
  and its two lines came out as three. `lay_block` returns the trailing inset
  now and `measure_block` adds it back, so the width it reports is the border
  box's.

Verified against `welcome.html` and against the real site, whose three hero
buttons now stand side by side instead of running together as one string of
text. The fixture's `a box<br>on two lines` badge sets in two lines, with the
text before and after it on the same line, and the three-in-a-row badges share
one line.

**Do not measure a box from the arranged root instead.** It reads as the
obvious simplification — `taffy` has just laid the subtree out, so ask it how
wide the tree came out — and it is wrong for the same reason the decor was: a
block box fills the column it is given, so the root reports `MAX_CONTENT_PROBE`
straight back and every badge takes a line to itself. Only the *lines* know how
wide the content is.

`float` is still absent and is not coming from `taffy`, which does not implement
it.
