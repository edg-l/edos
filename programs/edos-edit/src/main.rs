//! edos-edit: the graphical text editor.
//!
//! A window with a file tree, tabs, and one editor pane, in the shape a
//! person who uses VS Code already knows. This phase is the read-only
//! viewer: it opens a file, draws it on the character grid, and moves the
//! caret. Editing, selection, syntax, the tree, tabs and the prompt bar are
//! later phases.

mod buffer;
mod view;

use std::time::Duration;

use edos_lib::keymap::{Modifiers, keycode, map_keycode, update_modifiers};
use edos_render::window::{Window, WindowEvent, WindowEventType};

use buffer::{Buffer, Edit, Position};

/// Opening size.
const WIN_W: u32 = 1000;
const WIN_H: u32 = 680;

/// Rows moved per notch of the wheel.
const SCROLL_STEP: usize = 3;

/// What Tab inserts. The spec's language table replaces this with the open
/// file's own indent.
const TAB_INDENT: &str = "    ";

struct App {
    window: Window,
    buffer: Buffer,
    layout: view::Layout,
    mods: Modifiers,
    sidebar_open: bool,
    /// What the last save, undo or redo reported, shown in the status strip
    /// until the next edit. `true` marks a failure.
    status: Option<(String, bool)>,
}

impl App {
    fn new(window: Window, buffer: Buffer) -> Self {
        let layout =
            view::Layout::new(window.width, window.height, true, false, buffer.lines.len());
        let mut app = Self {
            window,
            buffer,
            layout,
            mods: Modifiers::default(),
            sidebar_open: true,
            status: None,
        };
        app.buffer.clamp_cursor();
        app.update_title();
        app
    }

    /// The window title: the buffer's name, with a trailing dot when it
    /// carries changes the disk does not have.
    fn update_title(&mut self) {
        let name = buffer_name(&self.buffer);
        let title = if self.buffer.dirty {
            format!("{name} \u{2022}")
        } else {
            name.to_string()
        };
        let _ = self.window.set_title(&title);
    }

    // --- Cursor movement ------------------------------------------------

    fn move_cursor_col(&mut self, delta: i32) {
        let mut pos = self.buffer.cursor;
        if delta < 0 {
            if pos.col > 0 {
                pos.col -= 1;
            } else if pos.line > 0 {
                pos.line -= 1;
                pos.col = self.buffer.line_chars(pos.line);
            }
        } else if delta > 0 {
            let len = self.buffer.line_chars(pos.line);
            if pos.col < len {
                pos.col += 1;
            } else if pos.line + 1 < self.buffer.lines.len() {
                pos.line += 1;
                pos.col = 0;
            }
        }
        self.buffer.cursor = pos;
        self.buffer.break_coalesce();
        self.scroll_into_view();
    }

    fn move_cursor_line(&mut self, delta: i32) {
        let last = self.buffer.lines.len() as i32 - 1;
        let line = (self.buffer.cursor.line as i32 + delta).clamp(0, last.max(0)) as usize;
        self.buffer.cursor.line = line;
        self.buffer.clamp_cursor();
        self.buffer.break_coalesce();
        self.scroll_into_view();
    }

    fn set_cursor(&mut self, pos: Position) {
        self.buffer.cursor = pos;
        self.buffer.clamp_cursor();
        self.buffer.break_coalesce();
        self.scroll_into_view();
    }

    // --- Editing ----------------------------------------------------------

    /// Insert `text` at the cursor as one log entry, and move the cursor
    /// past it. Consecutive single-character calls coalesce in the log
    /// unless `text` is `"\n"`, which always starts a fresh entry.
    fn insert_str(&mut self, text: &str) {
        self.status = None;
        let cursor_before = self.buffer.cursor;
        let end = self.buffer.insert_text(cursor_before, text);
        self.buffer.push_edit(
            Edit::Insert {
                at: cursor_before,
                text: text.to_string(),
            },
            cursor_before,
        );
        self.buffer.cursor = end;
        if text == "\n" {
            self.buffer.break_coalesce();
        }
        self.scroll_into_view();
        self.update_title();
    }

