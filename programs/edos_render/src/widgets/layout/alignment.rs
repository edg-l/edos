//! Alignment types for layout positioning.

/// Horizontal alignment within a layout cell.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum HAlign {
    /// Align to the start (left).
    #[default]
    Start,
    /// Center horizontally.
    Center,
    /// Align to the end (right).
    End,
    /// Stretch to fill available width.
    Stretch,
}

/// Vertical alignment within a layout cell.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum VAlign {
    /// Align to the start (top).
    #[default]
    Start,
    /// Center vertically.
    Center,
    /// Align to the end (bottom).
    End,
    /// Stretch to fill available height.
    Stretch,
}

/// Combined horizontal and vertical alignment.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Alignment {
    pub horizontal: HAlign,
    pub vertical: VAlign,
}

impl Alignment {
    /// Create a new alignment with specified horizontal and vertical values.
    pub fn new(horizontal: HAlign, vertical: VAlign) -> Self {
        Self {
            horizontal,
            vertical,
        }
    }

    /// Top-left alignment.
    pub fn top_left() -> Self {
        Self::new(HAlign::Start, VAlign::Start)
    }

    /// Top-center alignment.
    pub fn top_center() -> Self {
        Self::new(HAlign::Center, VAlign::Start)
    }

    /// Top-right alignment.
    pub fn top_right() -> Self {
        Self::new(HAlign::End, VAlign::Start)
    }

    /// Center-left alignment.
    pub fn center_left() -> Self {
        Self::new(HAlign::Start, VAlign::Center)
    }

    /// Center alignment (both axes).
    pub fn center() -> Self {
        Self::new(HAlign::Center, VAlign::Center)
    }

    /// Center-right alignment.
    pub fn center_right() -> Self {
        Self::new(HAlign::End, VAlign::Center)
    }

    /// Bottom-left alignment.
    pub fn bottom_left() -> Self {
        Self::new(HAlign::Start, VAlign::End)
    }

    /// Bottom-center alignment.
    pub fn bottom_center() -> Self {
        Self::new(HAlign::Center, VAlign::End)
    }

    /// Bottom-right alignment.
    pub fn bottom_right() -> Self {
        Self::new(HAlign::End, VAlign::End)
    }

    /// Fill both axes.
    pub fn fill() -> Self {
        Self::new(HAlign::Stretch, VAlign::Stretch)
    }
}
