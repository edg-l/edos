//! Dirty rectangle tracking for partial framebuffer updates.

/// Maximum number of dirty rects before collapsing to full-screen.
const MAX_RECTS: usize = 16;

/// A screen-space rectangle that needs to be redrawn.
#[derive(Clone, Copy, Debug, Default)]
pub struct DirtyRect {
    pub x: i32,
    pub y: i32,
    pub w: u32,
    pub h: u32,
}

impl DirtyRect {
    pub fn new(x: i32, y: i32, w: u32, h: u32) -> Self {
        Self { x, y, w, h }
    }

    /// Clip this rect to screen bounds and return None if it becomes empty.
    pub fn clipped(self, screen_w: u32, screen_h: u32) -> Option<Self> {
        let x0 = self.x.max(0);
        let y0 = self.y.max(0);
        let x1 = (self.x + self.w as i32).min(screen_w as i32);
        let y1 = (self.y + self.h as i32).min(screen_h as i32);
        if x1 <= x0 || y1 <= y0 {
            return None;
        }
        Some(Self {
            x: x0,
            y: y0,
            w: (x1 - x0) as u32,
            h: (y1 - y0) as u32,
        })
    }
}

/// Accumulates dirty regions for the current frame.
pub struct DirtyRegion {
    rects: [DirtyRect; MAX_RECTS],
    count: usize,
    /// When true, the entire screen needs redrawing.
    pub full_screen: bool,
}

impl DirtyRegion {
    pub fn new() -> Self {
        Self {
            rects: [DirtyRect::default(); MAX_RECTS],
            count: 0,
            full_screen: false,
        }
    }

    /// Mark a rectangle as dirty.
    pub fn mark_dirty(&mut self, rect: DirtyRect) {
        if self.full_screen {
            return;
        }
        if self.count >= MAX_RECTS {
            self.full_screen = true;
            return;
        }
        self.rects[self.count] = rect;
        self.count += 1;
    }

    /// Mark the entire screen as dirty.
    pub fn mark_full_screen(&mut self) {
        self.full_screen = true;
    }

    /// Returns true if there are no dirty regions.
    #[allow(unused)]
    pub fn is_empty(&self) -> bool {
        !self.full_screen && self.count == 0
    }

    /// Reset all dirty state.
    pub fn clear(&mut self) {
        self.count = 0;
        self.full_screen = false;
    }

    /// Iterate over accumulated dirty rects (only valid when `full_screen` is false).
    pub fn rects(&self) -> &[DirtyRect] {
        &self.rects[..self.count]
    }

    /// Compute the bounding box of all dirty rects.
    #[allow(dead_code)]
    pub fn merged_bounds(&self) -> Option<DirtyRect> {
        if self.count == 0 {
            return None;
        }
        let mut x0 = i32::MAX;
        let mut y0 = i32::MAX;
        let mut x1 = i32::MIN;
        let mut y1 = i32::MIN;
        for r in self.rects() {
            x0 = x0.min(r.x);
            y0 = y0.min(r.y);
            x1 = x1.max(r.x + r.w as i32);
            y1 = y1.max(r.y + r.h as i32);
        }
        Some(DirtyRect {
            x: x0,
            y: y0,
            w: (x1 - x0) as u32,
            h: (y1 - y0) as u32,
        })
    }
}
