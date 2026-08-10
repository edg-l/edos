//! Widget toolkit for building GUI applications.

pub mod button;
pub mod checkbox;
pub mod container;
pub mod label;
pub mod layout;
pub mod slider;
pub mod terminal;
pub mod text_input;

pub use button::Button;
pub use checkbox::Checkbox;
pub use container::WidgetContainer;
pub use label::Label;
pub use slider::Slider;
pub use terminal::Terminal;
pub use text_input::TextInput;

/// Unique identifier for a widget.
pub type WidgetId = u32;

/// Rectangle defining widget bounds.
#[derive(Clone, Copy, Debug)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
}

impl Rect {
    /// Create a new rectangle.
    pub fn new(x: i32, y: i32, width: u32, height: u32) -> Self {
        Self {
            x,
            y,
            width,
            height,
        }
    }

    /// Check if a point is inside the rectangle.
    pub fn contains(&self, px: i32, py: i32) -> bool {
        px >= self.x
            && px < self.x + self.width as i32
            && py >= self.y
            && py < self.y + self.height as i32
    }
}

/// Events emitted by widgets.
#[derive(Clone, Debug)]
pub enum WidgetEvent {
    /// Button was clicked.
    Clicked,
    /// Slider or other value changed.
    ValueChanged(i32),
    /// Text content changed.
    TextChanged(String),
    /// Text input submitted (Enter pressed).
    Submit(String),
}

/// Trait that all widgets must implement.
pub trait Widget {
    /// Get the widget's unique identifier.
    fn id(&self) -> WidgetId;

    /// Get the widget's bounding rectangle.
    fn bounds(&self) -> Rect;

    /// Set the widget's position.
    fn set_position(&mut self, x: i32, y: i32);

    /// Draw the widget to the given buffer.
    fn draw(&self, buffer: &mut [u32], buffer_width: u32, buffer_height: u32);

    /// Handle mouse movement.
    fn on_mouse_move(&mut self, x: i32, y: i32);

    /// Handle mouse button press/release. Returns event if one was triggered.
    fn on_mouse_button(&mut self, x: i32, y: i32, pressed: bool) -> Option<WidgetEvent>;

    /// Handle character input. Returns event if one was triggered.
    fn on_char(&mut self, ch: char) -> Option<WidgetEvent>;

    /// Handle key press/release. Returns event if one was triggered.
    fn on_key(&mut self, scancode: u32, pressed: bool) -> Option<WidgetEvent>;

    /// Check if this widget can receive focus.
    fn focusable(&self) -> bool;

    /// Set the focused state of this widget.
    fn set_focused(&mut self, focused: bool);
}

/// Color constants for widget rendering. Values are read from `Theme::DEFAULT`.
pub mod colors {
    use crate::theme::Theme;
    /// Light gray background.
    pub const BACKGROUND: u32 = Theme::DEFAULT.background.raw();
    /// Button normal state.
    pub const BUTTON_NORMAL: u32 = Theme::DEFAULT.button_normal.raw();
    /// Button hover state.
    pub const BUTTON_HOVER: u32 = Theme::DEFAULT.button_hover.raw();
    /// Button pressed state.
    pub const BUTTON_PRESSED: u32 = Theme::DEFAULT.button_pressed.raw();
    /// Focus ring color.
    pub const FOCUS_RING: u32 = Theme::DEFAULT.focus_ring.raw();
    /// Primary text color.
    pub const TEXT: u32 = Theme::DEFAULT.text_primary.raw();
    /// Placeholder text color.
    pub const TEXT_PLACEHOLDER: u32 = Theme::DEFAULT.text_placeholder.raw();
    /// Text input background.
    pub const INPUT_BG: u32 = Theme::DEFAULT.input_bg.raw();
    /// Text input border.
    pub const INPUT_BORDER: u32 = Theme::DEFAULT.input_border.raw();
    /// Border of a hovered or pressed control.
    pub const BORDER_HOVER: u32 = Theme::DEFAULT.border_hover.raw();
    /// Slider track color.
    pub const SLIDER_TRACK: u32 = Theme::DEFAULT.slider_track.raw();
    /// Slider thumb color.
    pub const SLIDER_THUMB: u32 = Theme::DEFAULT.slider_thumb.raw();
    /// Checkbox check mark color.
    pub const CHECKBOX_CHECK: u32 = Theme::DEFAULT.checkbox_check.raw();
}

