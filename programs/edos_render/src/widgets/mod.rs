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
///
/// `WidgetContainer` wraps every widget to assign it an id, and that wrapper
/// forwards each method by hand. A method added here with a default body is
/// inherited by the wrapper and never reaches the real widget, so add the
/// forwarding there in the same change.
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

    /// Whether this widget currently accepts input.
    ///
    /// A disabled control stays on screen and stays legible: hiding it instead
    /// makes the layout jump and leaves the user with no idea the action
    /// exists. Widgets that cannot be disabled report `true` and ignore
    /// `set_enabled`.
    fn enabled(&self) -> bool {
        true
    }

    /// Enable or disable the widget.
    fn set_enabled(&mut self, _enabled: bool) {}

    /// What a copy takes from this widget, or None if it holds nothing to copy.
    ///
    /// The container is what binds the keys and what talks to the clipboard, so
    /// a widget only has to say what its content is; a widget that implements
    /// these three gets copy, cut and paste without knowing the clipboard
    /// exists.
    fn clipboard_copy(&self) -> Option<String> {
        None
    }

    /// Remove what [`Widget::clipboard_copy`] would have taken. Cut is a copy
    /// followed by this.
    fn clipboard_cut(&mut self) -> Option<WidgetEvent> {
        None
    }

    /// Insert pasted text at the cursor.
    fn clipboard_paste(&mut self, _text: &str) -> Option<WidgetEvent> {
        None
    }
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
    /// Fill of a control that is present but cannot be used.
    pub const CONTROL_DISABLED: u32 = Theme::DEFAULT.control_disabled.raw();
    /// Label of a control that is present but cannot be used.
    pub const TEXT_DISABLED: u32 = Theme::DEFAULT.text_disabled.raw();
    /// Text that names or explains a control rather than being one.
    pub const LABEL_TEXT: u32 = Theme::DEFAULT.label_text.raw();
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

/// Draw a line of interface text with its top edge at `y`.
pub fn draw_text(
    buffer: &mut [u32],
    buffer_width: u32,
    buffer_height: u32,
    x: i32,
    y: i32,
    text: &str,
    color: u32,
) {
    draw_text_styled(
        buffer,
        buffer_width,
        buffer_height,
        x,
        y,
        text,
        crate::text::Style::new(color),
    );
}

/// Draw a line of text in an explicit style.
pub fn draw_text_styled(
    buffer: &mut [u32],
    buffer_width: u32,
    buffer_height: u32,
    x: i32,
    y: i32,
    text: &str,
    style: crate::text::Style,
) {
    let mut surface = crate::text::Surface::new(buffer, buffer_width, buffer_height);
    crate::text::draw(&mut surface, x, y, text, style);
}

/// Width of a string set as interface text, in pixels.
///
/// Proportional: a label's width is not its character count times a cell, and
/// laying out from a cell count is how columns end up ragged.
pub fn text_width(text: &str) -> u32 {
    crate::text::width(text, crate::text::Style::new(0))
}

/// Advance of one cell in the monospaced face, for a character grid.
pub fn char_width() -> u32 {
    crate::text::width("M", crate::text::Style::mono(0)).max(1)
}

/// Height of one line of interface text.
pub fn text_height() -> u32 {
    crate::text::line_height(crate::text::Style::new(0))
}
