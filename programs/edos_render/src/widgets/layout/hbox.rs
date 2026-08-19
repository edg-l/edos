//! Horizontal box layout - arranges widgets horizontally.

use super::{HAlign, Insets, LayoutItem, Sizable, SizePolicy, VAlign};
use crate::widgets::{Rect, WidgetContainer, WidgetId};

/// A layout that arranges widgets horizontally from left to right.
#[derive(Clone, Debug)]
pub struct HBoxLayout {
    items: Vec<LayoutItem>,
    bounds: Rect,
    padding: Insets,
    spacing: u32,
    uniform_columns: bool,
}

impl HBoxLayout {
    /// Create a new empty horizontal box layout.
    pub fn new() -> Self {
        Self {
            items: Vec::new(),
            bounds: Rect::new(0, 0, 0, 0),
            padding: Insets::default(),
            spacing: 0,
            uniform_columns: false,
        }
    }

    /// Add a widget to the layout and return a mutable reference to the layout item.
    pub fn add(&mut self, widget_id: WidgetId) -> &mut LayoutItem {
        self.items.push(LayoutItem::widget(widget_id));
        self.items.last_mut().unwrap()
    }

    /// Add a stretchable spacer with the given weight.
    pub fn add_stretch(&mut self, weight: f32) {
        self.items.push(LayoutItem::spacer(weight));
    }

    /// Set the padding around the layout content.
    pub fn set_padding(&mut self, padding: Insets) {
        self.padding = padding;
    }

    /// Get the current padding.
    pub fn padding(&self) -> Insets {
        self.padding
    }

    /// Give every content-sized item the width of the widest one.
    ///
    /// Controls that each size to their own label put the next column at a
    /// different x on every row, which reads as a broken grid. Items with an
    /// explicit width keep it, so a fixed label column still leads the row.
    pub fn set_uniform_columns(&mut self, uniform: bool) {
        self.uniform_columns = uniform;
    }

    /// Set the spacing between items.
    pub fn set_spacing(&mut self, spacing: u32) {
        self.spacing = spacing;
    }

    /// Get the current spacing.
    pub fn spacing(&self) -> u32 {
        self.spacing
    }

    /// Set the bounds for the layout.
    pub fn set_bounds(&mut self, bounds: Rect) {
        self.bounds = bounds;
    }

    /// Get the current bounds.
    pub fn bounds(&self) -> Rect {
        self.bounds
    }

    /// Get the number of items in the layout.
    pub fn len(&self) -> usize {
        self.items.len()
    }

    /// Check if the layout is empty.
    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    /// Clear all items from the layout.
    pub fn clear(&mut self) {
        self.items.clear();
    }

    /// Calculate positions and sizes for all widgets.
    pub fn layout(&self, container: &mut WidgetContainer) {
        if self.items.is_empty() {
            return;
        }

        // Calculate content area
        let content_x = self.bounds.x + self.padding.left as i32;
        let content_y = self.bounds.y + self.padding.top as i32;
        let content_width = self.bounds.width.saturating_sub(self.padding.horizontal());
        let content_height = self.bounds.height.saturating_sub(self.padding.vertical());

        // Calculate total spacing
        let total_spacing = if self.items.len() > 1 {
            self.spacing * (self.items.len() as u32 - 1)
        } else {
            0
        };

        // First pass: calculate preferred widths and total expand weight
        let mut preferred_widths: Vec<u32> = Vec::with_capacity(self.items.len());
        let mut total_expand_weight = 0.0f32;
        let mut total_fixed_width = 0u32;

        for item in &self.items {
            let (width, weight) = match item.width_policy {
                SizePolicy::Fixed(w) => (w, 0.0),
                SizePolicy::Preferred => {
                    let w = if let Some(id) = item.widget_id {
                        if let Some(widget) = container.get(id) {
                            widget.size_hint().preferred_width
                        } else {
                            0
                        }
                    } else {
                        0
                    };
                    (w, 0.0)
                }
                SizePolicy::Expand { weight } => (0, weight),
            };

            let width_with_margin = width + item.margin.horizontal();
            preferred_widths.push(width_with_margin);
            total_expand_weight += weight;
            if weight == 0.0 {
                total_fixed_width += width_with_margin;
            }
        }

        if self.uniform_columns {
            let widest = preferred_widths
                .iter()
                .zip(&self.items)
                .filter(|(_, item)| matches!(item.width_policy, SizePolicy::Preferred))
                .map(|(w, _)| *w)
                .max()
                .unwrap_or(0);
            for (width, item) in preferred_widths.iter_mut().zip(&self.items) {
                if matches!(item.width_policy, SizePolicy::Preferred) {
                    total_fixed_width = total_fixed_width - *width + widest;
                    *width = widest;
                }
            }
        }

        // Calculate remaining space for expanding items
        let remaining_width = content_width
            .saturating_sub(total_fixed_width)
            .saturating_sub(total_spacing);

        // Second pass: calculate actual widths
        let mut actual_widths: Vec<u32> = Vec::with_capacity(self.items.len());
        for (i, item) in self.items.iter().enumerate() {
            let width = match item.width_policy {
                SizePolicy::Expand { weight } => {
                    if total_expand_weight > 0.0 {
                        ((remaining_width as f32 * weight) / total_expand_weight) as u32
                    } else {
                        0
                    }
                }
                _ => preferred_widths[i],
            };
            actual_widths.push(width);
        }

        // Third pass: position widgets
        let mut x = content_x;
        for (i, item) in self.items.iter().enumerate() {
            if let Some(id) = item.widget_id
                && let Some(widget) = container.get_mut(id) {
                    let item_width = actual_widths[i].saturating_sub(item.margin.horizontal());
                    let item_x = x + item.margin.left as i32;

                    // Calculate widget height based on policy
                    let widget_height = match item.height_policy {
                        SizePolicy::Fixed(h) => h,
                        SizePolicy::Preferred => widget.size_hint().preferred_height,
                        SizePolicy::Expand { .. } => {
                            content_height.saturating_sub(item.margin.vertical())
                        }
                    };

                    // Calculate widget width
                    let widget_width = match item.width_policy {
                        SizePolicy::Fixed(w) => w,
                        SizePolicy::Preferred => widget.size_hint().preferred_width,
                        SizePolicy::Expand { .. } => item_width,
                    };

                    // Calculate y position based on vertical alignment
                    let available_height = content_height.saturating_sub(item.margin.vertical());
                    let y = match item.alignment.vertical {
                        VAlign::Start => content_y + item.margin.top as i32,
                        VAlign::Center => {
                            content_y
                                + item.margin.top as i32
                                + (available_height.saturating_sub(widget_height) / 2) as i32
                        }
                        VAlign::End => {
                            content_y
                                + item.margin.top as i32
                                + available_height.saturating_sub(widget_height) as i32
                        }
                        VAlign::Stretch => content_y + item.margin.top as i32,
                    };

                    // Calculate x position based on horizontal alignment within allocated space
                    let final_x = match item.alignment.horizontal {
                        HAlign::Start => item_x,
                        HAlign::Center => {
                            item_x + (item_width.saturating_sub(widget_width) / 2) as i32
                        }
                        HAlign::End => item_x + item_width.saturating_sub(widget_width) as i32,
                        HAlign::Stretch => item_x,
                    };

                    widget.set_position(final_x, y);
                }

            x += actual_widths[i] as i32 + self.spacing as i32;
        }
    }
}

impl Default for HBoxLayout {
    fn default() -> Self {
        Self::new()
    }
}
