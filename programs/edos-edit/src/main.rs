//! edos-edit: the graphical text editor.
//!
//! A window with a file tree, tabs, and one editor pane, in the shape a
//! person who uses VS Code already knows: click a file in the tree, type into
//! it, Ctrl+S. Selection, undo/redo, save, syntax colouring, find, go-to-line
//! and the open prompt all work through the same document the tab strip
//! switches between.

mod buffer;
mod syntax;
mod tree;
mod view;

use edos_render::widgets::Canvas;
use std::time::Duration;

use edos_lib::clipboard;
use edos_lib::keymap::{Modifiers, keycode, map_keycode, update_modifiers};
use edos_render::widgets::{TextInput, WidgetContainer, WidgetEvent, WidgetId};
use edos_render::window::{Window, WindowEvent, WindowEventType};

use buffer::{Buffer, Edit, Position};

/// Opening size.
const WIN_W: u32 = 1000;
const WIN_H: u32 = 680;

/// Rows moved per notch of the wheel.
const SCROLL_STEP: usize = 3;

/// Keys that move the cursor without editing, and so extend or clear a
/// selection depending on whether Shift is held — as opposed to a shortcut
/// like Ctrl+S, which leaves any selection alone.
fn is_movement_key(code: u32) -> bool {
    matches!(
        code,
        keycode::ARROW_LEFT
            | keycode::ARROW_RIGHT
            | keycode::ARROW_UP
            | keycode::ARROW_DOWN
            | keycode::PAGE_UP
            | keycode::PAGE_DOWN
            | keycode::HOME
            | keycode::END
    )
}

/// What the pointer is over, recomputed on every `MouseMove`.
#[derive(Default, Clone, Copy)]
struct Hover {
    tree: Option<usize>,
    tab: Option<usize>,
    /// Whether the pointer sits inside `tab`'s close cell specifically.
    tab_close: bool,
}

/// What the shared prompt bar at the pane's foot is doing. Only one at a
/// time — opening one replaces whatever was already open.
#[derive(Clone, Copy, PartialEq, Eq)]
enum PromptKind {
    Find,
    GoTo,
    Open,
}

/// The label the prompt bar shows for `kind`.
fn prompt_label(kind: PromptKind) -> &'static str {
    match kind {
        PromptKind::Find => "Find",
        PromptKind::GoTo => "Line",
        PromptKind::Open => "Open",
    }
}

/// The prompt bar's live state: a real `TextInput` in a `WidgetContainer`,
/// built and fed exactly the way `edos-files::Rename` builds its inline
/// field.
struct PromptState {
    kind: PromptKind,
    widgets: WidgetContainer,
    field: WidgetId,
}

struct App {
    window: Window,
    buffers: Vec<Buffer>,
    active: usize,
    layout: view::Layout,
    mods: Modifiers,
    sidebar_open: bool,
    /// Whether the left button is down over the pane, extending the
    /// selection as the pointer moves. Follows `edos-files`' `dragging_scroll`.
    dragging: bool,
    /// What the last save, undo or redo reported, shown in the status strip
    /// until the next edit. `true` marks a failure.
    status: Option<(String, bool)>,
    tree: tree::Tree,
    hover: Hover,
    /// The buffer index a Ctrl+W or a tab's close click already refused once
    /// because it carried unsaved changes. A second press on the same tab
    /// closes it; switching tabs or closing anything else clears this.
    close_confirm: Option<usize>,
    /// The find/go-to-line/open prompt, while one is open.
    prompt: Option<PromptState>,
    /// What Go To Line or Open has to report after a failed `Submit` — "Not a
    /// line number", or an `io::Error`'s message. Find's own report is
    /// computed live from `find_matches` instead, since it changes on every
    /// keystroke rather than only on submission.
    prompt_message: String,
    /// Every literal, case-insensitive match of the find field's text across
    /// the whole buffer, recomputed on every keystroke.
    find_matches: Vec<(Position, usize)>,
    /// Index into `find_matches` of the one Enter or Shift+Enter last walked
    /// to.
    find_current: Option<usize>,
}

impl App {
    fn new(window: Window, buffer: Buffer) -> Self {
        let layout =
            view::Layout::new(window.width, window.height, true, false, buffer.lines.len());
        let tree = tree::Tree::new(&root_dir(&buffer));
        let mut app = Self {
            window,
            buffers: vec![buffer],
            active: 0,
            layout,
            mods: Modifiers::default(),
            sidebar_open: true,
            dragging: false,
            status: None,
            tree,
            hover: Hover::default(),
            close_confirm: None,
            prompt: None,
            prompt_message: String::new(),
            find_matches: Vec::new(),
            find_current: None,
        };
        app.buffer_mut().clamp_cursor();
        app.update_title();
        app
    }

