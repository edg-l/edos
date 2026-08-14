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

### Stage 2 — a CSS subset

Colours, font families and sizes, margins, padding, borders — the box model.
Turns "readable" into "looks close to right".

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

## Check before planning

That `html5ever` and `taffy` build for `x86_64-unknown-edos` under `cargo
+edos`. `resvg` building is the precedent that says real crates can be pulled
in; the ones that fail are the ones reaching for a libc.

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
