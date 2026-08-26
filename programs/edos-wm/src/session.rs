//! The compositor session: the state that lives across frames, and the phases
//! of one frame.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use edos_lib::config;
use edos_lib::io::klog_dump;
use edos_render::graphics::{MAX_FLIP_RECTS, Screen};
use edos_render::window::{
    WINDOW_LIST_CONSUME_DAMAGE, WindowEvent, WindowEventType, WindowListEntry, flags, focused_id,
    set_frame, window_list, window_list_flags, window_send_event,
};

use crate::compositor::{self, ShmCache};
use crate::cursor::{Cursor, CursorShape};
use crate::decorations;
use crate::desktop_menu::{self, DesktopMenu};
use crate::dirty::{DirtyRect, DirtyRegion};
use crate::frametime::{self, FrameLog};
use crate::input::{InputAction, InputState};
use crate::interaction::{
    DragState, ResizeState, determine_cursor_shape, find_window_at, handle_drag,
    handle_mouse_press, handle_resize, validate_window_state,
};

/// Maximum number of windows to track.
const MAX_WINDOWS: usize = 64;

/// Fallback frame time when the display reports no refresh rate.
const FRAME_TIME_MS_DEFAULT: u64 = 16;

/// Cursor hotspot square, in pixels, used to invalidate what the pointer left.
const CURSOR_SIZE: u32 = 16;

/// Prefix on every line of a window dump, so a host-side reader can pick the
/// block out of an interleaved serial log.
const WINDOW_DUMP_TAG: &str = "windows|";
/// Hand the display the current cursor image, returning whether it took it.
///
/// The texture is ARGB with zero alpha where the cursor is transparent, which
/// is what both the software blit and the cursor plane expect, so there is one
/// cursor image rather than two.
fn upload_cursor(screen: &Screen, cursor: &Cursor) -> bool {
    let texture = cursor.current_texture();
    screen.set_cursor(
        texture.width as u32,
        texture.height as u32,
        0,
        0,
        &texture.pixels,
    )
}

/// Copy `/proc/windows` into the kernel log.
///
/// The serial console is the only channel out of a headless guest, so this is
/// what lets something outside the machine address a window by its title
/// instead of by a pixel copied out of the panel's layout. The lines are passed
/// through rather than reformatted: procfs owns the format, and a second
/// formatter here is a second thing to keep in step.
fn dump_windows() {
    let text = match std::fs::read_to_string("/proc/windows") {
        Ok(text) => text,
        Err(e) => {
            eprintln!("[wm] /proc/windows: {e}");
            return;
        }
    };
    klog_dump(WINDOW_DUMP_TAG, text.lines());
}
/// Snapshot of a window's position/size/visibility/focus from the previous frame.
#[derive(Clone, Copy, PartialEq, Eq)]
struct PrevWindowState {
    id: u64,
    /// The repaint count this compositor last drew for the window.
    damage_seq: u32,
    x: i32,
    y: i32,
    width: u32,
    height: u32,
    visible: u32,
    flags: u64,
    focused: u32,
    /// Hash of the title this compositor last drew in the bar. A client that
    /// renames its window does not move, resize or change focus, and the region
    /// it reports covers its content only, so the title is the sole evidence
    /// that the decorations are stale.
    title_hash: u64,
}

impl PrevWindowState {
    fn from_entry(w: &WindowListEntry) -> Self {
        Self {
            id: w.id,
            damage_seq: w.damage_seq,
            x: w.x,
            y: w.y,
            width: w.width,
            height: w.height,
            visible: w.visible,
            flags: w.flags,
            focused: w.focused,
            title_hash: title_hash(w),
        }
    }

    fn dirty_rect(&self) -> DirtyRect {
        let eff_w = decorations::effective_width_raw(self.flags, self.width);
        let eff_h = decorations::effective_height_raw(self.flags, self.height);
        DirtyRect::new(self.x, self.y, eff_w as u32, eff_h as u32)
    }
}

