//! EDOS Window Manager - User-space compositor.

use std::time::{Duration, Instant};

use std::collections::HashMap;

use edos_render::graphics::Screen;
use edos_render::window::{
    WindowEvent, WindowEventType, WindowListEntry, flags, focused_id, property, set_frame,
    window_list, window_minimize, window_send_event, window_set,
};

/// Height of the panel, which a maximized window must not cover.
///
/// The panel owns this number; duplicated here because there is no protocol
/// for a panel to reserve a strut yet, and a maximized window that hides it
/// leaves no way back to any other window.
const PANEL_HEIGHT: u32 = 40;

mod compositor;
mod cursor;
mod decorations;
mod desktop_menu;
mod dirty;
mod input;

use compositor::ShmCache;
use cursor::{Cursor, CursorShape};
use decorations::HitRegion;
use dirty::{DirtyRect, DirtyRegion};
use input::{InputAction, InputState};

/// Maximum number of windows to track.
const MAX_WINDOWS: usize = 64;

/// Prefix on every line of a window dump, so a host-side reader can pick the
/// block out of an interleaved serial log.
const WINDOW_DUMP_TAG: &str = "windows|";

/// Copy `/proc/windows` into the kernel log.
///
/// The serial console is the only channel out of a headless guest, so this is
/// what lets something outside the machine address a window by its title
/// instead of by a pixel copied out of the panel's layout. The lines are passed
/// through rather than reformatted: procfs owns the format, and a second
/// formatter here is a second thing to keep in step.
fn dump_windows() {
    use std::io::Write;

    let text = match std::fs::read_to_string("/proc/windows") {
        Ok(text) => text,
        Err(e) => {
            eprintln!("[wm] /proc/windows: {e}");
            return;
        }
    };
    let Ok(mut klog) = std::fs::OpenOptions::new().write(true).open("/dev/klog") else {
        eprintln!("[wm] /dev/klog is not writable");
        return;
    };
    // One write per line, formatted first: each write to /dev/klog is one log
    // message, and `write!` would split a line across several of them.
    for line in text.lines() {
        let _ = klog.write_all(format!("{WINDOW_DUMP_TAG} {line}").as_bytes());
    }
}

/// Target frame time (approximately 60 FPS).
const FRAME_TIME_MS_DEFAULT: u64 = 16;

/// Minimum window width in pixels.
const MIN_WINDOW_WIDTH: u32 = 100;

/// Minimum window height in pixels.
const MIN_WINDOW_HEIGHT: u32 = 50;

/// Track window dragging state.
struct DragState {
    window_id: u64,
    offset_x: i32, // cursor offset from window origin
    offset_y: i32,
}

/// Track window resize state.
struct ResizeState {
    window_id: u64,
    region: HitRegion,
    start_x: i32,
    start_y: i32,
    orig_win_x: i32,
    orig_win_y: i32,
    orig_win_w: u32,
    orig_win_h: u32,
}

/// Cursor size for dirty rect marking.
const CURSOR_SIZE: u32 = 16;

/// Snapshot of a window's position/size/visibility/focus from the previous frame.
#[derive(Clone, Copy, PartialEq, Eq)]
struct PrevWindowState {
    id: u64,
    x: i32,
    y: i32,
    width: u32,
    height: u32,
    visible: u32,
    flags: u64,
    focused: u32,
}

impl PrevWindowState {
    fn from_entry(w: &WindowListEntry) -> Self {
        Self {
            id: w.id,
            x: w.x,
            y: w.y,
            width: w.width,
            height: w.height,
            visible: w.visible,
            flags: w.flags,
            focused: w.focused,
        }
    }

    fn dirty_rect(&self) -> DirtyRect {
        let eff_w = decorations::effective_width_raw(self.flags, self.width);
        let eff_h = decorations::effective_height_raw(self.flags, self.height);
        DirtyRect::new(self.x, self.y, eff_w as u32, eff_h as u32)
    }
}

