# Damage, compositing and frame pacing

What the window system spends its time on, why it is the wrong amount, and the
shape of the fix. The numbers here come from `edos-wm`'s frame telemetry
(`/tmp/wm-frames`, and `wmfps` lines in the kernel log when `/etc/wm-metrics`
is `on`).

## What was measured

A still desktop with one terminal, nobody touching the machine:

| | before | damage fix | + regions, terminal on change | + per-row damage |
| --- | --- | --- | --- | --- |
| to the display | 263 MB/s | 82 MB/s | 6.6 MB/s | **4.1 MB/s** |
| frames doing work | 77 of 77 | 66 of 77 | **22 of 77** | 22 of 77 |
| composite p50 | 1354 us | 1322 us | **0 us** | 0 us |
| interval p50 / p95 | 13018 / 13024 us | 13018 / 13022 us | 13011 / 13014 us | 13011 / 13026 us |

The last column is the terminal reporting the rows it changed rather than all
of itself. Its share of the idle traffic went from about 2.6 MB/s to 0.09 MB/s,
which is the cursor blink costing one 640x17 row twice a second instead of the
whole 646x518 window. What is left is almost entirely the panel.

The median frame now does no work at all. What remains at idle is the panel
redrawing itself 20 times a second whatever is on it (~4 MB/s) and the
terminal's cursor blink (~2.5 MB/s), which together account for the 22 working
frames and the 6.6 MB/s almost exactly.

The cadence was never the problem and still is not: 76.8 fps with 6 us between
p50 and p95 is a metronome. Every symptom reported — a drag at about 5 fps,
tearing, a terminal that updates like a scan line, 14% CPU while idle — came
from the volume of pixels being pushed, not from the compositor being unable to
keep up.

Two defects account for the drop from 263 to 82:

**`sys_window_list` consumed damage as a side effect of reporting it.** The
panel polls the same list as the compositor, so whichever called first took the
signal and the other saw an unchanged window. The workaround was a clause in
the compositor that repainted every buffer-backed window unconditionally, which
defeated damage tracking entirely. Damage is now a counter the kernel never
clears; each reader keeps the value it last acted on.

**Every dirty region was merged into one bounding box.** A cursor near one
corner and a clock in the other span the screen between them, so a still
desktop transferred 86% of itself every frame. Regions are now coalesced only
when the union costs little more than the parts, and sent as separate
transfers.

## What the remaining 82 MB/s is

Clients repainting on a timer rather than on change:

- `edos-terminal` redraws all of itself and swaps buffers every 16 ms
- `edos-taskbar` redraws fully every 50 ms

Together that is about 82 announcements a second that something changed, while
the screen shows an identical picture.

Four things underneath, which compound:

1. **Clients repaint on a timer, not on change.**
2. **Damage has no rectangle.** `window_damage(id)` means "all of me", so a
   terminal that changed one character reports 330k pixels.
3. **`composite()` redraws the whole screen** for any damage however small; it
   ignores the dirty region completely.
4. **There is no frame callback.** `SYS_WINDOW_POLL` is non-blocking, so every
   client guesses an interval and sleeps. Nothing tells a client when a frame
   is actually wanted.

## The shape of the fix

### Region damage

`sys_window_damage(id, x, y, w, h)`, with the whole-window form kept for
callers that genuinely mean it. Per window the kernel keeps:

- `damage_seq: u32` — never cleared, so any observer can ask "did this change".
- an accumulated damage box, unioned across calls.

Reading the list does not consume the box. Consuming is explicit:
`sys_window_list` takes a flag, and only the compositor passes it. This is the
same coupling the old code had, but stated in the API instead of hidden in a
side effect, and it is what lets a second reader exist without breaking the
first.

Accumulation matters: a client may swap twice between two composites, and the
compositor must repaint the union of both, not just the latest.

### Compositing only what is dirty

`composite()` clips to the dirty region rather than redrawing 1280x800. This is
what makes a one-line change cost one line. Until this lands, region damage
reduces the *transfer* but not the compositing work, which is the 1.3 ms.

### Frame callbacks

A client asks to be woken when a frame is wanted, instead of sleeping 16 ms and
hoping. Needs a blocking wait — a per-window waitqueue woken by the compositor
after it presents, and by event delivery — since `SYS_WINDOW_POLL` cannot
block. This is the piece that removes timer guessing permanently, and the
largest of the three: it changes how every graphical program is written.

### Clients that know their own damage