    fn buffer(&self) -> &Buffer {
        &self.buffers[self.active]
    }

    fn buffer_mut(&mut self) -> &mut Buffer {
        &mut self.buffers[self.active]
    }

    /// The name every open buffer is shown by in the tab strip, in order.
    fn tab_labels(&self) -> Vec<String> {
        self.buffers
            .iter()
            .map(|buffer| buffer_name(buffer).to_string())
            .collect()
    }

    /// The window title: the active buffer's name, with a trailing dot when
    /// it carries changes the disk does not have.
    fn update_title(&mut self) {
        let name = buffer_name(self.buffer());
        let title = if self.buffer().dirty {
            format!("{name} \u{2022}")
        } else {
            name.to_string()
        };
        let _ = self.window.set_title(&title);
    }

    // --- Cursor movement ------------------------------------------------

    fn move_cursor_col(&mut self, delta: i32) {
        let mut pos = self.buffer().cursor;
        if delta < 0 {
            if pos.col > 0 {
                pos.col -= 1;
            } else if pos.line > 0 {
                pos.line -= 1;
                pos.col = self.buffer().line_chars(pos.line);
            }
        } else if delta > 0 {
            let len = self.buffer().line_chars(pos.line);
            if pos.col < len {
                pos.col += 1;
            } else if pos.line + 1 < self.buffer().lines.len() {
                pos.line += 1;
                pos.col = 0;
            }
        }
        self.buffer_mut().cursor = pos;
        self.buffer_mut().break_coalesce();
        self.scroll_into_view();
    }

    fn move_cursor_line(&mut self, delta: i32) {
        let last = self.buffer().lines.len() as i32 - 1;
        let line = (self.buffer().cursor.line as i32 + delta).clamp(0, last.max(0)) as usize;
        self.buffer_mut().cursor.line = line;
        self.buffer_mut().clamp_cursor();
        self.buffer_mut().break_coalesce();
        self.scroll_into_view();
    }

    fn set_cursor(&mut self, pos: Position) {
        self.buffer_mut().cursor = pos;
        self.buffer_mut().clamp_cursor();
        self.buffer_mut().break_coalesce();
        self.scroll_into_view();
    }

    // --- Editing ----------------------------------------------------------

    /// Insert `text` at the cursor as one log entry, and move the cursor
    /// past it. A selection under the cursor is replaced as a single
    /// `Edit::Replace` rather than a delete followed by an insert.
    /// Consecutive single-character calls coalesce in the log unless `text`
    /// is `"\n"`, which always starts a fresh entry.
    fn insert_str(&mut self, text: &str) {
        self.status = None;
        if let Some(end) = self.buffer_mut().replace_selection(text) {
            self.buffer_mut().cursor = end;
            self.scroll_into_view();
            self.update_title();
            return;
        }
        let cursor_before = self.buffer().cursor;
        let end = self.buffer_mut().insert_text(cursor_before, text);
        self.buffer_mut().push_edit(
            Edit::Insert {
                at: cursor_before,
                text: text.to_string(),
            },
            cursor_before,
        );
        self.buffer_mut().cursor = end;
        if text == "\n" {
            self.buffer_mut().break_coalesce();
        }
        self.scroll_into_view();
        self.update_title();
    }

    /// Delete the character before the cursor, joining with the previous
    /// line at column 0. Deletes the selection instead when there is one.
    fn backspace(&mut self) {
        self.status = None;
        if self.buffer_mut().delete_selection() {
            self.scroll_into_view();
            self.update_title();
            return;
        }
        let cursor = self.buffer().cursor;
        let from = if cursor.col > 0 {
            Position {
                line: cursor.line,
                col: cursor.col - 1,
            }
        } else if cursor.line > 0 {
            Position {
                line: cursor.line - 1,
                col: self.buffer().line_chars(cursor.line - 1),
            }
        } else {
            return;
        };
        let removed = self.buffer_mut().delete_range(from, cursor);
        self.buffer_mut().push_edit(
            Edit::Delete {
                at: from,
                text: removed,
            },
            cursor,
        );
        self.buffer_mut().cursor = from;
        self.buffer_mut().break_coalesce();
        self.scroll_into_view();
        self.update_title();
    }

