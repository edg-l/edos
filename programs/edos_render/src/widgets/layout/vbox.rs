//! Vertical box layout - stacks widgets vertically.

use super::{HAlign, Insets, LayoutItem, Sizable, SizePolicy, VAlign};
use crate::widgets::{Rect, WidgetContainer, WidgetId};

/// A layout that stacks widgets vertically from top to bottom.
#[derive(Clone, Debug)]
pub struct VBoxLayout {
    items: Vec<LayoutItem>,
    bounds: Rect,
    padding: Insets,
    spacing: u32,
}

impl VBoxLayout {
    /// Create a new empty vertical box layout.
    pub fn new() -> Self {
        Self {
            items: Vec::new(),
            bounds: Rect::new(0, 0, 0, 0),
            padding: Insets::default(),
            spacing: 0,
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

        // First pass: calculate preferred heights and total expand weight
        let mut preferred_heights: Vec<u32> = Vec::with_capacity(self.items.len());
        let mut total_expand_weight = 0.0f32;
        let mut total_fixed_height = 0u32;

        for item in &self.items {
            let (height, weight) = match item.height_policy {
                SizePolicy::Fixed(h) => (h, 0.0),
                SizePolicy::Preferred => {
                    let h = if let Some(id) = item.widget_id {
                        if let Some(widget) = container.get(id) {
                            widget.size_hint().preferred_height
                        } else {
                            0
                        }
                    } else {
                        0
                    };
                    (h, 0.0)
                }
                SizePolicy::Expand { weight } => (0, weight),
            };

            let height_with_margin = height + item.margin.vertical();
            preferred_heights.push(height_with_margin);
            total_expand_weight += weight;
            if weight == 0.0 {
                total_fixed_height += height_with_margin;
            }
        }

        // Calculate remaining space for expanding items
        let remaining_height = content_height
            .saturating_sub(total_fixed_height)
            .saturating_sub(total_spacing);

        // Second pass: calculate actual heights
        let mut actual_heights: Vec<u32> = Vec::with_capacity(self.items.len());
        for (i, item) in self.items.iter().enumerate() {
            let height = match item.height_policy {
                SizePolicy::Expand { weight } => {
                    if total_expand_weight > 0.0 {
                        ((remaining_height as f32 * weight) / total_expand_weight) as u32
                    } else {
                        0
                    }
                }
                _ => preferred_heights[i],
            };
            actual_heights.push(height);
        }

        // Third pass: position widgets
        let mut y = content_y;
        for (i, item) in self.items.iter().enumerate() {
            if let Some(id) = item.widget_id
                && let Some(widget) = container.get_mut(id)
            {
                let item_height = actual_heights[i].saturating_sub(item.margin.vertical());
                let item_y = y + item.margin.top as i32;

                // Calculate widget width based on policy
                let widget_width = match item.width_policy {
                    SizePolicy::Fixed(w) => w,
                    SizePolicy::Preferred => widget.size_hint().preferred_width,
                    SizePolicy::Expand { .. } => {
                        content_width.saturating_sub(item.margin.horizontal())
                    }
                };

                // Calculate widget height
                let widget_height = match item.height_policy {
                    SizePolicy::Fixed(h) => h,
                    SizePolicy::Preferred => widget.size_hint().preferred_height,
                    SizePolicy::Expand { .. } => item_height,
                };

                // Calculate x position based on horizontal alignment
                let available_width = content_width.saturating_sub(item.margin.horizontal());
                let x = match item.alignment.horizontal {
                    HAlign::Start => content_x + item.margin.left as i32,
                    HAlign::Center => {
                        content_x
                            + item.margin.left as i32
                            + (available_width.saturating_sub(widget_width) / 2) as i32
                    }
                    HAlign::End => {
                        content_x
                            + item.margin.left as i32
                            + available_width.saturating_sub(widget_width) as i32
                    }
                    HAlign::Stretch => content_x + item.margin.left as i32,
                };

                // Calculate y position based on vertical alignment within allocated space
                let final_y = match item.alignment.vertical {
                    VAlign::Start => item_y,
                    VAlign::Center => {
                        item_y + (item_height.saturating_sub(widget_height) / 2) as i32
                    }
                    VAlign::End => item_y + item_height.saturating_sub(widget_height) as i32,
                    VAlign::Stretch => item_y,
                };

                widget.set_position(x, final_y);
            }

            y += actual_heights[i] as i32 + self.spacing as i32;
        }
    }
}

impl Default for VBoxLayout {
    fn default() -> Self {
        Self::new()
    }
}
