//! Grid layout - arranges widgets in rows and columns.

use super::{Alignment, HAlign, Insets, Sizable, SizePolicy, VAlign};
use crate::widgets::{Rect, WidgetContainer, WidgetId};

/// Configuration for a grid column.
#[derive(Clone, Debug)]
pub struct ColumnConfig {
    /// Size policy for the column width.
    pub width: SizePolicy,
}

impl Default for ColumnConfig {
    fn default() -> Self {
        Self {
            width: SizePolicy::Preferred,
        }
    }
}

/// Configuration for a grid row.
#[derive(Clone, Debug)]
pub struct RowConfig {
    /// Size policy for the row height.
    pub height: SizePolicy,
}

impl Default for RowConfig {
    fn default() -> Self {
        Self {
            height: SizePolicy::Preferred,
        }
    }
}

/// A cell in the grid containing a widget.
#[derive(Clone, Debug)]
pub struct GridCell {
    /// Widget ID.
    pub widget_id: WidgetId,
    /// Starting row (0-indexed).
    pub row: u32,
    /// Starting column (0-indexed).
    pub col: u32,
    /// Number of rows to span.
    pub row_span: u32,
    /// Number of columns to span.
    pub col_span: u32,
    /// Alignment within the cell.
    pub alignment: Alignment,
    /// Margin around the widget.
    pub margin: Insets,
}

impl GridCell {
    /// Create a new grid cell.
    pub fn new(widget_id: WidgetId, row: u32, col: u32) -> Self {
        Self {
            widget_id,
            row,
            col,
            row_span: 1,
            col_span: 1,
            alignment: Alignment::default(),
            margin: Insets::default(),
        }
    }

    /// Set the row span.
    pub fn with_row_span(mut self, span: u32) -> Self {
        self.row_span = span.max(1);
        self
    }

    /// Set the column span.
    pub fn with_col_span(mut self, span: u32) -> Self {
        self.col_span = span.max(1);
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
}

/// A layout that arranges widgets in a grid of rows and columns.
#[derive(Clone, Debug)]
pub struct GridLayout {
    cells: Vec<GridCell>,
    columns: Vec<ColumnConfig>,
    rows: Vec<RowConfig>,
    bounds: Rect,
    padding: Insets,
    row_spacing: u32,
    col_spacing: u32,
}

impl GridLayout {
    /// Create a new grid layout with the specified number of columns and rows.
    pub fn new(columns: u32, rows: u32) -> Self {
        Self {
            cells: Vec::new(),
            columns: vec![ColumnConfig::default(); columns as usize],
            rows: vec![RowConfig::default(); rows as usize],
            bounds: Rect::new(0, 0, 0, 0),
            padding: Insets::default(),
            row_spacing: 0,
            col_spacing: 0,
        }
    }

    /// Add a widget at the specified row and column.
    pub fn add_at(&mut self, widget_id: WidgetId, row: u32, col: u32) -> &mut GridCell {
        self.cells.push(GridCell::new(widget_id, row, col));
        self.cells.last_mut().unwrap()
    }

    /// Add a widget with row and column span.
    pub fn add_with_span(
        &mut self,
        widget_id: WidgetId,
        row: u32,
        col: u32,
        row_span: u32,
        col_span: u32,
    ) -> &mut GridCell {
        self.cells.push(
            GridCell::new(widget_id, row, col)
                .with_row_span(row_span)
                .with_col_span(col_span),
        );
        self.cells.last_mut().unwrap()
    }

    /// Configure a column's width policy.
    pub fn set_column(&mut self, col: u32, width: SizePolicy) {
        if let Some(config) = self.columns.get_mut(col as usize) {
            config.width = width;
        }
    }

    /// Configure a row's height policy.
    pub fn set_row(&mut self, row: u32, height: SizePolicy) {
        if let Some(config) = self.rows.get_mut(row as usize) {
            config.height = height;
        }
    }

    /// Set the padding around the grid content.
    pub fn set_padding(&mut self, padding: Insets) {
        self.padding = padding;
    }

    /// Get the current padding.
    pub fn padding(&self) -> Insets {
        self.padding
    }

    /// Set the spacing between rows.
    pub fn set_row_spacing(&mut self, spacing: u32) {
        self.row_spacing = spacing;
    }

    /// Set the spacing between columns.
    pub fn set_col_spacing(&mut self, spacing: u32) {
        self.col_spacing = spacing;
    }

    /// Set both row and column spacing.
    pub fn set_spacing(&mut self, spacing: u32) {
        self.row_spacing = spacing;
        self.col_spacing = spacing;
    }

    /// Set the bounds for the layout.
    pub fn set_bounds(&mut self, bounds: Rect) {
        self.bounds = bounds;
    }

    /// Get the current bounds.
    pub fn bounds(&self) -> Rect {
        self.bounds
    }

    /// Get the number of columns.
    pub fn num_columns(&self) -> u32 {
        self.columns.len() as u32
    }

    /// Get the number of rows.
    pub fn num_rows(&self) -> u32 {
        self.rows.len() as u32
    }

    /// Clear all cells from the grid.
    pub fn clear(&mut self) {
        self.cells.clear();
    }