/// FNV-1a over the title bytes, which is enough to notice a rename without
/// carrying a 256-byte array through the per-frame window snapshots.
fn title_hash(w: &WindowListEntry) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in w.title_str().as_bytes() {
        hash ^= *byte as u64;
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}
/// Tell the kernel the frame this compositor draws around each window, so a
/// click lands in the client area the client thinks it has.
///
/// The kernel starts every window with no frame and only learns otherwise from
/// here, which is why this runs every pass rather than once: a window created
/// between two passes would otherwise route its pointer events into the title
/// bar until something else changed. The window list reports the frame already
/// recorded, so a pass where nothing changed makes no syscalls.
fn publish_frames(windows: &[WindowListEntry]) {
    for w in windows {
        let (left, top, right, bottom) = if w.flags & flags::FLAG_UNDECORATED != 0 {
            (0, 0, 0, 0)
        } else {
            (
                decorations::BORDER_WIDTH as u32,
                decorations::TITLE_HEIGHT as u32,
                decorations::BORDER_WIDTH as u32,
                decorations::BORDER_WIDTH as u32,
            )
        };
        let packed =
            (left as u64) | (top as u64) << 16 | (right as u64) << 32 | (bottom as u64) << 48;
        if w.frame != packed {
            let _ = set_frame(w.id, left, top, right, bottom);
        }
    }
}

/// Everything the compositor carries between frames.
pub struct Session {
    screen: Screen,
    frame_time_ms: u64,
    cursor: Cursor,
    /// False when the display has no cursor plane, and the software cursor is
    /// composited instead.
    hw_cursor: bool,
    /// True when the kernel places the plane from the input path, which is up
    /// to a frame earlier than this loop can manage.
    cursor_tracked: bool,
    /// The last position handed to the plane, so a move is sent on a change
    /// rather than every frame.
    sent_cursor: (i32, i32),
    uploaded_shape: CursorShape,
    frame_log: FrameLog,
    /// The cadence is measured between the starts of consecutive frames, which
    /// is what a viewer perceives; the phases only explain a bad one.
    previous_frame_start: Option<Instant>,
    previous_cursor: (i32, i32),
    previously_hovered_close: Option<u64>,
    previous_menu_origin: Option<(i32, i32)>,
    drag_state: Option<DragState>,
    resize_state: Option<ResizeState>,
    maximized: HashMap<u64, (i32, i32, u32, u32)>,
    shm_cache: ShmCache,
    input: InputState,
    backgrounds: Vec<compositor::Background>,
    background: usize,
    desktop_cache: Vec<u32>,
    desktop_menu: DesktopMenu,
    dirty: DirtyRegion,
    /// Previous frame's window snapshots, for change detection.
    prev_windows: [Option<PrevWindowState>; MAX_WINDOWS],
    prev_window_count: usize,
    damaged_cursor: (i32, i32),
}

impl Session {
    /// Open the display and build the state the loop runs on, or report why the
    /// screen could not be had.
    pub fn open() -> Result<Self, String> {
        let screen = Screen::get().map_err(|e| format!("{e:?}"))?;

        // Derive frame time from display refresh rate (from EDID).
        let refresh = screen.info().refresh_rate;
        let frame_time_ms = if refresh > 0 {
            (1000 / refresh as u64).max(1)
        } else {
            FRAME_TIME_MS_DEFAULT
        };
        eprintln!("wm: refresh={refresh}Hz frame_time={frame_time_ms}ms");

        let cursor = Cursor::new();

        // The hardware cursor is what stops a pointer move damaging the
        // framebuffer at all: the cursor lives on its own plane, so moving it
        // costs one small message and no compositing. Over a remote display
        // that is the difference between a pointer that lags the frame rate and
        // one the viewer draws itself.
        //
        // A plane is the default because it costs no damage and reaches the
        // screen without waiting for a frame. It is overridable because whether
        // the plane is ever shown is not the guest's to know: the display
        // accepts the image and says yes, and a host that does not draw what it
        // holds leaves the desktop with no pointer at all. `software` in
        // /etc/cursor is the way out of that, and the way out of a host that
        // mirrors the plane onto its own pointer.
        let want_hw_cursor = config::read(config::CURSOR)
            .map(|value| !value.eq_ignore_ascii_case("software"))
            .unwrap_or(true);
        let hw_cursor = want_hw_cursor && upload_cursor(&screen, &cursor);
        let cursor_tracked = hw_cursor && screen.track_pointer(true);
        // One line, formatted before it is written. The serial console
        // interleaves whatever arrives, and a line written a format fragment at
        // a time comes back with kernel logging spliced through the middle.
        let cursor_mode = match (want_hw_cursor, hw_cursor, cursor_tracked) {
            (_, true, true) => "plane, placed from the input path",
            (_, true, false) => "plane, placed per frame",
            (true, false, _) => "composited: the display has no plane",
            (false, false, _) => "composited, by /etc/cursor",
        };
        let cursor_line = format!("[wm] cursor: {cursor_mode}");
        eprintln!("{cursor_line}");

        // Pre-compute the desktop ground (avoids 1080 lerp+fill calls per
        // frame, and a wallpaper is rescaled once rather than per frame).
        let backgrounds = compositor::available_backgrounds();
        // The recorded choice, if it still names a background that exists. A
        // wallpaper that has been deleted falls back to the first generated
        // ground rather than leaving the desktop bare.
        let background = config::read(config::WALLPAPER)
            .and_then(|name| compositor::background_index(&backgrounds, &name))
            .unwrap_or(0);
        let desktop_cache = compositor::build_desktop_cache(
            screen.width(),
            screen.height(),
            &backgrounds[background],
        );

        let mut dirty = DirtyRegion::new();
        // Force full screen on the first frame.
        dirty.mark_full_screen();

        Ok(Self {
            screen,
            frame_time_ms,
            uploaded_shape: cursor.shape(),
            cursor,
            hw_cursor,
            cursor_tracked,
            sent_cursor: (i32::MIN, i32::MIN),
            frame_log: FrameLog::new(frame_time_ms),
            previous_frame_start: None,
            previous_cursor: (0, 0),
            previously_hovered_close: None,
            previous_menu_origin: None,
            drag_state: None,
            resize_state: None,
            maximized: HashMap::new(),
            shm_cache: ShmCache::new(),
            input: InputState::new(),
            backgrounds,
            background,
            desktop_cache,
            desktop_menu: DesktopMenu::default(),
            dirty,
            prev_windows: [None; MAX_WINDOWS],
            prev_window_count: 0,
            damaged_cursor: (0, 0),
        })
    }

