//! Widget container that manages a collection of widgets.

use super::{Widget, WidgetEvent, WidgetId};
use crate::window::{WindowEvent, WindowEventType};
use edos_lib::{
    clipboard::{self, Buffer},
    keymap::{Modifiers, keycode, map_keycode, update_modifiers},
};

/// Container that manages widgets and routes events.
pub struct WidgetContainer {
    widgets: Vec<Box<dyn Widget>>,
    focused: Option<WidgetId>,
    next_id: WidgetId,
    /// Modifier state, tracked here because the kernel reports scancodes and
    /// knows nothing about keyboard layouts.
    mods: Modifiers,
}

impl WidgetContainer {
    /// Create a new empty container.
    pub fn new() -> Self {
        Self {
            widgets: Vec::new(),
            focused: None,
            next_id: 1,
            mods: Modifiers::default(),
        }
    }

    /// Add a widget to the container.
    /// The widget's ID field is ignored; a new ID is assigned and returned.
    pub fn add<W: Widget + 'static>(&mut self, widget: W) -> WidgetId {
        let id = self.next_id;
        self.next_id += 1;

        // We need to create a new widget with the assigned ID
        // Since we can't modify the ID directly, we'll use a wrapper approach
        // Actually, let's just accept that widgets are created with id=0 and we track them by index

        self.widgets.push(Box::new(WidgetWrapper {
            inner: Box::new(widget),
            id,
        }));

        // Auto-focus first focusable widget
        if self.focused.is_none()
            && let Some(w) = self.widgets.last()
            && w.focusable()
        {
            self.focused = Some(id);
            if let Some(w) = self.widgets.last_mut() {
                w.set_focused(true);
            }
        }

