//! Horizontal slider widget.

use super::{FocusState, Rect, Widget, WidgetEvent, colors};
use crate::metrics::{
    CONTROL_HEIGHT, SLIDER_THUMB_HEIGHT, SLIDER_THUMB_WIDTH, SLIDER_TRACK_HEIGHT,
};
use crate::surface::Surface;
use crate::theme::Theme;

/// A horizontal value slider.
pub struct Slider {
    x: i32,
    y: i32,
    width: u32,
    min: i32,
    max: i32,
    value: i32,
    dragging: bool,
    hovered: bool,
    focus: FocusState,
}

impl Slider {
    /// Create a new slider.
    pub fn new(x: i32, y: i32, width: u32, min: i32, max: i32) -> Self {
        Self {
            x,
            y,
            width,
            min,
            max,
            value: min,
            dragging: false,
            hovered: false,
            focus: FocusState::default(),
        }
    }

    /// Create a slider with an initial value.
    pub fn with_value(x: i32, y: i32, width: u32, min: i32, max: i32, value: i32) -> Self {
        let clamped = value.clamp(min, max);
        Self {
            x,
            y,
            width,
            min,
            max,
            value: clamped,
            dragging: false,
            hovered: false,
            focus: FocusState::default(),
        }
    }

    pub fn value(&self) -> i32 {
        self.value
    }

    pub fn set_value(&mut self, value: i32) {
        self.value = value.clamp(self.min, self.max);
    }

    pub fn min(&self) -> i32 {
        self.min
    }

    pub fn max(&self) -> i32 {
        self.max
    }

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
        let track_width = (self.width - SLIDER_THUMB_WIDTH) as f32;
        let ratio = (self.value - self.min) as f32 / range;
        self.x + (ratio * track_width) as i32
    }

    /// Calculate value from X position.
    fn value_from_x(&self, mouse_x: i32) -> i32 {
        let track_width = (self.width - SLIDER_THUMB_WIDTH) as f32;
        let relative_x = (mouse_x - self.x) as f32;
        let ratio = (relative_x / track_width).clamp(0.0, 1.0);
        let range = (self.max - self.min) as f32;
        self.min + (ratio * range) as i32
    }

    /// Top of the thumb, centred in the control row.
    fn thumb_y(&self) -> i32 {
        self.y + (CONTROL_HEIGHT as i32 - SLIDER_THUMB_HEIGHT as i32) / 2
    }

    fn thumb_bounds(&self) -> Rect {
        Rect::new(
            self.thumb_x(),
            self.thumb_y(),
            SLIDER_THUMB_WIDTH,
            SLIDER_THUMB_HEIGHT,
        )
    }
}

impl Widget for Slider {
    fn bounds(&self) -> Rect {
        Rect::new(self.x, self.y, self.width, CONTROL_HEIGHT)
    }

    fn set_position(&mut self, x: i32, y: i32) {
        self.x = x;
        self.y = y;
    }

    fn draw(&self, surface: &mut Surface<'_>) {
        // Draw track
        let track_y = self.y + (CONTROL_HEIGHT as i32 - SLIDER_TRACK_HEIGHT as i32) / 2;
        surface.rect(
            self.x,
            track_y,
            self.width,
            SLIDER_TRACK_HEIGHT,
            colors::SLIDER_TRACK,
        );

        // Draw filled portion of track
        let thumb_center = self.thumb_x() + SLIDER_THUMB_WIDTH as i32 / 2;
        let filled_width = (thumb_center - self.x) as u32;
        surface.rect(
            self.x,
            track_y,
            filled_width,
            SLIDER_TRACK_HEIGHT,
            Theme::DEFAULT.slider_thumb.raw(),
        );

        // Draw focus ring if focused
        if self.focus.focused {
            surface.focus_ring(self.x, self.y, self.width, CONTROL_HEIGHT);
        }

        // Draw thumb
        let thumb_color = if !self.focus.enabled {
            colors::TEXT_DISABLED
        } else if self.dragging || self.hovered {
            Theme::DEFAULT.slider_thumb_hover.raw()
        } else {
            Theme::DEFAULT.slider_thumb.raw()
        };
        surface.rect(
            self.thumb_x(),
            self.thumb_y(),
            SLIDER_THUMB_WIDTH,
            SLIDER_THUMB_HEIGHT,
            thumb_color,
        );
        surface.rect_outline(
            self.thumb_x(),
            self.thumb_y(),
            SLIDER_THUMB_WIDTH,
            SLIDER_THUMB_HEIGHT,
            colors::INPUT_BORDER,
        );
    }

    fn on_mouse_move(&mut self, x: i32, y: i32) {
        self.hovered = self.thumb_bounds().contains(x, y) || self.bounds().contains(x, y);

        if self.dragging {
            let new_value = self.value_from_x(x);
            self.value = new_value.clamp(self.min, self.max);
        }
    }

    fn on_mouse_button(&mut self, x: i32, y: i32, pressed: bool) -> Option<WidgetEvent> {
        if !self.focus.enabled {
            return None;
        }
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
        if !self.focus.enabled {
            return None;
        }
        None
    }

    fn on_key(&mut self, scancode: u32, pressed: bool) -> Option<WidgetEvent> {
        if !self.focus.enabled {
            return None;
        }
        if !self.focus.focused || !pressed {
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
            79
                // End
                if self.value != self.max => {
                    self.value = self.max;
                    return Some(WidgetEvent::ValueChanged(self.value));
                }
            _ => {}
        }

        None
    }

    fn focus_state(&self) -> Option<&FocusState> {
        Some(&self.focus)
    }

    fn focus_state_mut(&mut self) -> Option<&mut FocusState> {
        Some(&mut self.focus)
    }
}
