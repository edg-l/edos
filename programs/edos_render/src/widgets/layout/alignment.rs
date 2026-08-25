//! Alignment types for layout positioning.

/// Where an item sits within the space a layout allocated it, on one axis.
///
/// One enum serves both axes: `Start` is left or top, `End` is right or
/// bottom, and which one it means is decided by the field it is stored in.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Align {
    /// Align to the start of the axis (left, or top).
    #[default]
    Start,
    /// Center on the axis.
    Center,
    /// Align to the end of the axis (right, or bottom).
    End,
    /// Stretch to fill the axis.
    Stretch,
}

/// Combined horizontal and vertical alignment.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Alignment {
    pub horizontal: Align,
    pub vertical: Align,
}

impl Alignment {
    /// Create a new alignment with specified horizontal and vertical values.
    pub fn new(horizontal: Align, vertical: Align) -> Self {
        Self {
            horizontal,
            vertical,
        }
    }

    /// Top-left alignment.
    pub fn top_left() -> Self {
        Self::new(Align::Start, Align::Start)
    }

    /// Top-center alignment.
    pub fn top_center() -> Self {
        Self::new(Align::Center, Align::Start)
    }

    /// Top-right alignment.
    pub fn top_right() -> Self {
        Self::new(Align::End, Align::Start)
    }

    /// Center-left alignment.
    pub fn center_left() -> Self {
        Self::new(Align::Start, Align::Center)
    }

    /// Center alignment (both axes).
    pub fn center() -> Self {
        Self::new(Align::Center, Align::Center)
    }

    /// Center-right alignment.
    pub fn center_right() -> Self {
        Self::new(Align::End, Align::Center)
    }

    /// Bottom-left alignment.
    pub fn bottom_left() -> Self {
        Self::new(Align::Start, Align::End)
    }

    /// Bottom-center alignment.
    pub fn bottom_center() -> Self {
        Self::new(Align::Center, Align::End)
    }

    /// Bottom-right alignment.
    pub fn bottom_right() -> Self {
        Self::new(Align::End, Align::End)
    }

    /// Fill both axes.
    pub fn fill() -> Self {
        Self::new(Align::Stretch, Align::Stretch)
    }
}