/// Find the topmost window under the cursor.
fn find_window_at(windows: &[WindowListEntry], x: i32, y: i32) -> Option<&WindowListEntry> {
    windows
        .iter()
        .filter(|w| w.on_screen())
        .filter(|w| {
            let total_w = decorations::effective_width(w) as i32;
            let total_h = decorations::effective_height(w) as i32;
            x >= w.x && x < w.x + total_w && y >= w.y && y < w.y + total_h
        })
        .max_by_key(|w| w.z_order)
}

/// Handle a left mouse button press: hit-test the topmost window and start
/// drag, resize, or close accordingly.
///
/// Focus itself is the kernel registry's: it already moves focus to the window
/// under a press. Only the desktop-background case has to be reported, since
/// the kernel leaves focus alone when no window is hit.
#[allow(clippy::too_many_arguments)]
fn handle_mouse_press(
    windows: &[WindowListEntry],
    mx: i32,
    my: i32,
    drag_state: &mut Option<DragState>,
    resize_state: &mut Option<ResizeState>,
    focused_window_id: Option<u64>,
    screen_w: u32,
    screen_h: u32,
    maximized: &mut HashMap<u64, (i32, i32, u32, u32)>,
) {
    let window = match find_window_at(windows, mx, my) {
        Some(w) => w,
        None => {
            if let Some(old_id) = focused_window_id {
                let lost_event = WindowEvent {
                    event_type: WindowEventType::FocusLost as u32,
                    x: 0,
                    y: 0,
                    code: 0,
                    data: 0,
                };
                let _ = window_send_event(old_id, &lost_event);
            }
            return;
        }
    };
    let window_id = window.id;
    let region = decorations::hit_test(window, mx, my);

    match region {
        HitRegion::CloseButton => {
            let _ = window_send_event(window_id, &WindowEvent::close_requested());
        }
        HitRegion::MinimizeButton => {
            // The panel keeps the button, so there is a way back.
            let _ = window_minimize(window_id, true);
        }
        HitRegion::MaximizeButton => {
            toggle_maximized(window, screen_w, screen_h, maximized);
        }
        HitRegion::TitleBar => {
            *drag_state = Some(DragState {
                window_id,
                offset_x: mx - window.x,
                offset_y: my - window.y,
            });
        }
        HitRegion::ResizeTop
        | HitRegion::ResizeBottom
        | HitRegion::ResizeLeft
        | HitRegion::ResizeRight
        | HitRegion::ResizeTopLeft
        | HitRegion::ResizeTopRight
        | HitRegion::ResizeBottomLeft
        | HitRegion::ResizeBottomRight => {
            *resize_state = Some(ResizeState {
                window_id,
                region,
                start_x: mx,
                start_y: my,
                orig_win_x: window.x,
                orig_win_y: window.y,
                orig_win_w: window.width,
                orig_win_h: window.height,
            });
        }
        _ => {}
    }
}

/// Update window position while dragging, or stop if mouse released.
fn handle_drag(drag_state: &mut Option<DragState>, mx: i32, my: i32, left_held: bool) {
    let drag = match *drag_state {
        Some(ref d) => d,
        None => return,
    };

    if left_held {
        let new_x = mx - drag.offset_x;
        let new_y = my - drag.offset_y;
        let _ = window_set(drag.window_id, property::X, new_x as i64 as u64);
        let _ = window_set(drag.window_id, property::Y, new_y as i64 as u64);
    } else {
        *drag_state = None;
    }
}