    /// Composite forever, one frame per display interval.
    pub fn run(&mut self) -> ! {
        let mut entries = [WindowListEntry::default(); MAX_WINDOWS];
        loop {
            self.frame(&mut entries);
        }
    }

    /// One frame: poll input, route it, work out what changed, draw it, sleep
    /// out the rest of the display interval.
    fn frame(&mut self, entries: &mut [WindowListEntry; MAX_WINDOWS]) {
        let frame_start = Instant::now();

        let (mx, my, buttons) = self.input.read_mouse();
        self.cursor.set_position(mx, my);
        self.place_pointer(mx, my);

        // This read is for routing input and publishing frames, so it must NOT
        // consume damage: consuming here empties the accumulated box before the
        // fetch below, which is the one that decides what to redraw, and every
        // client is then reported as having repainted all of itself.
        let count = window_list(entries).map_or(0, |n| n.min(MAX_WINDOWS));
        let windows = &mut entries[..count];
        publish_frames(windows);
        let focused = focused_id(windows);
        self.route_input(windows, mx, my, buttons, focused);
        // Everything above is polling: two device reads and the window-list
        // syscall, all of which happen whether or not anything changed.
        let input_us = frame_start.elapsed().as_micros() as u64;

        // Re-fetch after the interactions above have moved windows around.
        let count = window_list_flags(entries, WINDOW_LIST_CONSUME_DAMAGE)
            .map_or(0, |n| n.min(MAX_WINDOWS));
        let windows = &entries[..count];
        let focused = focused_id(windows);

        validate_window_state(windows, &mut self.drag_state, &mut self.resize_state);
        self.mark_window_damage(windows);
        let hovered_close = self.mark_hovered_close(windows);
        self.update_cursor_shape(windows);
        self.mark_menu_move();

        let (composite_us, flip_us, sent_pixels) = self.draw(windows, focused, hovered_close);

        let interval_us = self
            .previous_frame_start
            .map(|at| frame_start.duration_since(at).as_micros() as u64)
            .unwrap_or(0);
        self.previous_frame_start = Some(frame_start);
        // Motion is what smoothness is judged on, and an idle desktop averaged
        // together with a drag hides the behaviour of both.
        let pointer_moved = (mx, my) != self.previous_cursor;
        let in_motion = pointer_moved || self.drag_state.is_some() || self.resize_state.is_some();
        self.previous_cursor = (mx, my);

        self.frame_log.record(&frametime::Frame {
            interval_us,
            input_us,
            composite_us,
            flip_us,
            sent_pixels,
            pointer_moved,
            full_screen: self.dirty.full_screen,
            idle_repaint: sent_pixels == 0,
            in_motion,
        });
        self.frame_log.maybe_report();
        self.dirty.clear();
        self.throttle(frame_start);
    }