    /// Delete the character before the cursor, joining with the previous
    /// line at column 0.
    fn backspace(&mut self) {
        self.status = None;
        let cursor = self.buffer.cursor;
        let from = if cursor.col > 0 {
            Position {
                line: cursor.line,
                col: cursor.col - 1,
            }
        } else if cursor.line > 0 {
            Position {
                line: cursor.line - 1,
                col: self.buffer.line_chars(cursor.line - 1),
            }
        } else {
            return;
        };
        let removed = self.buffer.delete_range(from, cursor);
        self.buffer.push_edit(
            Edit::Delete {
                at: from,
                text: removed,
            },
            cursor,
        );
        self.buffer.cursor = from;
        self.buffer.break_coalesce();
        self.scroll_into_view();
        self.update_title();
    }

    /// Delete the character after the cursor, joining with the next line at
    /// its end.
    fn delete_forward(&mut self) {
        self.status = None;
        let cursor = self.buffer.cursor;
        let last_line = self.buffer.lines.len() - 1;
        let to = if cursor.col < self.buffer.line_chars(cursor.line) {
            Position {
                line: cursor.line,
                col: cursor.col + 1,
            }
        } else if cursor.line < last_line {
            Position {
                line: cursor.line + 1,
                col: 0,
            }
        } else {
            return;
        };
        let removed = self.buffer.delete_range(cursor, to);
        self.buffer.push_edit(
            Edit::Delete {
                at: cursor,
                text: removed,
            },
            cursor,
        );
        self.buffer.break_coalesce();
        self.scroll_into_view();
        self.update_title();
    }

    fn save(&mut self) {
        self.status = Some(match self.buffer.save() {
            Ok(()) => ("Saved.".to_string(), false),
            Err(err) => (err, true),
        });
        self.update_title();
    }

    fn undo(&mut self) {
        self.buffer.undo();
        self.scroll_into_view();
        self.update_title();
    }

    fn redo(&mut self) {
        self.buffer.redo();
        self.scroll_into_view();
        self.update_title();
    }

    /// Replace the document with an empty, unsaved one.
    fn new_buffer(&mut self) {
        self.buffer = Buffer::empty();
        self.buffer.clamp_cursor();
        self.status = None;
        self.update_title();
    }

    /// Keep the cursor inside the visible rows and columns, exactly as
    /// `edos-files` keeps the selected row inside the listing.
    fn scroll_into_view(&mut self) {
        let rows = self.layout.rows_visible.max(1);
        if self.buffer.cursor.line < self.buffer.scroll_line {
            self.buffer.scroll_line = self.buffer.cursor.line;
        } else if self.buffer.cursor.line >= self.buffer.scroll_line + rows {
            self.buffer.scroll_line = self.buffer.cursor.line + 1 - rows;
        }

        let cols = self.layout.cols_visible.max(1);
        if self.buffer.cursor.col < self.buffer.scroll_col {
            self.buffer.scroll_col = self.buffer.cursor.col;
        } else if self.buffer.cursor.col >= self.buffer.scroll_col + cols {
            self.buffer.scroll_col = self.buffer.cursor.col + 1 - cols;
        }
    }

    // --- Events -----------------------------------------------------------

    /// Returns false when the window should close.
    fn handle(&mut self, event: &WindowEvent) -> bool {
        match event.event_type() {
            Some(WindowEventType::CloseRequested) => return false,
            Some(WindowEventType::Resize) => {
                let (width, height) = (event.x as u32, event.y as u32);
                let _ = self.window.resize(width, height);
            }
            Some(WindowEventType::MouseButton) => self.on_mouse_button(event),
            Some(WindowEventType::MouseScroll) => self.on_scroll(event.data as i32),
            Some(WindowEventType::KeyPress) => {
                // A modifier key changes state and produces no character of
                // its own, so it is handled and consumed here rather than
                // falling through to the movement dispatch below.
                if update_modifiers(&mut self.mods, event.code, true) {
                    return true;
                }
                self.on_key(event.code);
            }
            Some(WindowEventType::KeyRelease) => {
                update_modifiers(&mut self.mods, event.code, false);
            }
            _ => {}
        }
        true
    }