/// Update window dimensions while resizing, or finalize and send resize event.
fn handle_resize(resize_state: &mut Option<ResizeState>, mx: i32, my: i32, left_held: bool) {
    let resize = match *resize_state {
        Some(ref r) => r,
        None => return,
    };

    let dx = mx - resize.start_x;
    let dy = my - resize.start_y;

    let (mut new_x, mut new_y, mut new_w, mut new_h) = (
        resize.orig_win_x,
        resize.orig_win_y,
        resize.orig_win_w as i32,
        resize.orig_win_h as i32,
    );

    match resize.region {
        HitRegion::ResizeRight => new_w += dx,
        HitRegion::ResizeBottom => new_h += dy,
        HitRegion::ResizeLeft => {
            new_x += dx;
            new_w -= dx;
        }
        HitRegion::ResizeTop => {
            new_y += dy;
            new_h -= dy;
        }
        HitRegion::ResizeBottomRight => {
            new_w += dx;
            new_h += dy;
        }
        HitRegion::ResizeTopLeft => {
            new_x += dx;
            new_y += dy;
            new_w -= dx;
            new_h -= dy;
        }
        HitRegion::ResizeTopRight => {
            new_y += dy;
            new_w += dx;
            new_h -= dy;
        }
        HitRegion::ResizeBottomLeft => {
            new_x += dx;
            new_w -= dx;
            new_h += dy;
        }
        _ => {}
    }

    // Enforce minimum window size
    if new_w < MIN_WINDOW_WIDTH as i32 {
        if matches!(
            resize.region,
            HitRegion::ResizeLeft | HitRegion::ResizeTopLeft | HitRegion::ResizeBottomLeft
        ) {
            new_x = resize.orig_win_x + resize.orig_win_w as i32 - MIN_WINDOW_WIDTH as i32;
        }
        new_w = MIN_WINDOW_WIDTH as i32;
    }
    if new_h < MIN_WINDOW_HEIGHT as i32 {
        if matches!(
            resize.region,
            HitRegion::ResizeTop | HitRegion::ResizeTopLeft | HitRegion::ResizeTopRight
        ) {
            new_y = resize.orig_win_y + resize.orig_win_h as i32 - MIN_WINDOW_HEIGHT as i32;
        }
        new_h = MIN_WINDOW_HEIGHT as i32;
    }

    let win_id = resize.window_id;
    if left_held {
        let _ = window_set(win_id, property::X, new_x as i64 as u64);
        let _ = window_set(win_id, property::Y, new_y as i64 as u64);
        let _ = window_set(win_id, property::WIDTH, new_w as i64 as u64);
        let _ = window_set(win_id, property::HEIGHT, new_h as i64 as u64);
    } else {
        let resize_event = WindowEvent {
            event_type: WindowEventType::Resize as u32,
            x: new_w,
            y: new_h,
            code: 0,
            data: 0,
        };
        let _ = window_send_event(win_id, &resize_event);
        *resize_state = None;
    }
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

/// Fill the working area, or put the window back where it was.
///
/// The previous geometry is remembered here rather than in the kernel: what
/// counts as "restored" is a window manager's policy, and the kernel has no
/// business holding a second position for a window.
fn toggle_maximized(
    window: &WindowListEntry,
    screen_w: u32,
    screen_h: u32,
    maximized: &mut HashMap<u64, (i32, i32, u32, u32)>,
) {
    let id = window.id;
    if let Some((x, y, w, h)) = maximized.remove(&id) {
        let _ = window_set(id, property::X, x as i64 as u64);
        let _ = window_set(id, property::Y, y as i64 as u64);
        let _ = window_set(id, property::WIDTH, w as u64);
        let _ = window_set(id, property::HEIGHT, h as u64);
        let event = WindowEvent {
            event_type: WindowEventType::Resize as u32,
            x: w as i32,
            y: h as i32,
            code: 0,
            data: 0,
        };
        let _ = window_send_event(id, &event);
        return;
    }

    maximized.insert(id, (window.x, window.y, window.width, window.height));

    // The working area, not the screen: a maximized window that covers the
    // panel hides the only way to get back to any other window.
    let frame_w = decorations::BORDER_WIDTH as u32 * 2;
    let frame_h = decorations::TITLE_HEIGHT as u32 + decorations::BORDER_WIDTH as u32;
    let avail_h = screen_h.saturating_sub(PANEL_HEIGHT);
    let new_w = screen_w.saturating_sub(frame_w).max(1);
    let new_h = avail_h.saturating_sub(frame_h).max(1);

    let _ = window_set(id, property::X, 0);
    let _ = window_set(id, property::Y, 0);
    let _ = window_set(id, property::WIDTH, new_w as u64);
    let _ = window_set(id, property::HEIGHT, new_h as u64);
    let event = WindowEvent {
        event_type: WindowEventType::Resize as u32,
        x: new_w as i32,
        y: new_h as i32,
        code: 0,
        data: 0,
    };
    let _ = window_send_event(id, &event);
}

/// Invalidate drag/resize state if their target windows no longer exist.
///
/// Focus needs no equivalent: the registry re-focuses the topmost survivor when
/// a window is destroyed.
fn validate_window_state(
    windows: &[WindowListEntry],
    drag_state: &mut Option<DragState>,
    resize_state: &mut Option<ResizeState>,
) {
    if let Some(ref drag) = *drag_state {
        if !windows.iter().any(|w| w.id == drag.window_id) {
            *drag_state = None;
        }
    }
    if let Some(ref resize) = *resize_state {
        if !windows.iter().any(|w| w.id == resize.window_id) {
            *resize_state = None;
        }
    }
}

/// Determine the cursor shape based on current interaction state and hover position.
fn determine_cursor_shape(
    windows: &[WindowListEntry],
    drag_state: &Option<DragState>,
    resize_state: &Option<ResizeState>,
    cx: i32,
    cy: i32,
) -> CursorShape {
    if drag_state.is_some() {
        return CursorShape::Arrow;
    }

    if let Some(ref rs) = *resize_state {
        return match rs.region {
            HitRegion::ResizeLeft | HitRegion::ResizeRight => CursorShape::ResizeH,
            HitRegion::ResizeTop | HitRegion::ResizeBottom => CursorShape::ResizeV,
            HitRegion::ResizeTopLeft | HitRegion::ResizeBottomRight => CursorShape::ResizeFDiag,
            HitRegion::ResizeTopRight | HitRegion::ResizeBottomLeft => CursorShape::ResizeBDiag,
            _ => CursorShape::Arrow,
        };
    }

    for window in windows.iter().rev() {
        if window.visible == 0 {
            continue;
        }
        let region = decorations::hit_test(window, cx, cy);
        if region != HitRegion::None {
            return match region {
                HitRegion::ResizeLeft | HitRegion::ResizeRight => CursorShape::ResizeH,
                HitRegion::ResizeTop | HitRegion::ResizeBottom => CursorShape::ResizeV,
                HitRegion::ResizeTopLeft | HitRegion::ResizeBottomRight => CursorShape::ResizeFDiag,
                HitRegion::ResizeTopRight | HitRegion::ResizeBottomLeft => CursorShape::ResizeBDiag,
                _ => CursorShape::Arrow,
            };
        }
    }

    CursorShape::Arrow
}

fn main() {
    eprintln!("[wm] starting");

    // Initialize screen
    let mut screen = match Screen::get() {
        Ok(s) => s,
        Err(e) => {
            eprintln!("Failed to initialize screen: {:?}", e);
            return;
        }
    };

    // Derive frame time from display refresh rate (from EDID).
    let frame_time_ms = if screen.info().refresh_rate > 0 {
        1000 / screen.info().refresh_rate as u64
    } else {
        FRAME_TIME_MS_DEFAULT
    };
    let frame_time_ms = frame_time_ms.max(1);
    eprintln!(
        "wm: refresh={}Hz frame_time={}ms",
        screen.info().refresh_rate,
        frame_time_ms
    );

    // Initialize cursor
    let mut cursor = Cursor::new();

    // Hardware cursor (virtio-gpu cursorq) is supported but requires absolute
    // input mode (usb-tablet) to work with QEMU's GTK backend. With PS/2
    // relative mouse, GTK hides the GDK cursor during input grab. Disabled
    // until we have a USB HID driver or virtio-input support.
    let hw_cursor = false;

    // Window list buffer
    let mut entries = [WindowListEntry::default(); MAX_WINDOWS];

    // Interaction state
    let mut drag_state: Option<DragState> = None;
    let mut resize_state: Option<ResizeState> = None;
    let mut maximized: HashMap<u64, (i32, i32, u32, u32)> = HashMap::new();
    let mut hovered_close_window: Option<u64>;

    // Shared memory mapping cache
    let mut shm_cache = ShmCache::new();

    // Input device state (mouse + keyboard)
    let mut input = InputState::new();

    // Pre-compute the desktop ground (avoids 1080 lerp+fill calls per frame,
    // and a wallpaper is rescaled once rather than per frame).
    let backgrounds = compositor::available_backgrounds();
    let mut background = 0usize;
    let mut desktop_cache =
        compositor::build_desktop_cache(screen.width(), screen.height(), &backgrounds[background]);
    let mut desktop_menu = desktop_menu::DesktopMenu::default();

    // Dirty region tracking
    let mut dirty = DirtyRegion::new();
    // Force full screen on the first frame.
    dirty.mark_full_screen();

    // Previous frame window snapshots for change detection.
    let mut prev_windows: [Option<PrevWindowState>; MAX_WINDOWS] = [None; MAX_WINDOWS];
    let mut prev_window_count: usize = 0;

    // Previous cursor position for dirty rect invalidation.
    let mut prev_cursor_x: i32 = 0;
    let mut prev_cursor_y: i32 = 0;

    // Main compositor loop
    loop {
        let frame_start = Instant::now();

        // Read mouse state
        let (mx, my, buttons) = input.read_mouse();
        cursor.set_position(mx, my);

        // Update hardware cursor position (very cheap, no frame redraw).
        if hw_cursor {
            screen.move_cursor(mx.max(0) as u32, my.max(0) as u32);
        }

        // Get current window list from kernel
        let window_count = match window_list(&mut entries) {
            Ok(count) => count.min(MAX_WINDOWS),
            Err(_) => 0,
        };

        let windows = &mut entries[..window_count];
        publish_frames(windows);
        let focused_window_id = focused_id(windows);

        // Process keyboard shortcuts (after window list so Alt+Tab has current data)
        match input.read_keyboard(focused_window_id, windows) {
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

        // Right click opens the desktop menu, which is the affordance every
        // desktop has and this one did not.
        let right_pressed = input.right_pressed(buttons);
        if right_pressed && find_window_at(windows, mx, my).is_none() {
            desktop_menu.open(
                mx,
                my,
                screen.info().width as u32,
                screen.info().height as u32,
            );
        }
        if desktop_menu.is_open() {
            desktop_menu.hover(mx, my);
        }

        // Detect mouse button transitions
        let left_pressed = input.detect_left_press(buttons);
        let left_held = InputState::left_held(buttons);

        // A click while the menu is open belongs to the menu, whether it lands
        // on a row or dismisses it.
        if left_pressed && desktop_menu.is_open() {
            if let Some(desktop_menu::Outcome::NextBackground) = desktop_menu.click(mx, my) {
                background = (background + 1) % backgrounds.len();
                desktop_cache = compositor::build_desktop_cache(
                    screen.width(),
                    screen.height(),
                    &backgrounds[background],
                );
                dirty.full_screen = true;
            }
        } else if left_pressed {
            handle_mouse_press(
                windows,
                mx,
                my,
                &mut drag_state,
                &mut resize_state,
                focused_window_id,
                screen.info().width as u32,
                screen.info().height as u32,
                &mut maximized,
            );
        }
        handle_drag(&mut drag_state, mx, my, left_held);
        handle_resize(&mut resize_state, mx, my, left_held);

        // Re-fetch window list after potential modifications
        let window_count = match window_list(&mut entries) {
            Ok(count) => count.min(MAX_WINDOWS),
            Err(_) => 0,
        };

        let windows = &entries[..window_count];
        let focused_window_id = focused_id(windows);

        // Validate interaction state against current window list
        validate_window_state(windows, &mut drag_state, &mut resize_state);

        // Detect dirty regions from window changes relative to previous frame.
        let screen_w = screen.width() as u32;
        let screen_h = screen.height() as u32;

        if !dirty.full_screen {
            // Mark windows that are damaged, newly appeared, or have a
            // SHM buffer (active rendering clients like the terminal).
            for w in windows.iter() {
                if w.visible == 0 {
                    continue;
                }
                let prev = prev_windows[..prev_window_count]
                    .iter()
                    .flatten()
                    .find(|s| s.id == w.id);
                // A focus change repaints the title-bar accent, so it is dirty
                // even when nothing else about the window moved.
                let focus_changed = prev.is_some_and(|s| s.focused != w.focused);
                let has_buffer = w.buffer_shm_id != 0;
                if w.damaged != 0 || prev.is_none() || focus_changed || has_buffer {
                    let s = PrevWindowState::from_entry(w);
                    if let Some(r) = s.dirty_rect().clipped(screen_w, screen_h) {
                        dirty.mark_dirty(r);
                    }
                }
            }

            // Mark old rects of moved or disappeared windows (exposes background).
            for slot in prev_windows[..prev_window_count].iter().flatten() {
                let still_here = windows
                    .iter()
                    .any(|w| w.id == slot.id && w.x == slot.x && w.y == slot.y);
                if !still_here {
                    if let Some(r) = slot.dirty_rect().clipped(screen_w, screen_h) {
                        dirty.mark_dirty(r);
                    }
                }
            }

            // Mark cursor regions dirty when cursor moved.
            if prev_cursor_x != cursor.x || prev_cursor_y != cursor.y {
                if let Some(r) =
                    DirtyRect::new(prev_cursor_x, prev_cursor_y, CURSOR_SIZE, CURSOR_SIZE)
                        .clipped(screen_w, screen_h)
                {
                    dirty.mark_dirty(r);
                }
                if let Some(r) = DirtyRect::new(cursor.x, cursor.y, CURSOR_SIZE, CURSOR_SIZE)
                    .clipped(screen_w, screen_h)
                {
                    dirty.mark_dirty(r);
                }
            }
            prev_cursor_x = cursor.x;
            prev_cursor_y = cursor.y;

            // Snapshot current windows for next frame's change detection.
            prev_window_count = window_count;
            for i in 0..MAX_WINDOWS {
                prev_windows[i] = if i < window_count && windows[i].visible != 0 {
                    Some(PrevWindowState::from_entry(&windows[i]))
                } else {
                    None
                };
            }
        }

        // Compute which window's close button the cursor is hovering over.
        // Windows are sorted back-to-front by z_order, so iterate in reverse
        // (front-to-back) and take the first close-button hit.
        hovered_close_window = windows
            .iter()
            .filter(|w| w.visible != 0)
            .rev()
            .find(|w| {
                decorations::hit_test(w, cursor.x, cursor.y) == decorations::HitRegion::CloseButton
            })
            .map(|w| w.id);

        // Update cursor shape
        cursor.set_shape(determine_cursor_shape(
            windows,
            &drag_state,
            &resize_state,
            cursor.x,
            cursor.y,
        ));

        // Composite all windows into back buffer (always full composite).
        compositor::composite(
            &mut screen,
            windows,
            &cursor,
            focused_window_id,
            &mut shm_cache,
            hovered_close_window,
            hw_cursor,
            &desktop_cache,
        );
        // Painted after compositing, so its rectangle has to be sent even
        // when nothing else on the frame moved.
        if let Some((ox, oy)) = desktop_menu.origin() {
            desktop_menu.draw(&mut screen);
            dirty.mark_dirty(dirty::DirtyRect::new(
                ox,
                oy,
                desktop_menu::WIDTH as u32,
                desktop_menu::height() as u32,
            ));
        }

        // Transfer the dirty region to the host and flush.
        // With single-buffered virtio-gpu, we only need to transfer the
        // pixels that changed, even though the compositor rewrites everything.
        if dirty.full_screen {
            screen.flip();
        } else if let Some(bounds) = dirty.merged_bounds() {
            if let Some(clipped) = bounds.clipped(screen.width() as u32, screen.height() as u32) {
                screen.flip_rect(clipped.x as u32, clipped.y as u32, clipped.w, clipped.h);
            }
        }
        dirty.clear();
        // Sleep remainder of frame budget to maintain frame rate.
        // Use a minimum sleep of 1ms to avoid sub-microsecond sleeps that
        // can interact badly with the scheduler.
        let frame_target = Duration::from_millis(frame_time_ms);
        let elapsed = frame_start.elapsed();
        if elapsed < frame_target {
            let remaining = frame_target - elapsed;
            if remaining > Duration::from_millis(1) {
                std::thread::sleep(remaining);
            } else {
                std::thread::yield_now();
            }
        }
    }
}
