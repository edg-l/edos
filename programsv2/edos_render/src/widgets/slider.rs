//! Horizontal slider widget.

use super::{Rect, Widget, WidgetEvent, WidgetId, colors, draw_rect, draw_rect_outline};
use crate::theme::Theme;

/// A horizontal value slider.
pub struct Slider {
    id: WidgetId,
    x: i32,
    y: i32,
    width: u32,
    min: i32,
    max: i32,
    value: i32,
    dragging: bool,
    hovered: bool,
    focused: bool,
}

const SLIDER_HEIGHT: u32 = 20;
const TRACK_HEIGHT: u32 = 6;
const THUMB_WIDTH: u32 = 12;
const THUMB_HEIGHT: u32 = 20;

impl Slider {
    /// Create a new slider.
    pub fn new(id: WidgetId, x: i32, y: i32, width: u32, min: i32, max: i32) -> Self {
        Self {
            id,
            x,
            y,
            width,
            min,
            max,
            value: min,
            dragging: false,
            hovered: false,
            focused: false,
        }
    }

    /// Create a slider with an initial value.
    pub fn with_value(
        id: WidgetId,
        x: i32,
        y: i32,
        width: u32,
        min: i32,
        max: i32,
        value: i32,
    ) -> Self {
        let clamped = value.clamp(min, max);
        Self {
            id,
            x,
            y,
            width,
            min,
            max,
            value: clamped,
            dragging: false,
            hovered: false,
            focused: false,
        }
    }

    /// Get the current value.
    pub fn value(&self) -> i32 {
        self.value
    }

    /// Set the value.
    pub fn set_value(&mut self, value: i32) {
        self.value = value.clamp(self.min, self.max);
    }

    /// Get the minimum value.
    pub fn min(&self) -> i32 {
        self.min
    }

    /// Get the maximum value.
    pub fn max(&self) -> i32 {
        self.max
    }

    /// Set the range.
    pub fn set_range(&mut self, min: i32, max: i32) {
        self.min = min;
        self.max = max;
        self.value = self.value.clamp(min, max);
    }

    /// Calculate thumb X position based on current value.
    fn thumb_x(&self) -> i32 {
        if self.max == self.min {
            return self.x;
        }
        let range = (self.max - self.min) as f32;
        let track_width = (self.width - THUMB_WIDTH) as f32;
        let ratio = (self.value - self.min) as f32 / range;
        self.x + (ratio * track_width) as i32
    }

    /// Calculate value from X position.
    fn value_from_x(&self, mouse_x: i32) -> i32 {
        let track_width = (self.width - THUMB_WIDTH) as f32;
        let relative_x = (mouse_x - self.x) as f32;
        let ratio = (relative_x / track_width).clamp(0.0, 1.0);
        let range = (self.max - self.min) as f32;
        self.min + (ratio * range) as i32
    }

    fn thumb_bounds(&self) -> Rect {
        Rect::new(self.thumb_x(), self.y, THUMB_WIDTH, THUMB_HEIGHT)
    }
}

impl Widget for Slider {
    fn id(&self) -> WidgetId {
        self.id
    }

    fn bounds(&self) -> Rect {
        Rect::new(self.x, self.y, self.width, SLIDER_HEIGHT)
    }

    fn set_position(&mut self, x: i32, y: i32) {
        self.x = x;
        self.y = y;
    }

    fn draw(&self, buffer: &mut [u32], buffer_width: u32, buffer_height: u32) {
        // Draw track
        let track_y = self.y + (SLIDER_HEIGHT as i32 - TRACK_HEIGHT as i32) / 2;
        draw_rect(
            buffer,
            buffer_width,
            buffer_height,
            self.x,
            track_y,
            self.width,
            TRACK_HEIGHT,
            colors::SLIDER_TRACK,
        );

        // Draw filled portion of track
        let thumb_center = self.thumb_x() + THUMB_WIDTH as i32 / 2;
        let filled_width = (thumb_center - self.x) as u32;
        draw_rect(
            buffer,
            buffer_width,
            buffer_height,
            self.x,
            track_y,
            filled_width,
            TRACK_HEIGHT,
            Theme::DEFAULT.slider_thumb.raw(),
        );

        // Draw focus ring if focused
        if self.focused {
            draw_rect_outline(
                buffer,
                buffer_width,
                buffer_height,
                self.x - 2,
                self.y - 2,
                self.width + 4,
                SLIDER_HEIGHT + 4,
                colors::FOCUS_RING,
            );
        }

        // Draw thumb
        let thumb_color = if self.dragging || self.hovered {
            Theme::DEFAULT.slider_thumb_hover.raw()
        } else {
            Theme::DEFAULT.slider_thumb.raw()
        };
        draw_rect(
            buffer,
            buffer_width,
            buffer_height,
            self.thumb_x(),
            self.y,
            THUMB_WIDTH,
            THUMB_HEIGHT,
            thumb_color,
        );
        draw_rect_outline(buffer, buffer_width, buffer_height,
            self.thumb_x(), self.y, THUMB_WIDTH, THUMB_HEIGHT,
            0xFF2060A0);
    }

    fn on_mouse_move(&mut self, x: i32, y: i32) {
        self.hovered = self.thumb_bounds().contains(x, y) || self.bounds().contains(x, y);

        if self.dragging {
            let new_value = self.value_from_x(x);
            self.value = new_value.clamp(self.min, self.max);
        }
    }

    fn on_mouse_button(&mut self, x: i32, y: i32, pressed: bool) -> Option<WidgetEvent> {
        if pressed {
            if self.bounds().contains(x, y) {
                self.dragging = true;
                // Immediately update value to click position
                let new_value = self.value_from_x(x);
                if new_value != self.value {
                    self.value = new_value.clamp(self.min, self.max);
                    return Some(WidgetEvent::ValueChanged(self.value));
                }
            }
            None
        } else {
            if self.dragging {
                self.dragging = false;
                return Some(WidgetEvent::ValueChanged(self.value));
            }
            None
        }
    }

    fn on_char(&mut self, _ch: char) -> Option<WidgetEvent> {
        None
    }

    fn on_key(&mut self, scancode: u32, pressed: bool) -> Option<WidgetEvent> {
        if !self.focused || !pressed {
            return None;
        }

        let step = ((self.max - self.min) / 10).max(1);

        match scancode {
            75 => {
                // Left arrow
                let new_value = (self.value - step).max(self.min);
                if new_value != self.value {
                    self.value = new_value;
                    return Some(WidgetEvent::ValueChanged(self.value));
                }
            }
            77 => {
                // Right arrow
                let new_value = (self.value + step).min(self.max);
                if new_value != self.value {
                    self.value = new_value;
                    return Some(WidgetEvent::ValueChanged(self.value));
                }
            }
            71 => {
                // Home
                if self.value != self.min {
                    self.value = self.min;
                    return Some(WidgetEvent::ValueChanged(self.value));
                }
            }
            79 => {
                // End
                if self.value != self.max {
                    self.value = self.max;
                    return Some(WidgetEvent::ValueChanged(self.value));
                }
            }
            _ => {}
        }

        None
    }

    fn focusable(&self) -> bool {
        true
    }

    fn set_focused(&mut self, focused: bool) {
        self.focused = focused;
    }
}
