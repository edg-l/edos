//! Box layout: items in a line along one axis.

use super::{Align, Insets, LayoutItem, Sizable, SizeHint, SizePolicy};
use crate::widgets::{Rect, WidgetContainer, WidgetId};

/// The axis a [`LinearLayout`] runs along. The other one is its cross axis,
/// on which every item is aligned within the layout's full content extent.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Axis {
    /// Left to right.
    Horizontal,
    /// Top to bottom.
    Vertical,
}

impl Axis {
    /// The axis at right angles to this one.
    pub fn cross(self) -> Self {
        match self {
            Axis::Horizontal => Axis::Vertical,
            Axis::Vertical => Axis::Horizontal,
        }
    }

    /// The size policy an item applies along this axis.
    fn policy(self, item: &LayoutItem) -> SizePolicy {
        match self {
            Axis::Horizontal => item.width_policy,
            Axis::Vertical => item.height_policy,
        }
    }

    /// The alignment an item asks for along this axis.
    fn align(self, item: &LayoutItem) -> Align {
        match self {
            Axis::Horizontal => item.alignment.horizontal,
            Axis::Vertical => item.alignment.vertical,
        }
    }

    /// A widget's preferred extent along this axis.
    fn hint(self, hint: SizeHint) -> u32 {
        match self {
            Axis::Horizontal => hint.preferred_width,
            Axis::Vertical => hint.preferred_height,
        }
    }

    /// Both margins on this axis, as one number.
    fn margin(self, margin: Insets) -> u32 {
        match self {
            Axis::Horizontal => margin.horizontal(),
            Axis::Vertical => margin.vertical(),
        }
    }

    /// The leading margin on this axis.
    fn lead(self, margin: Insets) -> u32 {
        match self {
            Axis::Horizontal => margin.left,
            Axis::Vertical => margin.top,
        }
    }

    /// Where a rectangle starts on this axis.
    fn origin(self, rect: Rect) -> i32 {
        match self {
            Axis::Horizontal => rect.x,
            Axis::Vertical => rect.y,
        }
    }

    /// How far a rectangle reaches along this axis.
    fn extent(self, rect: Rect) -> u32 {
        match self {
            Axis::Horizontal => rect.width,
            Axis::Vertical => rect.height,
        }
    }
}

/// Offset from the start of an allocated span to where an item of `size`
/// should sit inside it.
fn align_offset(align: Align, available: u32, size: u32) -> u32 {
    match align {
        Align::Start | Align::Stretch => 0,
        Align::Center => available.saturating_sub(size) / 2,
        Align::End => available.saturating_sub(size),
    }
}

/// A layout that arranges widgets in a line along one [`Axis`].
///
/// `HBox` and `VBox` are the same algorithm with the roles of width and height
/// swapped, so they are one type here: [`LinearLayout::horizontal`] and
/// [`LinearLayout::vertical`] pick which.
#[derive(Clone, Debug)]
pub struct LinearLayout {
    axis: Axis,
    items: Vec<LayoutItem>,
    bounds: Rect,
    padding: Insets,
    spacing: u32,
    uniform: bool,
}

impl LinearLayout {
    /// Create a new empty layout running along `axis`.
    pub fn new(axis: Axis) -> Self {
        Self {
            axis,
            items: Vec::new(),
            bounds: Rect::new(0, 0, 0, 0),
            padding: Insets::default(),
            spacing: 0,
            uniform: false,
        }
    }

    /// Create a new empty layout arranging widgets left to right.
    pub fn horizontal() -> Self {
        Self::new(Axis::Horizontal)
    }

    /// Create a new empty layout stacking widgets top to bottom.
    pub fn vertical() -> Self {
        Self::new(Axis::Vertical)
    }