    fn on_mouse_button(&mut self, event: &WindowEvent) {
        // Only a left-button press acts; nothing else is bound yet.
        if event.data == 0 || event.code != 0 {
            return;
        }
        if !self.layout.pane.contains(event.x, event.y) {
            return;
        }
        let Some(line) = view::line_at(&self.layout, self.buffer.scroll_line, event.y) else {
            return;
        };
        let line = line.min(self.buffer.lines.len().saturating_sub(1));
        let col = view::col_at(&self.layout, self.buffer.scroll_col, event.x)
            .min(self.buffer.line_chars(line));
        self.set_cursor(Position { line, col });
    }

    fn on_scroll(&mut self, delta: i32) {
        if delta > 0 {
            self.buffer.scroll_line = self.buffer.scroll_line.saturating_sub(SCROLL_STEP);
        } else if delta < 0 {
            let max = self
                .buffer
                .lines
                .len()
                .saturating_sub(self.layout.rows_visible.max(1));
            self.buffer.scroll_line = (self.buffer.scroll_line + SCROLL_STEP).min(max);
        }
    }

    fn on_key(&mut self, code: u32) {
        let rows = self.layout.rows_visible.max(1) as i32;
        match code {
            keycode::ARROW_LEFT => self.move_cursor_col(-1),
            keycode::ARROW_RIGHT => self.move_cursor_col(1),
            keycode::ARROW_UP => self.move_cursor_line(-1),
            keycode::ARROW_DOWN => self.move_cursor_line(1),
            keycode::PAGE_UP => self.move_cursor_line(-rows),
            keycode::PAGE_DOWN => self.move_cursor_line(rows),
            keycode::HOME if self.mods.ctrl => self.set_cursor(Position::default()),
            keycode::END if self.mods.ctrl => {
                let last = self.buffer.lines.len() - 1;
                let col = self.buffer.line_chars(last);
                self.set_cursor(Position { line: last, col });
            }
            keycode::HOME => {
                let line = self.buffer.cursor.line;
                self.set_cursor(Position { line, col: 0 });
            }
            keycode::END => {
                let line = self.buffer.cursor.line;
                let col = self.buffer.line_chars(line);
                self.set_cursor(Position { line, col });
            }
            keycode::S if self.mods.ctrl => self.save(),
            keycode::Z if self.mods.ctrl => self.undo(),
            keycode::Y if self.mods.ctrl => self.redo(),
            keycode::N if self.mods.ctrl => self.new_buffer(),
            keycode::RETURN | keycode::NUMPAD_ENTER if !self.mods.ctrl => self.insert_str("\n"),
            keycode::BACKSPACE if !self.mods.ctrl => self.backspace(),
            keycode::DELETE if !self.mods.ctrl => self.delete_forward(),
            keycode::TAB if !self.mods.ctrl => self.insert_str(TAB_INDENT),
            _ if !self.mods.ctrl => {
                if let Some(ch) = map_keycode(code, &self.mods)
                    && !ch.is_control()
                {
                    self.insert_str(&ch.to_string());
                }
            }
            _ => {}
        }
    }

    // --- Drawing ------------------------------------------------------------

