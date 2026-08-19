//! Dirty rectangle tracking for partial framebuffer updates.

/// Maximum number of dirty rects before collapsing to full-screen.
const MAX_RECTS: usize = 16;

/// How much larger a union may be than the two rects apart before it is not
/// worth merging them, as a percentage.
const MERGE_SLACK_PERCENT: u64 = 150;

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

    pub fn area(self) -> u64 {
        self.w as u64 * self.h as u64
    }

    /// The smallest rectangle containing both.
    pub fn union(self, other: Self) -> Self {
        let x0 = self.x.min(other.x);
        let y0 = self.y.min(other.y);
        let x1 = (self.x + self.w as i32).max(other.x + other.w as i32);
        let y1 = (self.y + self.h as i32).max(other.y + other.h as i32);
        Self {
            x: x0,
            y: y0,
            w: (x1 - x0) as u32,
            h: (y1 - y0) as u32,
        }
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

    /// The dirty rects with cheap merges applied.
    ///
    /// Sending one rectangle per region is not automatically cheaper than
    /// sending their union: two overlapping rects pay for the overlap twice,
    /// and each transfer is its own ioctl. But merging *everything* into one
    /// bounding box is far worse in the common case — a cursor near one corner
    /// and a clock in the other span the whole screen between them, which is
    /// how a still desktop came to transfer 86% of itself every frame.
    ///
    /// So a pair is merged only when the union costs little more than the two
    /// apart. Areas are summed without subtracting the overlap, which biases
    /// very slightly toward merging, and toward fewer ioctls.
    #[allow(
        clippy::mut_range_bound,
        reason = "every `n -= 1` is followed at once by `break 'pairs`, so the                   ranges the loops already captured are never read again; the                   outer `loop` re-enters with the new `n`"
    )]
    pub fn coalesced(&self) -> ([DirtyRect; MAX_RECTS], usize) {
        let mut out = self.rects;
        let mut n = self.count;

        loop {
            let mut merged = false;
            'pairs: for i in 0..n {
                for j in (i + 1)..n {
                    let union = out[i].union(out[j]);
                    if union.area() * 100 <= (out[i].area() + out[j].area()) * MERGE_SLACK_PERCENT {
                        out[i] = union;
                        out[j] = out[n - 1];
                        n -= 1;
                        merged = true;
                        break 'pairs;
                    }
                }
            }
            if !merged {
                break;
            }
        }

        (out, n)
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
