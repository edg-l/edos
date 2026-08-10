//! Spacing and sizing scale for the EDOS GUI.
//!
//! Every widget dimension is derived from one dial, [`UNIT`], rather than
//! hand-picked per widget. `UNIT` is a quarter of the font cell height, so the
//! scale and the text baseline stay commensurate: nothing lands on a half pixel
//! and a control is always a whole number of cells tall.

/// Height of one text cell in the default font.
pub const TEXT_CELL_HEIGHT: u32 = 16;

/// The base spacing unit. Every other metric here is a multiple of it.
pub const UNIT: u32 = TEXT_CELL_HEIGHT / 4;

/// `n` steps on the spacing scale.
pub const fn space(n: u32) -> u32 {
    UNIT * n
}

/// Height shared by every interactive control, so a row of mixed widgets sits
/// on one baseline: a text cell with one step of breathing room above and below.
pub const CONTROL_HEIGHT: u32 = TEXT_CELL_HEIGHT + space(2) * 2;

/// Horizontal padding between a control's edge and its label.
pub const CONTROL_PAD_X: u32 = space(4);

/// Inner padding of a text field, from the border to the first glyph.
pub const TEXT_PAD_X: u32 = space(2);

/// Gap between the edge of a control and its focus ring.
pub const FOCUS_RING_GAP: u32 = UNIT / 2;

/// Gap between a checkbox or radio box and its label.
pub const LABEL_GAP: u32 = space(2);

/// Side of a checkbox box: one text cell, so it reads as the same weight as
/// the label beside it.
pub const CHECKBOX_BOX: u32 = TEXT_CELL_HEIGHT;

/// Inset of the check mark inside the checkbox box.
pub const CHECKBOX_INSET: u32 = UNIT;

/// Thickness of a slider track.
pub const SLIDER_TRACK_HEIGHT: u32 = UNIT;

/// Width of a slider thumb.
pub const SLIDER_THUMB_WIDTH: u32 = space(3);

/// Height of a slider thumb. Shorter than the control it sits in, so the thumb
/// reads as an object on the track rather than a break in it.
pub const SLIDER_THUMB_HEIGHT: u32 = space(5);