    /// Move the cursor plane, which costs no frame redraw.
    ///
    /// While the display tracks the pointer this is the backstop for the one
    /// move that has no later report to correct it: a tracked move is dropped
    /// rather than waited for when the display is busy, and if the dropped one
    /// is the last of a motion the cursor rests a few pixels from where the
    /// pointer stopped.
    ///
    /// It fires only once the pointer is at rest, which is the whole of when it
    /// is needed. A position read at the top of a frame is older than whatever
    /// the input path has placed since, so sending it during motion walks the
    /// plane backwards a report at a time — the compositor undoing, once per
    /// frame, the freshness this exists to preserve.
    fn place_pointer(&mut self, mx: i32, my: i32) {
        let at_rest = (mx, my) == self.previous_cursor;
        if self.hw_cursor && (mx, my) != self.sent_cursor && (!self.cursor_tracked || at_rest) {
            self.screen.move_cursor(mx.max(0) as u32, my.max(0) as u32);
            self.sent_cursor = (mx, my);
        }
    }

    /// Keyboard shortcuts, the desktop menu, and press/drag/resize.
    fn route_input(
        &mut self,
        windows: &mut [WindowListEntry],
        mx: i32,
        my: i32,
        buttons: u8,
        focused: Option<u64>,
    ) {
        // After the window list, so Alt+Tab has current data.
        match self.input.read_keyboard(focused, windows) {
            InputAction::AltF4 { focused_id } => {
                let _ = window_send_event(focused_id, &WindowEvent::close_requested());
            }
            InputAction::AltTab { next_id } => {
                let focus_event = WindowEvent {
                    event_type: WindowEventType::FocusGained as u32,
                    x: 0,
                    y: 0,
                    code: 0,
                    data: 0,
                };
                let _ = window_send_event(next_id, &focus_event);
            }
            InputAction::DumpWindows => dump_windows(),
            InputAction::None => {}
        }

        let (screen_w, screen_h) = (self.screen.width() as u32, self.screen.height() as u32);

        // Right click opens the desktop menu, which is the affordance every
        // desktop has and this one did not.
        if self.input.right_pressed(buttons) && find_window_at(windows, mx, my).is_none() {
            self.desktop_menu.open(mx, my, screen_w, screen_h);
        }
        if self.desktop_menu.is_open() {
            self.desktop_menu.hover(mx, my);
        }

        let left_pressed = self.input.detect_left_press(buttons);
        let left_held = InputState::left_held(buttons);

        // A click while the menu is open belongs to the menu, whether it lands
        // on a row or dismisses it.
        if left_pressed && self.desktop_menu.is_open() {
            if let Some(desktop_menu::Outcome::NextBackground) = self.desktop_menu.click(mx, my) {
                self.next_background();
            }
        } else if left_pressed {
            handle_mouse_press(
                windows,
                mx,
                my,
                &mut self.drag_state,
                &mut self.resize_state,
                focused,
                screen_w,
                screen_h,
                &mut self.maximized,
            );
        }
        handle_drag(&mut self.drag_state, mx, my, left_held);
        handle_resize(&mut self.resize_state, mx, my, left_held);
    }

    /// Cycle to the next background and record the choice, so the desktop comes
    /// back the way it was left. A read-only or absent root costs the
    /// persistence, not the change the user just made.
    fn next_background(&mut self) {
        self.background = (self.background + 1) % self.backgrounds.len();
        self.desktop_cache = compositor::build_desktop_cache(
            self.screen.width(),
            self.screen.height(),
            &self.backgrounds[self.background],
        );
        let _ = config::write(
            config::WALLPAPER,
            &compositor::background_name(&self.backgrounds[self.background]),
            "Desktop background: a path to a BMP, or lit:N for a generated ground.\n\
             Right-click the desktop and pick Change background to cycle.",
        );
        self.dirty.full_screen = true;
    }