    /// The axis this layout runs along.
    pub fn axis(&self) -> Axis {
        self.axis
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

    /// Give every content-sized item the extent of the largest one.
    ///
    /// Controls that each size to their own label put the next one at a
    /// different offset on every row, which reads as a broken grid. Items with
    /// an explicit size keep it, so a fixed label column still leads the row.
    pub fn set_uniform(&mut self, uniform: bool) {
        self.uniform = uniform;
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

    /// The extent each item is allocated along the main axis, margins included.
    ///
    /// Content-sized and fixed items take what they ask for; what is left over
    /// after them and the inter-item spacing is shared between the expanding
    /// items in proportion to their weights.
    fn main_extents(&self, container: &WidgetContainer, content_main: u32) -> Vec<u32> {
        let main = self.axis;

        let mut extents: Vec<u32> = Vec::with_capacity(self.items.len());
        let mut total_weight = 0.0f32;
        let mut total_fixed = 0u32;

        for item in &self.items {
            let (size, weight) = match main.policy(item) {
                SizePolicy::Fixed(size) => (size, 0.0),
                SizePolicy::Preferred => {
                    let size = item
                        .widget_id
                        .and_then(|id| container.get(id))
                        .map(|widget| main.hint(widget.size_hint()))
                        .unwrap_or(0);
                    (size, 0.0)
                }
                SizePolicy::Expand { weight } => (0, weight),
            };

            let with_margin = size + main.margin(item.margin);
            extents.push(with_margin);
            total_weight += weight;
            if weight == 0.0 {
                total_fixed += with_margin;
            }
        }

        let is_content_sized =
            |item: &LayoutItem| matches!(main.policy(item), SizePolicy::Preferred);

        if self.uniform {
            let largest = extents
                .iter()
                .zip(&self.items)
                .filter(|(_, item)| is_content_sized(item))
                .map(|(size, _)| *size)
                .max()
                .unwrap_or(0);
            for (size, item) in extents.iter_mut().zip(&self.items) {
                if is_content_sized(item) {
                    total_fixed = total_fixed - *size + largest;
                    *size = largest;
                }
            }
        }

        let spacing = self.spacing * (self.items.len().saturating_sub(1)) as u32;
        let remaining = content_main
            .saturating_sub(total_fixed)
            .saturating_sub(spacing);

        for (extent, item) in extents.iter_mut().zip(&self.items) {
            if let SizePolicy::Expand { weight } = main.policy(item) {
                *extent = if total_weight > 0.0 {
                    ((remaining as f32 * weight) / total_weight) as u32
                } else {
                    0
                };
            }
        }

        extents
    }

    /// Calculate positions and sizes for all widgets.
    pub fn layout(&self, container: &mut WidgetContainer) {
        if self.items.is_empty() {
            return;
        }

        let main = self.axis;
        let cross = main.cross();

        let content_main = main
            .extent(self.bounds)
            .saturating_sub(main.margin(self.padding));
        let content_cross = cross
            .extent(self.bounds)
            .saturating_sub(cross.margin(self.padding));
        let content_cross_origin = cross.origin(self.bounds) + cross.lead(self.padding) as i32;

        let extents = self.main_extents(container, content_main);

        let mut offset = main.origin(self.bounds) + main.lead(self.padding) as i32;
        for (item, allocated) in self.items.iter().zip(&extents) {
            if let Some(id) = item.widget_id
                && let Some(widget) = container.get_mut(id)
            {
                let hint = widget.size_hint();
                let inner = allocated.saturating_sub(main.margin(item.margin));

                let main_size = match main.policy(item) {
                    SizePolicy::Fixed(size) => size,
                    SizePolicy::Preferred => main.hint(hint),
                    SizePolicy::Expand { .. } => inner,
                };
                let cross_size = match cross.policy(item) {
                    SizePolicy::Fixed(size) => size,
                    SizePolicy::Preferred => cross.hint(hint),
                    SizePolicy::Expand { .. } => {
                        content_cross.saturating_sub(cross.margin(item.margin))
                    }
                };

                // The main axis aligns within the item's own allocation; the
                // cross axis aligns within the layout's whole content extent.
                let main_start = offset + main.lead(item.margin) as i32;
                let main_pos = main_start + align_offset(main.align(item), inner, main_size) as i32;

                let cross_start = content_cross_origin + cross.lead(item.margin) as i32;
                let cross_available = content_cross.saturating_sub(cross.margin(item.margin));
                let cross_pos = cross_start
                    + align_offset(cross.align(item), cross_available, cross_size) as i32;

                let (x, y) = match main {
                    Axis::Horizontal => (main_pos, cross_pos),
                    Axis::Vertical => (cross_pos, main_pos),
                };
                widget.set_position(x, y);
            }

            offset += *allocated as i32 + self.spacing as i32;
        }
    }
}
