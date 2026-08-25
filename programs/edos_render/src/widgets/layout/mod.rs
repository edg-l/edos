//! Layout system for automatic widget positioning.
//!
//! This module provides layout containers that automatically position
//! their child widgets according to various rules.
//!
//! # Available Layouts
//!
//! - [`VBoxLayout`] - Stacks widgets vertically
//! - [`HBoxLayout`] - Arranges widgets horizontally
//! - [`GridLayout`] - Arranges widgets in a grid
//!
//! # Example
//!
//! ```ignore
//! use edos_render::widgets::{WidgetContainer, Label, Button};
//! use edos_render::widgets::layout::{VBoxLayout, Insets, Rect};
//!
//! let mut widgets = WidgetContainer::new();
//! let label = widgets.add(Label::new(0, 0, "Hello"));
//! let button = widgets.add(Button::new(0, 0, "Click"));
//!
//! let mut layout = VBoxLayout::new();
//! layout.set_padding(Insets::all(10));
//! layout.set_spacing(5);
//! layout.set_bounds(Rect::new(0, 0, 200, 100));
//! layout.add(label);
//! layout.add(button);
//! layout.layout(&mut widgets);
//! ```

pub mod alignment;
pub mod grid;
pub mod hbox;
pub mod size;
pub mod vbox;

pub use alignment::{Alignment, HAlign, VAlign};
pub use grid::{GridCell, GridLayout};
pub use hbox::HBoxLayout;
pub use size::{SizeHint, SizePolicy, Sizing};
pub use vbox::VBoxLayout;

use super::{Widget, WidgetId};

/// Padding/margin around a rectangular area.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Insets {
    pub top: u32,
    pub right: u32,
    pub bottom: u32,
    pub left: u32,
}

impl Insets {
    /// Create insets with all sides set to the same value.
    pub fn all(value: u32) -> Self {
        Self {
            top: value,
            right: value,
            bottom: value,
            left: value,
        }
    }

    /// Create insets with symmetric horizontal and vertical values.
    pub fn symmetric(horizontal: u32, vertical: u32) -> Self {
        Self {
            top: vertical,
            right: horizontal,
            bottom: vertical,
            left: horizontal,
        }
    }

    /// Create insets with individual values for each side.
    pub fn new(top: u32, right: u32, bottom: u32, left: u32) -> Self {
        Self {
            top,
            right,
            bottom,
            left,
        }
    }

    /// Total horizontal insets (left + right).
    pub fn horizontal(&self) -> u32 {
        self.left + self.right
    }

    /// Total vertical insets (top + bottom).
    pub fn vertical(&self) -> u32 {
        self.top + self.bottom
    }
}

/// Trait for widgets that can report their preferred size.
pub trait Sizable {
    /// Get the size hint for this widget.
    fn size_hint(&self) -> SizeHint;
}

/// Default implementation for any Widget - uses bounds as preferred size.
impl<T: Widget + ?Sized> Sizable for T {
    fn size_hint(&self) -> SizeHint {
        let bounds = self.bounds();
        SizeHint::preferred(bounds.width, bounds.height)
    }
}

/// An item in a layout, which can be a widget or a spacer.
#[derive(Clone, Debug)]
pub struct LayoutItem {
    /// The widget ID, or None for a spacer.
    pub widget_id: Option<WidgetId>,
    /// Horizontal size policy.
    pub width_policy: SizePolicy,
    /// Vertical size policy.
    pub height_policy: SizePolicy,
    /// Alignment within the allocated space.
    pub alignment: Alignment,
    /// Margin around this item.
    pub margin: Insets,
}

impl LayoutItem {
    /// Create a new layout item for a widget.
    pub fn widget(id: WidgetId) -> Self {
        Self {
            widget_id: Some(id),
            width_policy: SizePolicy::Preferred,
            height_policy: SizePolicy::Preferred,
            alignment: Alignment::default(),
            margin: Insets::default(),
        }
    }

    /// Create a spacer with the given expand weight.
    pub fn spacer(weight: f32) -> Self {
        Self {
            widget_id: None,
            width_policy: SizePolicy::Expand { weight },
            height_policy: SizePolicy::Expand { weight },
            alignment: Alignment::default(),
            margin: Insets::default(),
        }
    }

    /// Set the width policy.
    pub fn with_width(mut self, policy: SizePolicy) -> Self {
        self.width_policy = policy;
        self
    }

    /// Set the height policy.
    pub fn with_height(mut self, policy: SizePolicy) -> Self {
        self.height_policy = policy;
        self
    }

    /// Set the alignment.
    pub fn with_alignment(mut self, alignment: Alignment) -> Self {
        self.alignment = alignment;
        self
    }

    /// Set the margin.
    pub fn with_margin(mut self, margin: Insets) -> Self {
        self.margin = margin;
        self
    }

    /// Set the width policy (mutable reference version).
    pub fn set_width(&mut self, policy: SizePolicy) -> &mut Self {
        self.width_policy = policy;
        self
    }

    /// Set the height policy (mutable reference version).
    pub fn set_height(&mut self, policy: SizePolicy) -> &mut Self {
        self.height_policy = policy;
        self
    }

    /// Set the alignment (mutable reference version).
    pub fn set_alignment(&mut self, alignment: Alignment) -> &mut Self {
        self.alignment = alignment;
        self
    }

    /// Set the margin (mutable reference version).
    pub fn set_margin(&mut self, margin: Insets) -> &mut Self {
        self.margin = margin;
        self
    }
}