    /// Sleep out the rest of the frame budget.
    ///
    /// A minimum sleep of 1ms avoids sub-microsecond sleeps that interact badly
    /// with the scheduler.
    fn throttle(&self, frame_start: Instant) {
        let target = Duration::from_millis(self.frame_time_ms);
        let elapsed = frame_start.elapsed();
        let Some(remaining) = target.checked_sub(elapsed) else {
            return;
        };
        if remaining > Duration::from_millis(1) {
            std::thread::sleep(remaining);
        } else {
            std::thread::yield_now();
        }
    }

    /// Work out what changed since the last frame and mark it dirty.
    fn mark_window_damage(&mut self, windows: &[WindowListEntry]) {
        if self.dirty.full_screen {
            return;
        }
        let screen_w = self.screen.width() as u32;
        let screen_h = self.screen.height() as u32;

        // Mark windows that are damaged, newly appeared, or have a SHM buffer
        // (active rendering clients like the terminal).
        for w in windows.iter() {
            if w.visible == 0 {
                continue;
            }
            let prev = self.prev_windows[..self.prev_window_count]
                .iter()
                .flatten()
                .find(|s| s.id == w.id);
            // A focus change repaints the title-bar accent, so it is dirty even
            // when nothing else about the window moved.
            let focus_changed = prev.is_some_and(|s| s.focused != w.focused);
            // A window that moved or resized has to be drawn where it is now.
            // Only the ground it left behind is covered below, and a move does
            // not touch the damage counter, so without this a dragged window is
            // transferred one frame behind its own edge.
            let moved = prev.is_some_and(|s| {
                s.x != w.x || s.y != w.y || s.width != w.width || s.height != w.height
            });
            // A repaint is a change in the window's damage counter since the
            // frame this compositor last drew. The counter is never cleared by
            // the kernel, so the panel polling the same window list cannot
            // consume the signal first and leave the window looking unchanged.
            let repainted = prev.is_none_or(|s| s.damage_seq != w.damage_seq);
            // A rename repaints the title bar, which is outside every region
            // the client itself can report.
            let renamed = prev.is_some_and(|s| s.title_hash != title_hash(w));
            if !(repainted || focus_changed || moved || renamed) {
                continue;
            }
            let s = PrevWindowState::from_entry(w);
            // A client that reported the region it drew gets that region
            // transferred. Anything that changes the window as a whole -- its
            // first frame, its decorations, where it sits -- gets all of it,
            // because the client's box describes its content and nothing else.
            let reported_region = repainted
                && !focus_changed
                && !moved
                && !renamed
                && prev.is_some()
                && w.damage_w != 0
                && w.damage_h != 0;
            let region = if reported_region {
                let (fx, fy) = decorations::content_origin(w);
                DirtyRect::new(
                    w.x + fx + w.damage_x as i32,
                    w.y + fy + w.damage_y as i32,
                    w.damage_w,
                    w.damage_h,
                )
            } else {
                s.dirty_rect()
            };
            if let Some(r) = region.clipped(screen_w, screen_h) {
                self.dirty.mark_dirty(r);
            }
        }

        // Mark old rects of moved, resized or disappeared windows, which is
        // what repaints the ground they no longer cover. Size counts as much as
        // position: a window dragged smaller vacates a band down its old right
        // and bottom edges, and leaving it out of this test left that band
        // showing the window that used to be there.
        for slot in self.prev_windows[..self.prev_window_count].iter().flatten() {
            let still_here = windows.iter().any(|w| {
                w.id == slot.id
                    && w.x == slot.x
                    && w.y == slot.y
                    && w.width == slot.width
                    && w.height == slot.height
            });
            if !still_here && let Some(r) = slot.dirty_rect().clipped(screen_w, screen_h) {
                self.dirty.mark_dirty(r);
            }
        }

        // Mark cursor regions dirty when the cursor moved.
        let cursor_now = (self.cursor.x, self.cursor.y);
        if self.damaged_cursor != cursor_now {
            for (x, y) in [self.damaged_cursor, cursor_now] {
                if let Some(r) =
                    DirtyRect::new(x, y, CURSOR_SIZE, CURSOR_SIZE).clipped(screen_w, screen_h)
                {
                    self.dirty.mark_dirty(r);
                }
            }
        }
        self.damaged_cursor = cursor_now;

        // Snapshot current windows for next frame's change detection.
        self.prev_window_count = windows.len();
        for (slot, w) in self.prev_windows.iter_mut().zip(windows) {
            *slot = (w.visible != 0).then(|| PrevWindowState::from_entry(w));
        }
        self.prev_windows[windows.len()..].fill(None);
    }