        id
    }

    /// Get a reference to a widget by ID.
    pub fn get(&self, id: WidgetId) -> Option<&dyn Widget> {
        self.widgets.iter().find(|w| w.id() == id).map(|w| &**w)
    }

    /// Get a mutable reference to a widget by ID.
    pub fn get_mut(&mut self, id: WidgetId) -> Option<&mut Box<dyn Widget>> {
        self.widgets.iter_mut().find(|w| w.id() == id)
    }

    /// Draw all widgets to the buffer.
    pub fn draw_all(&self, buffer: &mut [u32], width: u32, height: u32) {
        for widget in &self.widgets {
            widget.draw(buffer, width, height);
        }
    }

    /// Handle a window event and return any widget events that were triggered.
    pub fn handle_event(&mut self, event: &WindowEvent) -> Vec<(WidgetId, WidgetEvent)> {
        let mut results = Vec::new();

        match event.event_type() {
            Some(WindowEventType::MouseMove) => {
                for widget in &mut self.widgets {
                    widget.on_mouse_move(event.x, event.y);
                }
            }
            Some(WindowEventType::MouseButton) => {
                let pressed = event.data != 0;
                let x = event.x;
                let y = event.y;

                // Check if click was on a focusable widget
                if pressed {
                    let mut new_focus = None;
                    for widget in &self.widgets {
                        if widget.focusable() && widget.bounds().contains(x, y) {
                            new_focus = Some(widget.id());
                            break;
                        }
                    }

                    // Update focus if changed
                    if new_focus != self.focused {
                        // Unfocus old widget
                        if let Some(old_id) = self.focused
                            && let Some(w) = self.widgets.iter_mut().find(|w| w.id() == old_id)
                        {
                            w.set_focused(false);
                        }
                        // Focus new widget
                        if let Some(new_id) = new_focus
                            && let Some(w) = self.widgets.iter_mut().find(|w| w.id() == new_id)
                        {
                            w.set_focused(true);
                        }
                        self.focused = new_focus;
                    }
                }

                // Dispatch to all widgets
                for widget in &mut self.widgets {
                    if let Some(evt) = widget.on_mouse_button(x, y, pressed) {
                        results.push((widget.id(), evt));
                    }
                }
            }
            Some(WindowEventType::Character) => {
                if let Some(ch) = event.character() {
                    // Send to focused widget
                    if let Some(focused_id) = self.focused
                        && let Some(w) = self.widgets.iter_mut().find(|w| w.id() == focused_id)
                        && let Some(evt) = w.on_char(ch)
                    {
                        results.push((focused_id, evt));
                    }
                }
            }
            Some(WindowEventType::KeyPress) => {
                let scancode = event.code;
                if update_modifiers(&mut self.mods, scancode, true) {
                    return results;
                }

                // Cut, copy and paste, before the key is decoded: with Ctrl
                // held these are control characters, and a control character
                // is exactly what `on_char` is not given.
                if self.mods.ctrl
                    && !self.mods.alt
                    && let Some(evt) = self.clipboard_shortcut(scancode)
                {
                    results.extend(evt);
                    return results;
                }

                if scancode == keycode::TAB {
                    self.focus_next();
                } else if let Some(focused_id) = self.focused
                    && let Some(w) = self.widgets.iter_mut().find(|w| w.id() == focused_id)
                {
                    if let Some(evt) = w.on_key(scancode, true) {
                        results.push((focused_id, evt));
                    }
                    // The kernel reports scancodes, never characters: it has
                    // no keyboard layout and should not grow one. Text
                    // reaches a widget only if the toolkit maps the key
                    // here, the same way the terminal does.
                    if let Some(ch) = map_keycode(scancode, &self.mods)
                        && !ch.is_control()
                        && let Some(evt) = w.on_char(ch)
                    {
                        results.push((focused_id, evt));
                    }
                }
            }
            Some(WindowEventType::KeyRelease) => {
                let scancode = event.code;
                if update_modifiers(&mut self.mods, scancode, false) {
                    return results;
                }
                if let Some(focused_id) = self.focused
                    && let Some(w) = self.widgets.iter_mut().find(|w| w.id() == focused_id)
                    && let Some(evt) = w.on_key(scancode, false)
                {
                    results.push((focused_id, evt));
                }
            }
            _ => {}
        }

        results
    }

    /// Run the cut, copy or paste bound to `scancode`, if it is one of them.
    ///
    /// Returns None when the key is not a clipboard shortcut, so the caller can
    /// tell "not mine" from "mine, and it changed nothing".
    fn clipboard_shortcut(&mut self, scancode: u32) -> Option<Vec<(WidgetId, WidgetEvent)>> {
        let focused_id = self.focused?;
        let widget = self
            .widgets
            .iter_mut()
            .find(|widget| widget.id() == focused_id)?;

        let event = match scancode {
            keycode::C => {
                if let Some(text) = widget.clipboard_copy() {
                    let _ = clipboard::set(Buffer::Clipboard, &text);
                }
                None
            }
            keycode::X => {
                if let Some(text) = widget.clipboard_copy() {
                    let _ = clipboard::set(Buffer::Clipboard, &text);
                }
                widget.clipboard_cut()
            }
            keycode::V => {
                let text = clipboard::get(Buffer::Clipboard);
                if text.is_empty() {
                    None
                } else {
                    widget.clipboard_paste(&text)
                }
            }
            _ => return None,
        };

        Some(event.into_iter().map(|evt| (focused_id, evt)).collect())
    }

    /// Move focus to the next focusable widget.
    pub fn focus_next(&mut self) {
        let focusable_ids: Vec<WidgetId> = self
            .widgets
            .iter()
            .filter(|w| w.focusable())
            .map(|w| w.id())
            .collect();

        if focusable_ids.is_empty() {
            return;
        }

        // Unfocus current
        if let Some(old_id) = self.focused
            && let Some(w) = self.widgets.iter_mut().find(|w| w.id() == old_id)
        {
            w.set_focused(false);
        }

        // Find next
        let next_id = if let Some(current_id) = self.focused {
            if let Some(pos) = focusable_ids.iter().position(|&id| id == current_id) {
                focusable_ids[(pos + 1) % focusable_ids.len()]
            } else {
                focusable_ids[0]
            }
        } else {
            focusable_ids[0]
        };

        // Focus next
        if let Some(w) = self.widgets.iter_mut().find(|w| w.id() == next_id) {
            w.set_focused(true);
        }
        self.focused = Some(next_id);
    }

    /// Move focus to the previous focusable widget.
    pub fn focus_prev(&mut self) {
        let focusable_ids: Vec<WidgetId> = self
            .widgets
            .iter()
            .filter(|w| w.focusable())
            .map(|w| w.id())
            .collect();

        if focusable_ids.is_empty() {
            return;
        }

        // Unfocus current
        if let Some(old_id) = self.focused
            && let Some(w) = self.widgets.iter_mut().find(|w| w.id() == old_id)
        {
            w.set_focused(false);
        }

        // Find previous
        let prev_id = if let Some(current_id) = self.focused {
            if let Some(pos) = focusable_ids.iter().position(|&id| id == current_id) {
                let prev_pos = if pos == 0 {
                    focusable_ids.len() - 1
                } else {
                    pos - 1
                };
                focusable_ids[prev_pos]
            } else {
                focusable_ids[focusable_ids.len() - 1]
            }
        } else {
            focusable_ids[focusable_ids.len() - 1]
        };

        // Focus previous
        if let Some(w) = self.widgets.iter_mut().find(|w| w.id() == prev_id) {
            w.set_focused(true);
        }
        self.focused = Some(prev_id);
    }

    /// Get the currently focused widget ID.
    pub fn focused(&self) -> Option<WidgetId> {
        self.focused
    }

    /// Set focus to a specific widget.
    pub fn set_focus(&mut self, id: WidgetId) {
        // Unfocus current
        if let Some(old_id) = self.focused
            && let Some(w) = self.widgets.iter_mut().find(|w| w.id() == old_id)
        {
            w.set_focused(false);
        }

        // Focus new
        if let Some(w) = self.widgets.iter_mut().find(|w| w.id() == id)
            && w.focusable()
        {
            w.set_focused(true);
            self.focused = Some(id);
        }
    }

    /// Clear focus from all widgets.
    pub fn clear_focus(&mut self) {
        if let Some(old_id) = self.focused
            && let Some(w) = self.widgets.iter_mut().find(|w| w.id() == old_id)
        {
            w.set_focused(false);
        }
        self.focused = None;
    }
}