    fn draw(&mut self) {
        // Derived state, rebuilt every frame: the gutter width, the sidebar
        // and the prompt all change during ordinary use, and a `Layout` built
        // only on resize puts clicks on the wrong character the moment any of
        // them do.
        let (width, height) = (self.window.width, self.window.height);
        self.layout = view::Layout::new(
            width,
            height,
            self.sidebar_open,
            false,
            self.buffer.lines.len(),
        );

        let name = buffer_name(&self.buffer).to_string();
        let root = root_label(&self.buffer);
        let encoding = if self.buffer.repaired {
            "UTF-8 (repaired)"
        } else {
            "UTF-8"
        };
        let volume = volume_label(self.buffer.path.as_deref());

        let Some(buf) = self.window.buffer_mut() else {
            return;
        };
        let mut canvas = view::Canvas { buf, width, height };
        canvas.fill(
            edos_render::widgets::Rect::new(0, 0, width, height),
            edos_render::theme::Theme::DEFAULT.background.raw(),
        );

        view::draw_tabs(&mut canvas, self.layout.tabs, &name);
        if let Some(rect) = self.layout.sidebar {
            view::draw_sidebar(&mut canvas, rect, &root);
        }
        view::draw_pane(&mut canvas, &self.layout, &self.buffer);
        let note = self
            .status
            .as_ref()
            .map(|(message, warning)| (message.as_str(), *warning));
        view::draw_status(
            &mut canvas,
            &self.layout,
            &name,
            "Plain text",
            self.buffer.cursor.line + 1,
            self.buffer.cursor.col + 1,
            "4 spaces",
            encoding,
            &volume,
            note,
        );

        self.window.swap_buffers();
    }
}

/// The name a buffer is known by: its file name, or `untitled` for one with
/// no path yet.
fn buffer_name(buffer: &Buffer) -> &str {
    match &buffer.path {
        Some(path) => path.rsplit('/').next().unwrap_or(path),
        None => "untitled",
    }
}

/// The name of the directory the sidebar is rooted at: the open file's
/// directory, or the working directory for an unsaved buffer.
fn root_label(buffer: &Buffer) -> String {
    let dir = match &buffer.path {
        Some(path) => match path.rsplit_once('/') {
            Some(("", _)) => "/".to_string(),
            Some((dir, _)) => dir.to_string(),
            None => ".".to_string(),
        },
        None => std::env::current_dir()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|_| "/".to_string()),
    };
    match dir.rsplit('/').next() {
        Some("") | None => "/".to_string(),
        Some(name) => name.to_string(),
    }
}

/// What the status strip's right side says: the filesystem and device under
/// the open file, `efs · dev0p2` the way `mount` and `df` would name them.
/// Empty when nothing covers the path, which does not happen for a real one.
fn volume_label(path: Option<&str>) -> String {
    let target = path.unwrap_or("/");
    let mounts = edos_lib::mounts::list();
    match edos_lib::mounts::covering(&mounts, target) {
        Some(mount) => format!(
            "{} · {}",
            mount.filesystem.name(),
            mount
                .filesystem
                .device_label(mount.device_id, mount.partition)
        ),
        None => String::new(),
    }
}

fn main() {
    let path = std::env::args().nth(1);
    let buffer = match &path {
        Some(path) => Buffer::open(path).unwrap_or_else(|err| {
            eprintln!("edos-edit: {err}");
            Buffer::empty()
        }),
        None => Buffer::empty(),
    };

    let window = match Window::new(140, 20, WIN_W, WIN_H) {
        Ok(window) => window,
        Err(err) => {
            eprintln!("edos-edit: could not open a window: {err:?}");
            std::process::exit(1);
        }
    };

    let mut app = App::new(window, buffer);
    if app.window.show().is_err() {
        eprintln!("edos-edit: could not show the window");
        std::process::exit(1);
    }

    let mut events = [WindowEvent::default(); 16];
    loop {
        if let Ok(count) = app.window.poll_events(&mut events) {
            for event in &events[..count] {
                if !app.handle(event) {
                    return;
                }
            }
        }
        app.draw();
        std::thread::sleep(Duration::from_millis(16));
    }
}