    /// Which window's close button the cursor is over, marking the title bars
    /// that gained or lost the hover.
    ///
    /// The close button paints a red field under the pointer, which is a change
    /// to the window's decorations that the client knows nothing about and that
    /// no other test here catches: it is not a repaint, a focus change or a
    /// move. Without this the button was only redrawn where some *other* dirty
    /// region happened to overlap it -- in practice the moving cursor's own
    /// rectangle -- so it came out in ragged patches of old and new colour.
    fn mark_hovered_close(&mut self, windows: &[WindowListEntry]) -> Option<u64> {
        // Windows are sorted back-to-front by z_order, so iterate in reverse
        // (front-to-back) and take the first close-button hit.
        let hovered = windows
            .iter()
            .filter(|w| w.visible != 0)
            .rev()
            .find(|w| {
                decorations::hit_test(w, self.cursor.x, self.cursor.y)
                    == decorations::HitRegion::CloseButton
            })
            .map(|w| w.id);
        if hovered == self.previously_hovered_close {
            return hovered;
        }
        let screen_w = self.screen.width() as u32;
        let screen_h = self.screen.height() as u32;
        for id in [hovered, self.previously_hovered_close]
            .into_iter()
            .flatten()
        {
            let Some(w) = windows.iter().find(|w| w.id == id) else {
                continue;
            };
            // The title bar only: the rest of the window is unchanged.
            let bar = DirtyRect::new(
                w.x,
                w.y,
                decorations::effective_width_raw(w.flags, w.width) as u32,
                decorations::TITLE_HEIGHT as u32,
            );
            if let Some(r) = bar.clipped(screen_w, screen_h) {
                self.dirty.mark_dirty(r);
            }
        }
        self.previously_hovered_close = hovered;
        hovered
    }

    /// Pick the shape the pointer's position implies and get it on screen.
    fn update_cursor_shape(&mut self, windows: &[WindowListEntry]) {
        let previous_shape = self.cursor.shape();
        self.cursor.set_shape(determine_cursor_shape(
            windows,
            &self.drag_state,
            &self.resize_state,
            self.cursor.x,
            self.cursor.y,
        ));
        // A hardware cursor is an image the display holds, so a shape change is
        // an upload rather than a different texture at composite time.
        if self.hw_cursor && self.cursor.shape() != self.uploaded_shape {
            self.hw_cursor = upload_cursor(&self.screen, &self.cursor);
            self.uploaded_shape = self.cursor.shape();
        }
        // A software cursor is composited instead, so the shape is part of the
        // picture and a change to it is damage the pointer's own motion does
        // not cover. Reachable with the pointer perfectly still: releasing the
        // button at the end of a drag while over a resize edge, or a window
        // moving or resizing under it.
        if !self.hw_cursor
            && self.cursor.shape() != previous_shape
            && let Some(r) = DirtyRect::new(self.cursor.x, self.cursor.y, CURSOR_SIZE, CURSOR_SIZE)
                .clipped(self.screen.width() as u32, self.screen.height() as u32)
        {
            self.dirty.mark_dirty(r);
        }
    }

    /// The desktop menu is compositor-owned too. While it is open its rectangle
    /// joins the dirty set every frame, but the frame it closes or moves on has
    /// nothing else to report it, so the rows stayed on the ground until
    /// unrelated damage happened to cover them.
    fn mark_menu_move(&mut self) {
        let origin = self.desktop_menu.origin();
        if origin == self.previous_menu_origin {
            return;
        }
        let screen_w = self.screen.width() as u32;
        let screen_h = self.screen.height() as u32;
        for (ox, oy) in [origin, self.previous_menu_origin].into_iter().flatten() {
            let rect = DirtyRect::new(
                ox,
                oy,
                desktop_menu::WIDTH as u32,
                desktop_menu::height() as u32,
            );
            if let Some(r) = rect.clipped(screen_w, screen_h) {
                self.dirty.mark_dirty(r);
            }
        }
        self.previous_menu_origin = origin;
    }

