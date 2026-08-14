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
| Images | `resvg`/`usvg` for SVG and BMP decode, both already wired into `imgview` |
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
rendering is in `text.rs` behind `-d`; the window, its header and its scrolling
are in `ui.rs`, and the layout that feeds it is in `view.rs`. Driving the guest
renders `edos.edgl.dev` in a window with its headings at the right sizes, its
links underlined and its lists marked. What is left in this stage is the click:
`view::Fragment` already carries an index into `Layout::links`, so following one
is a hit test against the fragment boxes, a re-fetch, and a stack for the back
action.

One limit is stage-2 work, not a defect: an element set the page lays out
with CSS -- a `nav` of bare `<a>` with no whitespace between them -- renders
run-together, because without CSS there is nothing in the document to say those
are separate boxes.

### Stage 2 — a CSS subset

Colours, font families and sizes, margins, padding, borders — the box model.
Turns "readable" into "looks close to right".

Where it stands: `css.rs` is the cascade. It reads every `<style>` element,
every `<link rel=stylesheet>` the document fetches, and every `style=`
attribute; matches selectors that are comma-separated descendant chains of
tag/`.class`/`#id` compounds; orders them by real specificity with the inline
attribute winning; and computes `color`, `font-size` (px, pt, em, rem, `%` and
the absolute keywords), `font-weight`, `font-style`, `font-family`'s
monospace-or-not, `text-decoration`, the vertical and left margins, the measure
a box asks for with `width`/`max-width`, the box it paints for itself with
`background-color`, `padding` and `border` (both shorthands and the per-edge
longhands), and `display: none`. `doc.rs` carries the computed style onto every `Run` and every
`Block`, and `view.rs` lets it override the plan the tag alone implies — where
it says nothing, the reader typography stands.

Three pieces exist because a stylesheet written this decade is unreadable
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
- **`:root`.** The one pseudo-class implemented, because it is where a sheet
  declares its palette; in an HTML document it is `html` and nothing else, so
  it is rewritten to that tag.
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

Three deliberate limits:

- **`@supports` is skipped whole**, body included: the feature set it asks
  about is not one this can answer for, and a rule set applied on a guess is
  worse than one lost.
- **A selector using anything not implemented is dropped, not approximated.**
  `a:hover`, `p[hidden]`, `>` and `*` all refuse to parse, because a selector
  matched loosely applies far too widely; the failure mode of dropping one is a
  style that does not appear, which is what an unimplemented property does
  anyway.
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

- **BMP and SVG are what decodes**, sniffed from the bytes rather than taken
  from the URL. A PNG or a JPEG is not an error — the block falls back to the
  `[alt]` text it carries, which is also what a failed fetch gives, so the page
  reads the same either way.
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
into a nonsense HTTP request. Its 34 RFC §5.4 examples run on the host the same
way `css.rs`'s do, through a harness that supplies the one item the module
borrows from its crate:

```rust
// /tmp/urltest.rs
#[derive(Debug)]
pub enum Error { Url(String) }
#[path = "<repo>/programs/edos_http/src/url.rs"]
mod url;
fn main() {}
```

```
rustc +nightly --edition 2024 --test /tmp/urltest.rs -o /tmp/urltest && /tmp/urltest
```

`css.rs` depends on nothing but `std`, so its unit tests run on the host even
though the crate only links for `x86_64-unknown-edos`:
`rustc +nightly --edition 2024 --test programs/edos-web/src/css.rs -o /tmp/t && /tmp/t`.

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