impl Default for WidgetContainer {
    fn default() -> Self {
        Self::new()
    }
}

/// Internal wrapper to assign IDs to widgets.
struct WidgetWrapper {
    inner: Box<dyn Widget>,
    id: WidgetId,
}

impl Widget for WidgetWrapper {
    fn id(&self) -> WidgetId {
        self.id
    }

    fn bounds(&self) -> super::Rect {
        self.inner.bounds()
    }

    fn set_position(&mut self, x: i32, y: i32) {
        self.inner.set_position(x, y);
    }

    fn draw(&self, buffer: &mut [u32], buffer_width: u32, buffer_height: u32) {
        self.inner.draw(buffer, buffer_width, buffer_height);
    }

    fn on_mouse_move(&mut self, x: i32, y: i32) {
        self.inner.on_mouse_move(x, y);
    }

    fn on_mouse_button(&mut self, x: i32, y: i32, pressed: bool) -> Option<WidgetEvent> {
        self.inner.on_mouse_button(x, y, pressed)
    }

    fn on_char(&mut self, ch: char) -> Option<WidgetEvent> {
        self.inner.on_char(ch)
    }

    fn on_key(&mut self, scancode: u32, pressed: bool) -> Option<WidgetEvent> {
        self.inner.on_key(scancode, pressed)
    }

    fn focusable(&self) -> bool {
        self.inner.focusable()
    }

    fn set_focused(&mut self, focused: bool) {
        self.inner.set_focused(focused);
    }

    fn enabled(&self) -> bool {
        self.inner.enabled()
    }

    fn set_enabled(&mut self, enabled: bool) {
        self.inner.set_enabled(enabled);
    }

    // Forwarded like the rest: a defaulted trait method left off this wrapper
    // takes the default silently, so the wrapped widget loses the capability
    // with nothing to show for it at compile time.
    fn clipboard_copy(&self) -> Option<String> {
        self.inner.clipboard_copy()
    }

    fn clipboard_cut(&mut self) -> Option<WidgetEvent> {
        self.inner.clipboard_cut()
    }

    fn clipboard_paste(&mut self, text: &str) -> Option<WidgetEvent> {
        self.inner.clipboard_paste(text)
    }
}