    /// Calculate positions and sizes for all widgets.
    pub fn layout(&self, container: &mut WidgetContainer) {
        if self.cells.is_empty() || self.columns.is_empty() || self.rows.is_empty() {
            return;
        }

        // Calculate content area
        let content_x = self.bounds.x + self.padding.left as i32;
        let content_y = self.bounds.y + self.padding.top as i32;
        let content_width = self.bounds.width.saturating_sub(self.padding.horizontal());
        let content_height = self.bounds.height.saturating_sub(self.padding.vertical());

        // Calculate column widths
        let column_widths = self.calculate_sizes(
            &self.columns.iter().map(|c| c.width).collect::<Vec<_>>(),
            content_width,
            self.col_spacing,
            |col| self.max_preferred_width_for_column(col, container),
        );

        // Calculate row heights
        let row_heights = self.calculate_sizes(
            &self.rows.iter().map(|r| r.height).collect::<Vec<_>>(),
            content_height,
            self.row_spacing,
            |row| self.max_preferred_height_for_row(row, container),
        );

        // Calculate column positions (cumulative)
        let mut col_positions: Vec<i32> = Vec::with_capacity(self.columns.len());
        let mut x = content_x;
        for width in &column_widths {
            col_positions.push(x);
            x += *width as i32 + self.col_spacing as i32;
        }

        // Calculate row positions (cumulative)
        let mut row_positions: Vec<i32> = Vec::with_capacity(self.rows.len());
        let mut y = content_y;
        for height in &row_heights {
            row_positions.push(y);
            y += *height as i32 + self.row_spacing as i32;
        }

        // Position widgets
        for cell in &self.cells {
            let col = cell.col as usize;
            let row = cell.row as usize;

            if col >= self.columns.len() || row >= self.rows.len() {
                continue;
            }

            if let Some(widget) = container.get_mut(cell.widget_id) {
                // Calculate cell bounds (accounting for spans)
                let cell_x = col_positions[col];
                let cell_y = row_positions[row];

                let cell_width: u32 = (col..col + cell.col_span as usize)
                    .filter_map(|c| column_widths.get(c))
                    .sum::<u32>()
                    + (cell.col_span.saturating_sub(1)) * self.col_spacing;

                let cell_height: u32 = (row..row + cell.row_span as usize)
                    .filter_map(|r| row_heights.get(r))
                    .sum::<u32>()
                    + (cell.row_span.saturating_sub(1)) * self.row_spacing;

                // Apply margin
                let available_x = cell_x + cell.margin.left as i32;
                let available_y = cell_y + cell.margin.top as i32;
                let available_width = cell_width.saturating_sub(cell.margin.horizontal());
                let available_height = cell_height.saturating_sub(cell.margin.vertical());

                // Get widget preferred size
                let widget_size = widget.size_hint();
                let widget_width = widget_size.preferred_width.min(available_width);
                let widget_height = widget_size.preferred_height.min(available_height);

                // Calculate position based on alignment
                let final_x = match cell.alignment.horizontal {
                    HAlign::Start => available_x,
                    HAlign::Center => {
                        available_x + (available_width.saturating_sub(widget_width) / 2) as i32
                    }
                    HAlign::End => {
                        available_x + available_width.saturating_sub(widget_width) as i32
                    }
                    HAlign::Stretch => available_x,
                };

                let final_y = match cell.alignment.vertical {
                    VAlign::Start => available_y,
                    VAlign::Center => {
                        available_y + (available_height.saturating_sub(widget_height) / 2) as i32
                    }
                    VAlign::End => {
                        available_y + available_height.saturating_sub(widget_height) as i32
                    }
                    VAlign::Stretch => available_y,
                };

                widget.set_position(final_x, final_y);
            }
        }
    }

    /// Calculate sizes for columns or rows.
    fn calculate_sizes<F>(
        &self,
        policies: &[SizePolicy],
        total_size: u32,
        spacing: u32,
        preferred_size_fn: F,
    ) -> Vec<u32>
    where
        F: Fn(u32) -> u32,
    {
        let count = policies.len();
        if count == 0 {
            return Vec::new();
        }

        let total_spacing = if count > 1 {
            spacing * (count as u32 - 1)
        } else {
            0
        };

        // First pass: calculate preferred sizes and expand weights
        let mut sizes: Vec<u32> = Vec::with_capacity(count);
        let mut total_expand_weight = 0.0f32;
        let mut total_fixed_size = 0u32;

        for (i, policy) in policies.iter().enumerate() {
            let (size, weight) = match policy {
                SizePolicy::Fixed(s) => (*s, 0.0),
                SizePolicy::Preferred => (preferred_size_fn(i as u32), 0.0),
                SizePolicy::Expand { weight } => (0, *weight),
            };

            sizes.push(size);
            total_expand_weight += weight;
            if weight == 0.0 {
                total_fixed_size += size;
            }
        }

        // Calculate remaining space for expanding items
        let remaining = total_size
            .saturating_sub(total_fixed_size)
            .saturating_sub(total_spacing);

        // Second pass: distribute remaining space to expanding items
        for (i, policy) in policies.iter().enumerate() {
            if let SizePolicy::Expand { weight } = policy
                && total_expand_weight > 0.0
            {
                sizes[i] = ((remaining as f32 * weight) / total_expand_weight) as u32;
            }
        }

        sizes
    }

    /// Get the maximum preferred width of widgets in a column.
    fn max_preferred_width_for_column(&self, col: u32, container: &WidgetContainer) -> u32 {
        self.cells
            .iter()
            .filter(|cell| cell.col == col && cell.col_span == 1)
            .filter_map(|cell| container.get(cell.widget_id))
            .map(|widget| widget.size_hint().preferred_width)
            .max()
            .unwrap_or(0)
    }

    /// Get the maximum preferred height of widgets in a row.
    fn max_preferred_height_for_row(&self, row: u32, container: &WidgetContainer) -> u32 {
        self.cells
            .iter()
            .filter(|cell| cell.row == row && cell.row_span == 1)
            .filter_map(|cell| container.get(cell.widget_id))
            .map(|widget| widget.size_hint().preferred_height)
            .max()
            .unwrap_or(0)
    }
}

impl Default for GridLayout {
    fn default() -> Self {
        Self::new(1, 1)
    }
}
