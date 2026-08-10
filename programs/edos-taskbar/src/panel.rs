//! Panel geometry: where every region and every button sits.
//!
//! Laid out here rather than inline in the draw loop because hit testing and
//! drawing have to agree exactly, and two copies of the same arithmetic is how
//! a button ends up an inch from the thing it activates.

use edos_render::icons;
use edos_render::widgets::text_width;
use edos_render::window::WindowListEntry;

/// Panel height. Deep enough for a 16px icon beside a line of interface text
/// with room to breathe, rather than the text cell alone.
pub const HEIGHT: u32 = 40;

/// Height of the buttons inside the panel.
pub const BUTTON_HEIGHT: u32 = 28;

/// Height of the accent underline marking the focused window.
pub const ACCENT_HEIGHT: u32 = 2;

/// Gap between adjacent task buttons.
pub const BUTTON_GAP: i32 = 4;

/// Padding inside a button, from its edge to the icon or label.
pub const BUTTON_PAD: u32 = 10;

/// Gap between a button's icon and its label.
pub const ICON_GAP: u32 = 7;

/// Bounds a task button so a long title cannot swallow the panel, and a short
/// one still reads as a button rather than a word.
pub const TASK_MIN_WIDTH: u32 = 96;
pub const TASK_MAX_WIDTH: u32 = 200;

/// Distance from the panel's edges to the first and last element.
pub const EDGE_PAD: i32 = 8;

/// Gap between the status area's controls.
pub const STATUS_GAP: i32 = 4;

/// A rectangle inside the panel, and what activates it.
#[derive(Clone, Copy, Debug)]
pub struct Hit {
    pub x: i32,
    pub width: u32,
    pub action: Action,
}

/// What clicking a region of the panel does.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    /// Open the applications menu.
    Launcher,
    /// Focus a window, or put it away if it already has focus.
    Task(u64),
    /// Open the volume control.
    Volume,
    /// Open the network status.
    Network,
    /// Open the clock.
    Clock,
}

impl Hit {
    pub fn contains(&self, x: i32) -> bool {
        x >= self.x && x < self.x + self.width as i32
    }
}

/// Vertical position of a button, centred in the panel.
pub fn button_y() -> i32 {
    (HEIGHT as i32 - BUTTON_HEIGHT as i32) / 2
}

/// Width of a task button holding `label`, clamped so the row stays readable.
pub fn task_width(label: &str) -> u32 {
    let content = icons::SIZE as u32 + ICON_GAP + text_width(label) + BUTTON_PAD * 2;
    content.clamp(TASK_MIN_WIDTH, TASK_MAX_WIDTH)
}

/// Width of a status button holding only an icon, or an icon and a label.
pub fn status_width(label: &str) -> u32 {
    if label.is_empty() {
        icons::SIZE as u32 + BUTTON_PAD * 2
    } else {
        icons::SIZE as u32 + ICON_GAP + text_width(label) + BUTTON_PAD * 2
    }
}

/// Everything clickable in the panel, in one pass.
///
/// The task list is centred on the panel and squeezed between the launcher and
/// the status area, so a window opening never pushes the clock off the edge.
pub struct Layout {
    pub hits: Vec<Hit>,
    /// Where the task list actually starts, after centring and clamping.
    pub tasks_x: i32,
}

pub fn compute(panel_width: u32, tasks: &[(&WindowListEntry, String)], clock: &str) -> Layout {
    let mut hits = Vec::new();

    // Left: the launcher, which is one icon and no label.
    let launcher_w = status_width("");
    hits.push(Hit {
        x: EDGE_PAD,
        width: launcher_w,
        action: Action::Launcher,
    });
    let left_edge = EDGE_PAD + launcher_w as i32 + EDGE_PAD;

    // Right: the status area, laid out from the right edge inwards so the
    // clock's own width cannot shift what is beside it.
    let clock_w = status_width(clock);
    let net_w = status_width("");
    let vol_w = status_width("");
    let mut x = panel_width as i32 - EDGE_PAD - clock_w as i32;
    let clock_x = x;
    x -= STATUS_GAP + net_w as i32;
    let net_x = x;
    x -= STATUS_GAP + vol_w as i32;
    let vol_x = x;
    let right_edge = vol_x - EDGE_PAD;

    // Centre: the task list, centred on the panel, then pushed right if it
    // would overlap the launcher and clamped if it would reach the status area.
    let total: u32 = tasks
        .iter()
        .map(|(_, label)| task_width(label))
        .sum::<u32>()
        + tasks.len().saturating_sub(1) as u32 * BUTTON_GAP as u32;
    let centred = (panel_width as i32 - total as i32) / 2;
    let tasks_x = centred.max(left_edge);

    let mut tx = tasks_x;
    for (entry, label) in tasks {
        let w = task_width(label);
        if tx + w as i32 > right_edge {
            break;
        }
        hits.push(Hit {
            x: tx,
            width: w,
            action: Action::Task(entry.id),
        });
        tx += w as i32 + BUTTON_GAP;
    }

    hits.push(Hit {
        x: vol_x,
        width: vol_w,
        action: Action::Volume,
    });
    hits.push(Hit {
        x: net_x,
        width: net_w,
        action: Action::Network,
    });
    hits.push(Hit {
        x: clock_x,
        width: clock_w,
        action: Action::Clock,
    });

    Layout { hits, tasks_x }
}
