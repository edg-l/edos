//! Size policy and hints for layout calculations.

/// Policy for how a widget should be sized in a layout.
#[derive(Clone, Copy, Debug, PartialEq)]
#[derive(Default)]
pub enum SizePolicy {
    /// Use an exact fixed size.
    Fixed(u32),
    /// Use the widget's preferred (natural) size.
    #[default]
    Preferred,
    /// Expand to fill available space, with relative weight.
    Expand { weight: f32 },
}


impl SizePolicy {
    /// Create an expand policy with the given weight.
    pub fn expand(weight: f32) -> Self {
        Self::Expand { weight }
    }

    /// Check if this policy should expand.
    pub fn is_expand(&self) -> bool {
        matches!(self, Self::Expand { .. })
    }

    /// Get the expand weight, or 0.0 if not expanding.
    pub fn weight(&self) -> f32 {
        match self {
            Self::Expand { weight } => *weight,
            _ => 0.0,
        }
    }
}

/// Size hints for a widget.
#[derive(Clone, Copy, Debug, Default)]
pub struct SizeHint {
    /// Minimum width the widget needs.
    pub min_width: u32,
    /// Preferred width for the widget.
    pub preferred_width: u32,
    /// Minimum height the widget needs.
    pub min_height: u32,
    /// Preferred height for the widget.
    pub preferred_height: u32,
}

impl SizeHint {
    /// Create a new size hint with all values set.
    pub fn new(
        min_width: u32,
        preferred_width: u32,
        min_height: u32,
        preferred_height: u32,
    ) -> Self {
        Self {
            min_width,
            preferred_width,
            min_height,
            preferred_height,
        }
    }

    /// Create a size hint with preferred size only (min = 0).
    pub fn preferred(width: u32, height: u32) -> Self {
        Self {
            min_width: 0,
            preferred_width: width,
            min_height: 0,
            preferred_height: height,
        }
    }

    /// Create a size hint with fixed size (min = preferred).
    pub fn fixed(width: u32, height: u32) -> Self {
        Self {
            min_width: width,
            preferred_width: width,
            min_height: height,
            preferred_height: height,
        }
    }
}

/// Size configuration for a layout item along one axis.
#[derive(Clone, Copy, Debug, Default)]
pub struct Sizing {
    /// Size policy for this axis.
    pub policy: SizePolicy,
    /// Minimum size.
    pub min: u32,
    /// Preferred size (used when policy is Preferred).
    pub preferred: u32,
}

impl Sizing {
    /// Create a new sizing with all values set.
    pub fn new(policy: SizePolicy, min: u32, preferred: u32) -> Self {
        Self {
            policy,
            min,
            preferred,
        }
    }

    /// Create a fixed sizing.
    pub fn fixed(size: u32) -> Self {
        Self {
            policy: SizePolicy::Fixed(size),
            min: size,
            preferred: size,
        }
    }

    /// Create a preferred sizing.
    pub fn preferred(size: u32) -> Self {
        Self {
            policy: SizePolicy::Preferred,
            min: 0,
            preferred: size,
        }
    }

    /// Create an expand sizing.
    pub fn expand(weight: f32) -> Self {
        Self {
            policy: SizePolicy::Expand { weight },
            min: 0,
            preferred: 0,
        }
    }
}