/// Helper function to draw a filled rectangle in a buffer.
pub fn draw_rect(
    buffer: &mut [u32],
    buffer_width: u32,
    buffer_height: u32,
    x: i32,
    y: i32,
    width: u32,
    height: u32,
    color: u32,
) {
    let start_x = x.max(0) as u32;
    let start_y = y.max(0) as u32;
    let end_x = ((x + width as i32) as u32).min(buffer_width);
    let end_y = ((y + height as i32) as u32).min(buffer_height);

    for py in start_y..end_y {
        for px in start_x..end_x {
            let idx = (py * buffer_width + px) as usize;
            if idx < buffer.len() {
                buffer[idx] = color;
            }
        }
    }
}

/// Helper function to draw a rectangle outline.
pub fn draw_rect_outline(
    buffer: &mut [u32],
    buffer_width: u32,
    buffer_height: u32,
    x: i32,
    y: i32,
    width: u32,
    height: u32,
    color: u32,
) {
    // Top edge
    draw_rect(buffer, buffer_width, buffer_height, x, y, width, 1, color);
    // Bottom edge
    draw_rect(
        buffer,
        buffer_width,
        buffer_height,
        x,
        y + height as i32 - 1,
        width,
        1,
        color,
    );
    // Left edge
    draw_rect(buffer, buffer_width, buffer_height, x, y, 1, height, color);
    // Right edge
    draw_rect(
        buffer,
        buffer_width,
        buffer_height,
        x + width as i32 - 1,
        y,
        1,
        height,
        color,
    );
}

/// Draw the keyboard focus ring around a control, offset from its edge by
/// [`crate::metrics::FOCUS_RING_GAP`] so the ring never touches the border.
pub fn draw_focus_ring(
    buffer: &mut [u32],
    buffer_width: u32,
    buffer_height: u32,
    x: i32,
    y: i32,
    width: u32,
    height: u32,
) {
    let gap = crate::metrics::FOCUS_RING_GAP;
    draw_rect_outline(
        buffer,
        buffer_width,
        buffer_height,
        x - gap as i32,
        y - gap as i32,
        width + gap * 2,
        height + gap * 2,
        colors::FOCUS_RING,
    );
}

/// Helper function to draw text into a buffer.
pub fn draw_text(
    buffer: &mut [u32],
    buffer_width: u32,
    buffer_height: u32,
    x: i32,
    y: i32,
    text: &str,
    color: u32,
) {
    use crate::graphics::Color;
    use noto_sans_mono_bitmap::{FontWeight, RasterHeight, get_raster, get_raster_width};

    let font_size = RasterHeight::Size16;
    let char_width = get_raster_width(FontWeight::Regular, font_size);

    let mut current_x = x;
    for ch in text.chars() {
        if current_x < 0 || current_x >= buffer_width as i32 {
            current_x += char_width as i32;
            continue;
        }

        if let Some(raster) = get_raster(ch, FontWeight::Regular, font_size) {
            let raster_data = raster.raster();
            let char_h = raster.height();
            let char_w = raster.width();

            for cy in 0..char_h {
                let py = y + cy as i32;
                if py < 0 || py >= buffer_height as i32 {
                    continue;
                }

                for cx in 0..char_w {
                    let px = current_x + cx as i32;
                    if px < 0 || px >= buffer_width as i32 {
                        continue;
                    }

                    let intensity = raster_data[cy][cx];
                    if intensity > 0 {
                        let idx = (py as u32 * buffer_width + px as u32) as usize;
                        if idx < buffer.len() {
                            if intensity == 255 {
                                buffer[idx] = color;
                            } else {
                                // Blend with background
                                let bg = Color::from(buffer[idx]);
                                let fg = Color::from(color);
                                let alpha = intensity as u32;
                                let inv_alpha = 255 - alpha;

                                let r =
                                    (fg.red() as u32 * alpha + bg.red() as u32 * inv_alpha) / 255;
                                let g = (fg.green() as u32 * alpha + bg.green() as u32 * inv_alpha)
                                    / 255;
                                let b =
                                    (fg.blue() as u32 * alpha + bg.blue() as u32 * inv_alpha) / 255;

                                buffer[idx] = Color::from_rgb(r as u8, g as u8, b as u8).raw();
                            }
                        }
                    }
                }
            }
        }

        current_x += char_width as i32;
    }
}

/// Get the width of a character in the default font.
pub fn char_width() -> u32 {
    use noto_sans_mono_bitmap::{FontWeight, RasterHeight, get_raster_width};
    get_raster_width(FontWeight::Regular, RasterHeight::Size16) as u32
}

/// Get the height of text in the default font.
pub fn text_height() -> u32 {
    crate::metrics::TEXT_CELL_HEIGHT
}