    /// Composite the dirty set and transfer it, reporting how long each half
    /// took and how many pixels reached the display.
    ///
    /// Nothing changed and no menu painted over the top means the image already
    /// on the display is the image this frame would produce. The compositor
    /// rewrites every pixel whatever the damage says, so skipping here is what
    /// turns an idle desktop into an idle CPU.
    fn draw(
        &mut self,
        windows: &[WindowListEntry],
        focused: Option<u64>,
        hovered_close: Option<u64>,
    ) -> (u64, u64, u64) {
        let menu_open = self.desktop_menu.origin().is_some();
        if self.dirty.is_empty() && !menu_open {
            return (0, 0, 0);
        }

        let composite_start = Instant::now();
        self.shm_cache.retain_active(windows);

        // The menu is painted over the top, so its rectangle joins the dirty
        // set before anything is drawn: compositing is now clipped to that set,
        // and a region nobody declared is a region nobody draws.
        if let Some((ox, oy)) = self.desktop_menu.origin() {
            self.dirty.mark_dirty(DirtyRect::new(
                ox,
                oy,
                desktop_menu::WIDTH as u32,
                desktop_menu::height() as u32,
            ));
        }

        // Compositing runs once per dirty region rather than once over the
        // screen. This is what makes a one-line change cost one line of drawing
        // instead of a megapixel of desktop and every window on top of it.
        let whole = DirtyRect::new(
            0,
            0,
            self.screen.width() as u32,
            self.screen.height() as u32,
        );
        let (coalesced, count) = self.dirty.coalesced();
        let passes: &[DirtyRect] = if self.dirty.full_screen {
            std::slice::from_ref(&whole)
        } else {
            &coalesced[..count]
        };

        for region in passes {
            self.screen
                .set_clip(Some((region.x, region.y, region.w, region.h)));
            compositor::composite(
                &mut self.screen,
                windows,
                &self.cursor,
                focused,
                &mut self.shm_cache,
                hovered_close,
                self.hw_cursor,
                &self.desktop_cache,
            );
            if menu_open {
                self.desktop_menu.draw(&mut self.screen);
            }
        }
        self.screen.set_clip(None);
        let composite_us = composite_start.elapsed().as_micros() as u64;

        let flip_start = Instant::now();
        let sent_pixels = self.transfer();
        // After the frame is on the display, not before: a client woken by the
        // damage being consumed would be racing the compositor it is trying to
        // keep step with.
        edos_render::window::present();
        (
            composite_us,
            flip_start.elapsed().as_micros() as u64,
            sent_pixels,
        )
    }

    /// Send the dirty set to the display, returning how many pixels went.
    ///
    /// One transfer for the whole frame, over the union of the regions. Each
    /// transfer is a separate display update, so sending the parts separately
    /// lets a viewer see the frame arrive in pieces: during a drag the window
    /// is erased at its old rectangle and drawn at its new one as two different
    /// updates, which reads as the window tearing into slices. Costs the pixels
    /// between the regions.
    fn transfer(&mut self) -> u64 {
        let screen_w = self.screen.width() as u32;
        let screen_h = self.screen.height() as u32;
        if self.dirty.full_screen {
            self.screen.flip();
            return screen_w as u64 * screen_h as u64;
        }

        let (rects, count) = self.dirty.coalesced();
        let union = rects[..count]
            .iter()
            .copied()
            .reduce(|acc, rect| acc.union(rect));
        let Some(c) = union.and_then(|u| u.clipped(screen_w, screen_h)) else {
            return 0;
        };

        // The regions go out as themselves, behind one flush over the box that
        // covers them. `coalesced` has already decided which are worth keeping
        // apart -- it refuses to merge a pair whose union costs more than the
        // two -- and sending the box instead spent the gap between them on
        // every frame. Free where the display reads guest memory, a real copy
        // where it does not.
        let mut list = [(0u32, 0u32, 0u32, 0u32); MAX_FLIP_RECTS];
        let mut n = 0;
        let mut sent_pixels = 0u64;
        for rect in rects[..count].iter() {
            if let Some(r) = rect.clipped(screen_w, screen_h) {
                list[n] = (r.x as u32, r.y as u32, r.w, r.h);
                sent_pixels += r.area();
                n += 1;
                if n == MAX_FLIP_RECTS {
                    break;
                }
            }
        }
        if n == 0 {
            sent_pixels += c.area();
        }
        self.screen
            .flip_rects(&list[..n], (c.x as u32, c.y as u32, c.w, c.h));
        // Past the whole screen there is nothing left to save, and one ioctl
        // beats several.
        sent_pixels.min(screen_w as u64 * screen_h as u64)
    }
}
