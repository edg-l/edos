//! Widget container that manages a collection of widgets.

use super::{Widget, WidgetEvent, WidgetId};
use crate::surface::Surface;
use crate::window::{WindowEvent, WindowEventType};
use edos_lib::{
    clipboard::{self, Buffer},
    keymap::{Modifiers, keycode, map_keycode, update_modifiers},
};

/// Container that manages widgets and routes events.
pub struct WidgetContainer {
    /// Identity belongs to the container, not to the widget: a widget answers
    /// only to the id `add` handed back, and never carries one of its own.
    widgets: Vec<(WidgetId, Box<dyn Widget>)>,
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

    /// Add a widget to the container, returning the id it now answers to.
    pub fn add<W: Widget + 'static>(&mut self, widget: W) -> WidgetId {
        let id = self.next_id;
        self.next_id += 1;
        self.widgets.push((id, Box::new(widget)));

        // Auto-focus first focusable widget
        if self.focused.is_none()
            && let Some((_, w)) = self.widgets.last_mut()
            && w.focusable()
        {
            w.set_focused(true);
            self.focused = Some(id);
        }

        id
    }

    /// Get a reference to a widget by ID.
    pub fn get(&self, id: WidgetId) -> Option<&dyn Widget> {
        self.widgets
            .iter()
            .find(|(wid, _)| *wid == id)
            .map(|(_, w)| &**w)
    }

    /// Get a mutable reference to a widget by ID.
    ///
    /// The `'static` is the box's own bound, spelled out because a `&mut` to a
    /// trait object is invariant and would otherwise shrink to the borrow.
    pub fn get_mut(&mut self, id: WidgetId) -> Option<&mut (dyn Widget + 'static)> {
        self.widgets
            .iter_mut()
            .find(|(wid, _)| *wid == id)
            .map(|(_, w)| &mut **w)
    }

    /// Draw all widgets onto `surface`.
    pub fn draw_all(&self, surface: &mut Surface<'_>) {
        for (_, widget) in &self.widgets {
            widget.draw(surface);
        }
    }

    /// Handle a window event and return any widget events that were triggered.
    pub fn handle_event(&mut self, event: &WindowEvent) -> Vec<(WidgetId, WidgetEvent)> {
        let mut results = Vec::new();

        match event.event_type() {
            Some(WindowEventType::MouseMove) => {
                for (_, widget) in &mut self.widgets {
                    widget.on_mouse_move(event.x, event.y);
                }
            }
            Some(WindowEventType::MouseButton) => {
                let pressed = event.data != 0;
                let x = event.x;
                let y = event.y;

                // Check if click was on a focusable widget
                if pressed {
                    let new_focus = self
                        .widgets
                        .iter()
                        .find(|(_, w)| w.focusable() && w.bounds().contains(x, y))
                        .map(|(id, _)| *id);

                    if new_focus != self.focused {
                        self.set_focused_id(new_focus);
                    }
                }

                // Dispatch to all widgets
                for (id, widget) in &mut self.widgets {
                    if let Some(evt) = widget.on_mouse_button(x, y, pressed) {
                        results.push((*id, evt));
                    }
                }
            }
            Some(WindowEventType::Character) => {
                if let Some(ch) = event.character() {
                    // Send to focused widget
                    if let Some(focused_id) = self.focused
                        && let Some(w) = self.get_mut(focused_id)
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
                } else if let Some(focused_id) = self.focused {
                    let mods = self.mods;
                    if let Some(w) = self.get_mut(focused_id) {
                        if let Some(evt) = w.on_key(scancode, true) {
                            results.push((focused_id, evt));
                        }
                        // The kernel reports scancodes, never characters: it
                        // has no keyboard layout and should not grow one. Text
                        // reaches a widget only if the toolkit maps the key
                        // here, the same way the terminal does.
                        if let Some(ch) = map_keycode(scancode, &mods)
                            && !ch.is_control()
                            && let Some(evt) = w.on_char(ch)
                        {
                            results.push((focused_id, evt));
                        }
                    }
                }
            }
            Some(WindowEventType::KeyRelease) => {
                let scancode = event.code;
                if update_modifiers(&mut self.mods, scancode, false) {
                    return results;
                }
                if let Some(focused_id) = self.focused
                    && let Some(w) = self.get_mut(focused_id)
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
        let widget = self.get_mut(focused_id)?;

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

    /// The ids of every widget that can take focus, in insertion order.
    fn focusable_ids(&self) -> Vec<WidgetId> {
        self.widgets
            .iter()
            .filter(|(_, w)| w.focusable())
            .map(|(id, _)| *id)
            .collect()
    }

    /// Move focus to `id`, unfocusing whoever holds it now.
    fn set_focused_id(&mut self, id: Option<WidgetId>) {
        if let Some(old_id) = self.focused
            && let Some(w) = self.get_mut(old_id)
        {
            w.set_focused(false);
        }
        if let Some(new_id) = id
            && let Some(w) = self.get_mut(new_id)
        {
            w.set_focused(true);
        }
        self.focused = id;
    }

    /// Move focus to the next focusable widget.
    pub fn focus_next(&mut self) {
        self.focus_step(1);
    }

    /// Move focus to the previous focusable widget.
    pub fn focus_prev(&mut self) {
        self.focus_step(-1);
    }

    /// Move focus `step` places along the focusable widgets, wrapping.
    fn focus_step(&mut self, step: isize) {
        let ids = self.focusable_ids();
        if ids.is_empty() {
            return;
        }

        let pos = self
            .focused
            .and_then(|current| ids.iter().position(|&id| id == current))
            .map(|pos| (pos as isize + step).rem_euclid(ids.len() as isize) as usize)
            .unwrap_or(if step >= 0 { 0 } else { ids.len() - 1 });

        self.set_focused_id(Some(ids[pos]));
    }

    pub fn focused(&self) -> Option<WidgetId> {
        self.focused
    }

    /// Set focus to a specific widget, if it can take it.
    pub fn set_focus(&mut self, id: WidgetId) {
        if self.get(id).is_some_and(|w| w.focusable()) {
            self.set_focused_id(Some(id));
        }
    }

    /// Clear focus from all widgets.
    pub fn clear_focus(&mut self) {
        self.set_focused_id(None);
    }
}

impl Default for WidgetContainer {
    fn default() -> Self {
        Self::new()
    }
}