    /// Delete the character after the cursor, joining with the next line at
    /// its end. Deletes the selection instead when there is one.
    fn delete_forward(&mut self) {
        self.status = None;
        if self.buffer_mut().delete_selection() {
            self.scroll_into_view();
            self.update_title();
            return;
        }
        let cursor = self.buffer().cursor;
        let last_line = self.buffer().lines.len() - 1;
        let to = if cursor.col < self.buffer().line_chars(cursor.line) {
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
        let removed = self.buffer_mut().delete_range(cursor, to);
        self.buffer_mut().push_edit(
            Edit::Delete {
                at: cursor,
                text: removed,
            },
            cursor,
        );
        self.buffer_mut().break_coalesce();
        self.scroll_into_view();
        self.update_title();
    }

    fn save(&mut self) {
        let saved = self.buffer_mut().save();
        let ok = saved.is_ok();
        self.status = Some(match saved {
            Ok(()) => ("Saved.".to_string(), false),
            Err(err) => (err, true),
        });
        if ok {
            self.refresh_tree();
        }
        self.update_title();
    }

    fn undo(&mut self) {
        self.buffer_mut().undo();
        self.scroll_into_view();
        self.update_title();
    }

    fn redo(&mut self) {
        self.buffer_mut().redo();
        self.scroll_into_view();
        self.update_title();
    }

    // --- Selection and clipboard --------------------------------------

    /// Select the whole document.
    fn select_all(&mut self) {
        self.buffer_mut().anchor = Some(Position::default());
        let last = self.buffer().lines.len() - 1;
        let col = self.buffer().line_chars(last);
        self.set_cursor(Position { line: last, col });
    }

    fn copy(&mut self) {
        if let Some(text) = self.buffer().selected_text() {
            let _ = clipboard::set(clipboard::Buffer::Clipboard, &text);
        }
    }

    /// Copy the selection to the clipboard, then delete it as one log entry.
    fn cut(&mut self) {
        let Some(text) = self.buffer().selected_text() else {
            return;
        };
        let _ = clipboard::set(clipboard::Buffer::Clipboard, &text);
        self.status = None;
        self.buffer_mut().delete_selection();
        self.scroll_into_view();
        self.update_title();
    }

    /// Insert the clipboard's contents at the cursor, replacing the
    /// selection when there is one — through `insert_str`, so either shape
    /// lands as a single log entry.
    fn paste(&mut self) {
        let text = clipboard::get(clipboard::Buffer::Clipboard);
        if !text.is_empty() {
            self.insert_str(&text);
        }
    }

    // --- Tabs -------------------------------------------------------------

    /// Open a fresh, unsaved buffer in a new tab and switch to it.
    fn new_buffer(&mut self) {
        self.buffers.push(Buffer::empty());
        self.active = self.buffers.len() - 1;
        self.close_confirm = None;
        self.status = None;
        self.buffer_mut().clamp_cursor();
        self.update_title();
    }

    /// Switch to `path`'s tab, opening it fresh only when it is not already
    /// open — clicking the same file in the tree twice does not duplicate
    /// its tab.
    fn open_path(&mut self, path: &str) {
        if let Some(index) = self
            .buffers
            .iter()
            .position(|buffer| buffer.path.as_deref() == Some(path))
        {
            self.active = index;
            self.close_confirm = None;
            self.status = None;
            self.update_title();
            return;
        }
        match Buffer::open(path) {
            Ok(buffer) => {
                self.buffers.push(buffer);
                self.active = self.buffers.len() - 1;
                self.status = None;
            }
            Err(err) => self.status = Some((err, true)),
        }
        self.buffer_mut().clamp_cursor();
        self.update_title();
    }

    fn next_tab(&mut self) {
        if self.buffers.len() > 1 {
            self.active = (self.active + 1) % self.buffers.len();
            self.close_confirm = None;
            self.update_title();
        }
    }

    /// What Ctrl+W closes: the active tab, under the same refuse-once rule
    /// every tab's close cell follows.
    fn close_active(&mut self) {
        self.request_close(self.active);
    }

    /// Close the tab at `index`, refusing once with a status message when it
    /// carries unsaved changes; a second call for the same index — a second
    /// Ctrl+W, or a second click on the tab's own close cell — closes it.
    /// The last tab never actually closes — it is replaced with a fresh
    /// empty one, the same as Ctrl+N.
    fn request_close(&mut self, index: usize) {
        let Some(buffer) = self.buffers.get(index) else {
            return;
        };
        if buffer.dirty && self.close_confirm != Some(index) {
            self.close_confirm = Some(index);
            self.status = Some((
                "Unsaved changes \u{2014} press Ctrl+W again to close without saving.".to_string(),
                true,
            ));
            return;
        }

        self.close_confirm = None;
        self.buffers.remove(index);
        if self.buffers.is_empty() {
            self.buffers.push(Buffer::empty());
        }
        if self.active >= self.buffers.len() {
            self.active = self.buffers.len() - 1;
        } else if index < self.active {
            self.active -= 1;
        }
        self.status = None;
        self.buffer_mut().clamp_cursor();
        self.update_title();
    }

    fn toggle_sidebar(&mut self) {
        self.sidebar_open = !self.sidebar_open;
    }

    /// Keep the cursor inside the visible rows and columns, exactly as
    /// `edos-files` keeps the selected row inside the listing.
    fn scroll_into_view(&mut self) {
        let rows = self.layout.rows_visible.max(1);
        let cursor_line = self.buffer().cursor.line;
        if cursor_line < self.buffer().scroll_line {
            self.buffer_mut().scroll_line = cursor_line;
        } else if cursor_line >= self.buffer().scroll_line + rows {
            self.buffer_mut().scroll_line = cursor_line + 1 - rows;
        }

        let cols = self.layout.cols_visible.max(1);
        let cursor_col = self.buffer().cursor.col;
        if cursor_col < self.buffer().scroll_col {
            self.buffer_mut().scroll_col = cursor_col;
        } else if cursor_col >= self.buffer().scroll_col + cols {
            self.buffer_mut().scroll_col = cursor_col + 1 - cols;
        }
    }

    /// Keep the tree's scroll inside what it actually holds, after the
    /// window resizes or a directory collapses out from under it.
    fn clamp_tree_scroll(&mut self) {
        let Some(rect) = self.layout.sidebar else {
            return;
        };
        let visible = view::tree_rows_visible(rect).max(1);
        let max = self.tree.rows.len().saturating_sub(visible);
        self.tree.scroll = self.tree.scroll.min(max);
    }

    /// Re-read the sidebar. F5 asks for this, and so does the window regaining
    /// focus, which is what covers a file another program wrote while this one
    /// was in the background. Saving to a new path refreshes for the same
    /// reason: the editor's own write is a change the tree cannot see either.
    fn refresh_tree(&mut self) {
        self.tree.refresh();
        self.clamp_tree_scroll();
    }

    // --- Prompt bar ---------------------------------------------------------

    /// Open the shared prompt bar as `kind`, seeded the way each one wants —
    /// Find with the current selection, Open with the active buffer's own
    /// directory, Go To with nothing — and replacing whatever prompt was
    /// already open. Built the way `edos-files::begin_rename` builds its
    /// inline field: a fresh `WidgetContainer` holding one focused
    /// `TextInput`, positioned from the layout the prompt bar itself is
    /// about to occupy.
    fn open_prompt(&mut self, kind: PromptKind) {
        let (width, height) = (self.window.width, self.window.height);
        let layout = view::Layout::new(
            width,
            height,
            self.sidebar_open,
            true,
            self.buffer().lines.len(),
        );
        let Some(rect) = layout.prompt else {
            return;
        };
        let field_rect = view::prompt_field_rect(rect, prompt_label(kind));
        let seed = match kind {
            PromptKind::Find => self.buffer().selected_text().unwrap_or_default(),
            PromptKind::GoTo => String::new(),
            PromptKind::Open => root_dir(self.buffer()),
        };

        let mut field = TextInput::new(field_rect.x, field_rect.y, field_rect.width);
        field.set_text(&seed);
        let mut widgets = WidgetContainer::new();
        let field = widgets.add(field);
        widgets.set_focus(field);

        self.status = None;
        self.prompt_message = String::new();
        self.prompt = Some(PromptState {
            kind,
            widgets,
            field,
        });
        if kind == PromptKind::Find {
            self.recompute_find(&seed);
        }
    }

    /// Close the prompt bar, if one is open, and drop the find highlight
    /// that goes with it.
    fn dismiss_prompt(&mut self) {
        if self.prompt.take().is_some() {
            self.find_matches.clear();
            self.find_current = None;
            self.prompt_message.clear();
        }
    }

    /// What the prompt bar's report region says, on the right of the field.
    fn prompt_report(&self) -> String {
        let Some(prompt) = &self.prompt else {
            return String::new();
        };
        match prompt.kind {
            PromptKind::Find => {
                if self.find_matches.is_empty() {
                    "No matches".to_string()
                } else {
                    format!(
                        "{} of {}",
                        self.find_current.map_or(0, |i| i + 1),
                        self.find_matches.len()
                    )
                }
            }
            PromptKind::GoTo | PromptKind::Open => self.prompt_message.clone(),
        }
    }

    /// Feed one window event to the prompt's field, after the shortcuts in
    /// `on_key` have had no chance to also act on it — the same order
    /// `edos-files::feed_rename` follows.
    fn feed_prompt(&mut self, event: &WindowEvent) {
        let Some(prompt) = self.prompt.as_mut() else {
            return;
        };
        let mut result = None;
        for (id, widget_event) in prompt.widgets.handle_event(event) {
            if id == prompt.field {
                result = Some(widget_event);
            }
        }
        match result {
            Some(WidgetEvent::TextChanged(text)) => self.on_prompt_text_changed(text),
            Some(WidgetEvent::Submit(text)) => self.on_prompt_submit(text),
            _ => {}
        }
    }

    fn on_prompt_text_changed(&mut self, text: String) {
        self.prompt_message.clear();
        let kind = self.prompt.as_ref().map(|prompt| prompt.kind);
        if kind == Some(PromptKind::Find) {
            self.recompute_find(&text);
        }
    }

    fn on_prompt_submit(&mut self, text: String) {
        let Some(kind) = self.prompt.as_ref().map(|prompt| prompt.kind) else {
            return;
        };
        match kind {
            PromptKind::Find => self.find_step(!self.mods.shift),
            PromptKind::GoTo => self.submit_goto(&text),
            PromptKind::Open => self.submit_open(&text),
        }
    }

    /// Recompute every literal, case-insensitive match of `query` across the
    /// whole buffer, and make the first one current. Case folding is ASCII
    /// only, so it never changes a match's length against the original text.
    fn recompute_find(&mut self, query: &str) {
        let mut matches = Vec::new();
        if !query.is_empty() {
            let needle: Vec<char> = query.chars().map(|c| c.to_ascii_lowercase()).collect();
            for (line_index, line) in self.buffer().lines.iter().enumerate() {
                let haystack: Vec<char> =
                    line.text.chars().map(|c| c.to_ascii_lowercase()).collect();
                if haystack.len() < needle.len() {
                    continue;
                }
                for start in 0..=(haystack.len() - needle.len()) {
                    if haystack[start..start + needle.len()] == needle[..] {
                        matches.push((
                            Position {
                                line: line_index,
                                col: start,
                            },
                            needle.len(),
                        ));
                    }
                }
            }
        }
        self.find_current = (!matches.is_empty()).then_some(0);
        self.find_matches = matches;
    }

    /// Step to the next (`forward`) or previous match, wrapping at either
    /// end, scroll it into view, and select it.
    fn find_step(&mut self, forward: bool) {
        if self.find_matches.is_empty() {
            return;
        }
        let len = self.find_matches.len();
        let next = match self.find_current {
            Some(i) if forward => (i + 1) % len,
            Some(i) => (i + len - 1) % len,
            None => 0,
        };
        self.find_current = Some(next);
        let (pos, match_len) = self.find_matches[next];
        self.buffer_mut().anchor = Some(pos);
        self.set_cursor(Position {
            line: pos.line,
            col: pos.col + match_len,
        });
    }

    /// Go to the line `text` names, 1-based and clamped to the buffer, or
    /// report that it does not name one.
    fn submit_goto(&mut self, text: &str) {
        match text.trim().parse::<usize>() {
            Ok(n) if n >= 1 => {
                let last = self.buffer().lines.len().saturating_sub(1);
                let line = (n - 1).min(last);
                // Clears whatever a prior find left selected — Go To Line
                // moves the caret, it does not extend a selection.
                self.buffer_mut().clear_selection();
                self.set_cursor(Position { line, col: 0 });
                self.dismiss_prompt();
            }
            _ => self.prompt_message = "Not a line number".to_string(),
        }
    }

    /// Open `text` as a path, through the same `open_path` a tree click
    /// uses, and mirror its failure into the prompt bar rather than only the
    /// status strip.
    fn submit_open(&mut self, text: &str) {
        let path = text.trim();
        if path.is_empty() {
            return;
        }
        self.open_path(path);
        match &self.status {
            Some((message, true)) => self.prompt_message = message.clone(),
            _ => self.dismiss_prompt(),
        }
    }

    // --- Events -----------------------------------------------------------

    /// Returns false when the window should close.
    fn handle(&mut self, event: &WindowEvent) -> bool {
        // Whether the prompt was already open when this event arrived. A key
        // that opens it — Ctrl+F, Ctrl+G, Ctrl+O — must not also be typed
        // into the field it just opened, so this is captured before `on_key`
        // runs rather than read from `self.prompt` afterward.
        let was_prompting = self.prompt.is_some();

        match event.event_type() {
            Some(WindowEventType::CloseRequested) => return false,
            Some(WindowEventType::Resize) => {
                let (width, height) = (event.x as u32, event.y as u32);
                let _ = self.window.resize(width, height);
                self.dismiss_prompt();
                self.clamp_tree_scroll();
            }
            Some(WindowEventType::FocusGained) => self.refresh_tree(),
            Some(WindowEventType::MouseButton) => self.on_mouse_button(event),
            Some(WindowEventType::MouseMove) => self.on_mouse_move(event.x, event.y),
            Some(WindowEventType::MouseScroll) => {
                self.on_scroll(event.x, event.y, event.data as i32)
            }
            Some(WindowEventType::KeyPress) => {
                // A modifier key changes state and produces no character of
                // its own, so `on_key` is skipped for it — but the event
                // still has to reach the prompt bar below, exactly as its own
                // `KeyRelease` does. Returning early here instead would leave
                // the prompt's own modifier tracking permanently stuck: it
                // would see every Ctrl release but never the matching press,
                // so `WidgetContainer`'s Ctrl+C/X/V would never fire while a
                // prompt is open.
                if !update_modifiers(&mut self.mods, event.code, true) {
                    self.on_key(event.code);
                }
            }
            Some(WindowEventType::KeyRelease) => {
                update_modifiers(&mut self.mods, event.code, false);
            }
            _ => {}
        }

        // The prompt's field is the only thing that wants raw keys and
        // characters, and it wants them after the shortcuts above have had
        // their say — never before, or the field would starve; never on the
        // same keystroke that opened it, or that keystroke would land in it
        // twice.
        if was_prompting
            && self.prompt.is_some()
            && !matches!(event.event_type(), Some(WindowEventType::MouseMove))
        {
            self.feed_prompt(event);
        }
        true
    }

    /// The buffer position under `(x, y)`, or None when `y` falls outside
    /// the pane's rows. Column is clamped to the line's length and line to
    /// the last line in the buffer, the same clamping `line_at`/`col_at`'s
    /// callers already did before this helper existed.
    fn pane_position(&self, x: i32, y: i32) -> Option<Position> {
        let buffer = self.buffer();
        let line = view::line_at(&self.layout, buffer.scroll_line, y)?;
        let line = line.min(buffer.lines.len().saturating_sub(1));
        let col = view::col_at(&self.layout, buffer.scroll_col, x).min(buffer.line_chars(line));
        Some(Position { line, col })
    }

    fn on_mouse_button(&mut self, event: &WindowEvent) {
        if event.data == 0 {
            self.dragging = false;
            return;
        }
        // Only a left-button press acts; nothing else is bound yet.
        if event.code != 0 {
            return;
        }
        let (x, y) = (event.x, event.y);

        if let Some(rect) = self.layout.sidebar
            && rect.contains(x, y)
        {
            if let Some(index) = view::tree_row_at(rect, self.tree.scroll, y)
                && index < self.tree.rows.len()
            {
                if self.tree.rows[index].is_dir {
                    self.tree.toggle(index);
                    self.clamp_tree_scroll();
                } else {
                    let path = self.tree.rows[index].path.clone();
                    self.open_path(&path);
                }
            }
            return;
        }

        if self.layout.tabs.contains(x, y) {
            let labels = self.tab_labels();
            let rects = view::tab_rects(self.layout.tabs, &labels);
            if let Some(index) = rects.iter().position(|rect| rect.contains(x, y)) {
                if view::tab_close_rect(rects[index]).contains(x, y) {
                    self.request_close(index);
                } else if self.active != index {
                    self.active = index;
                    self.close_confirm = None;
                    self.status = None;
                    self.update_title();
                }
            }
            return;
        }

        if !self.layout.pane.contains(x, y) {
            return;
        }
        let Some(pos) = self.pane_position(x, y) else {
            return;
        };
        self.buffer_mut().anchor = Some(pos);
        self.set_cursor(pos);
        self.dragging = true;
    }

    /// Extend the selection to the pointer while the button is held over the
    /// pane; otherwise recompute what the pointer is hovering, for the tree's
    /// highlight and the tab strip's close cells.
    fn on_mouse_move(&mut self, x: i32, y: i32) {
        if self.dragging {
            if let Some(pos) = self.pane_position(x, y) {
                self.set_cursor(pos);
            }
            return;
        }

        let tree_hover = self.layout.sidebar.and_then(|rect| {
            if !rect.contains(x, y) {
                return None;
            }
            view::tree_row_at(rect, self.tree.scroll, y)
                .filter(|index| *index < self.tree.rows.len())
        });

        let mut hover = Hover {
            tree: tree_hover,
            tab: None,
            tab_close: false,
        };
        if self.layout.tabs.contains(x, y) {
            let labels = self.tab_labels();
            let rects = view::tab_rects(self.layout.tabs, &labels);
            if let Some(index) = rects.iter().position(|rect| rect.contains(x, y)) {
                hover.tab = Some(index);
                hover.tab_close = view::tab_close_rect(rects[index]).contains(x, y);
            }
        }
        self.hover = hover;
    }

    /// The pane scrolls by line when the pointer sits over it; the sidebar
    /// scrolls its tree instead when the pointer sits there.
    fn on_scroll(&mut self, x: i32, y: i32, delta: i32) {
        if let Some(rect) = self.layout.sidebar
            && rect.contains(x, y)
        {
            if delta > 0 {
                self.tree.scroll = self.tree.scroll.saturating_sub(SCROLL_STEP);
            } else if delta < 0 {
                self.tree.scroll += SCROLL_STEP;
                self.clamp_tree_scroll();
            }
            return;
        }

        if delta > 0 {
            self.buffer_mut().scroll_line = self.buffer().scroll_line.saturating_sub(SCROLL_STEP);
        } else if delta < 0 {
            let max = self
                .buffer()
                .lines
                .len()
                .saturating_sub(self.layout.rows_visible.max(1));
            let next = (self.buffer().scroll_line + SCROLL_STEP).min(max);
            self.buffer_mut().scroll_line = next;
        }
    }

    fn on_key(&mut self, code: u32) {
        // Alt marks a chord as the window manager's. The manager claims the
        // ones it acts on, and those never arrive here, but an Alt chord it
        // does not claim still does — and it is not one this program bound
        // either: Ctrl+Alt+W is a registry dump, not a request to close a tab.
        if self.mods.alt {
            return;
        }
        if code == keycode::ESCAPE {
            self.dismiss_prompt();
            return;
        }
        if self.prompt.is_some() {
            // Every other key belongs to the field. `handle` feeds it the
            // raw event once this returns, which is what keeps this arm from
            // starving it: returning early here only skips the document
            // shortcuts below, not the field.
            return;
        }

        // A movement key extends the selection when Shift is held — setting
        // the anchor to wherever the cursor already sits, the first time,
        // rather than on every key so a run of Shift+Right keeps the start
        // fixed — and otherwise ends it, the same way a plain click does.
        if is_movement_key(code) {
            if self.mods.shift {
                let cursor = self.buffer().cursor;
                self.buffer_mut().anchor.get_or_insert(cursor);
            } else {
                self.buffer_mut().clear_selection();
            }
        }

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
                let last = self.buffer().lines.len() - 1;
                let col = self.buffer().line_chars(last);
                self.set_cursor(Position { line: last, col });
            }
            keycode::HOME => {
                let line = self.buffer().cursor.line;
                self.set_cursor(Position { line, col: 0 });
            }
            keycode::END => {
                let line = self.buffer().cursor.line;
                let col = self.buffer().line_chars(line);
                self.set_cursor(Position { line, col });
            }
            keycode::F5 => self.refresh_tree(),
            keycode::A if self.mods.ctrl => self.select_all(),
            keycode::C if self.mods.ctrl => self.copy(),
            keycode::X if self.mods.ctrl => self.cut(),
            keycode::V if self.mods.ctrl => self.paste(),
            keycode::S if self.mods.ctrl => self.save(),
            keycode::Z if self.mods.ctrl => self.undo(),
            keycode::Y if self.mods.ctrl => self.redo(),
            keycode::N if self.mods.ctrl => self.new_buffer(),
            keycode::B if self.mods.ctrl => self.toggle_sidebar(),
            keycode::W if self.mods.ctrl => self.close_active(),
            keycode::TAB if self.mods.ctrl => self.next_tab(),
            keycode::F if self.mods.ctrl => self.open_prompt(PromptKind::Find),
            keycode::G if self.mods.ctrl => self.open_prompt(PromptKind::GoTo),
            keycode::O if self.mods.ctrl => self.open_prompt(PromptKind::Open),
            keycode::RETURN | keycode::NUMPAD_ENTER if !self.mods.ctrl => self.insert_str("\n"),
            keycode::BACKSPACE if !self.mods.ctrl => self.backspace(),
            keycode::DELETE if !self.mods.ctrl => self.delete_forward(),
            keycode::TAB if !self.mods.ctrl => {
                let indent = self.buffer().lang.indent.text();
                self.insert_str(&indent);
            }
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
        let active = self.active;
        let (width, height) = (self.window.width, self.window.height);
        self.layout = view::Layout::new(
            width,
            height,
            self.sidebar_open,
            self.prompt.is_some(),
            self.buffers[active].lines.len(),
        );

        let labels = self.tab_labels();
        let dirty: Vec<bool> = self.buffers.iter().map(|buffer| buffer.dirty).collect();
        let root_name = root_display_name(&self.tree.root);
        let active_path = self.buffers[active].path.clone();
        let name = labels[active].clone();
        let language = self.buffers[active].lang.name;
        let indent_label = self.buffers[active].lang.indent.label();
        let encoding = if self.buffers[active].repaired {
            "UTF-8 (repaired)"
        } else {
            "UTF-8"
        };
        let volume = volume_label(active_path.as_deref());
        let hover_tree = self.hover.tree;
        let hover_tab = self.hover.tab;
        let hover_tab_close = self.hover.tab_close;
        let note = self.status.clone();
        let prompt_bar_label = self.prompt.as_ref().map(|prompt| prompt_label(prompt.kind));
        let prompt_report = self.prompt_report();

        let Some(buf) = self.window.buffer_mut() else {
            return;
        };
        let mut canvas = Canvas::new(buf, width, height);
        canvas.fill(
            edos_render::widgets::Rect::new(0, 0, width, height),
            edos_render::theme::Theme::DEFAULT.background.raw(),
        );

        view::draw_tabs(
            &mut canvas,
            self.layout.tabs,
            &labels,
            &dirty,
            active,
            hover_tab,
            hover_tab_close,
        );
        if let Some(rect) = self.layout.sidebar {
            view::draw_sidebar(
                &mut canvas,
                rect,
                &root_name,
                &self.tree,
                hover_tree,
                active_path.as_deref(),
            );
        }
        view::draw_pane(
            &mut canvas,
            &self.layout,
            &mut self.buffers[active],
            &self.find_matches,
            self.find_current,
        );
        let note_ref = note
            .as_ref()
            .map(|(message, warning)| (message.as_str(), *warning));
        view::draw_status(
            &mut canvas,
            &self.layout,
            &name,
            language,
            self.buffers[active].cursor.line + 1,
            self.buffers[active].cursor.col + 1,
            &indent_label,
            encoding,
            &volume,
            note_ref,
        );
        if let Some(rect) = self.layout.prompt {
            let label = prompt_bar_label.unwrap_or("");
            view::draw_prompt(&mut canvas, rect, label, &prompt_report);
        }
        if let Some(prompt) = &self.prompt {
            prompt.widgets.draw_all(canvas.buf, width, height);
        }

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

/// The directory a fresh sidebar tree is rooted at: the opening file's own
/// directory, or the working directory for a buffer with no path yet. Read
/// once, at startup — the tree stays rooted there regardless of which tab is
/// later made active, the way a project explorer does.
fn root_dir(buffer: &Buffer) -> String {
    match &buffer.path {
        Some(path) => match path.rsplit_once('/') {
            Some(("", _)) => "/".to_string(),
            Some((dir, _)) => dir.to_string(),
            None => ".".to_string(),
        },
        None => std::env::current_dir()
            .map(|path| path.display().to_string())
            .unwrap_or_else(|_| "/".to_string()),
    }
}

/// What the sidebar calls a root directory: its own last path component.
fn root_display_name(dir: &str) -> String {
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