The terminal has to track which rows changed and skip the repaint entirely when
none did; the panel the same for its clock and task buttons. Without this the
kernel and compositor work is wasted, because the clients still say "all of me"
every frame.

## Order

1. ~~Region damage in the kernel and `edos_render`, compositor using the box.~~
   **Done.** `sys_window_damage(id, x, y, w, h)`, accumulated per window;
   `sys_window_list` takes `WINDOW_LIST_CONSUME_DAMAGE`, which only the
   compositor passes.
2. `composite()` clipped to the dirty region. **Open.** The compositor still
   redraws all 1280x800 whenever any frame does work; only the transfer is
   bounded by damage today. This is what would move composite p95, which is
   still 1.37 ms.
3. Terminal and panel tracking their own damage and skipping unchanged frames.
   **Done.** `Terminal::draw_changed` compares the viewport against what the
   buffer being drawn already holds, repaints only the rows that differ, and
   returns the rectangle to report as damage. The panel does the same over the
   controls it draws.
4. Frame callbacks, and clients converted from timers to callbacks. **Open.**
   `SYS_WINDOW_POLL` cannot block, so this needs a per-window waitqueue woken
   by event delivery and by the compositor after it presents.

Each stage is measurable on its own with the telemetry above: 1 and 3 move
KiB/s, 2 moves composite p50, 4 moves both plus idle CPU.

## What the terminal costs, measured

`programs/termbench` times the widget with no window and no compositor, so the
number is the client's own work. A 640x480 terminal, 69x27 cells:

| | full redraw | incremental |
| --- | --- | --- |
| nothing changed | 310 us | **5 us** |
| one character typed | 544 us | **23 us** |
| one line scrolled in | 538 us | **159 us** |
| every cell a glyph | 595 us | — |

Against a 13 ms frame, rasterising was never the bottleneck: the ceiling is
0.6 ms, and half of even that is filling the background rather than drawing
glyphs. That is why the win here came from *not* drawing rather than from
drawing faster.

A scroll is the one case where every row genuinely changes. It costs one
memmove of the rows the buffer already holds plus the rasterising of the rows
that scrolled in, which is where 538 us becomes 159 us. It saves client CPU
and nothing else: every pixel on screen really is different, so the transfer
is the same size either way.

## Traps

- **`WindowListEntry` is mirrored field for field** in `kernel/src/syscalls/window.rs`
  and `programs/edos_render/src/window.rs`. Changing one without the other
  compiles cleanly and makes the compositor read garbage.
- **The framebuffer here is single-buffered.** `flip()` returns a new back-page
  offset only on the Bochs VBE path; virtio-gpu, which every `run` target uses,
  has one buffer. Partial rect transfers are therefore safe. If a double
  buffered path is ever the default, partial transfers must accumulate damage
  across two frames or each buffer will only receive the rects sent while it
  was the back one.
- **Damage is not the only reason to repaint.** Focus changes repaint the title
  bar accent, and a window that moves exposes background where it was and has
  to be drawn where it now is; all of these are tracked by the compositor from
  the previous frame's geometry rather than by the client. A client's damage box
  describes its content only, so anything that changes the window as a whole
  takes the whole rectangle instead.
- **Reading the window list consumes damage, so read it once.** The compositor
  polls the list twice a frame — once to route input, once to decide what to
  redraw — and passing `WINDOW_LIST_CONSUME_DAMAGE` to the first emptied the
  accumulated box before the second could read it. Every client then looked
  like it had repainted all of itself, which made region damage unobservable
  rather than merely unused.
- **Moving pixels needs to know which buffer they are.** A scroll shifts rows
  the buffer already holds, which is only true of the buffer that holds them:
  with two buffers alternating, a memmove inside the one about to be drawn
  shifts a frame two old. `Terminal` moves the pixels and shifts that buffer's
  record by the same amount, so the row comparison that follows rasterises
  exactly the rows that scrolled in — and, if the shift were ever wrong, every
  row it fails to account for compares unequal and is drawn again.
- **The buffer being drawn is not the buffer on screen.** With two buffers, what
  a client has to *redraw* is what the back buffer does not already hold (two
  frames old), while what it has to *report* is what the front buffer does not
  hold. `Terminal` keeps a record per buffer for exactly this: comparing against
  only one of them makes a partial repaint correct on alternate frames, and
  leaves a blinking cursor stuck because the buffer being drawn is already right
  while the screen is not.
