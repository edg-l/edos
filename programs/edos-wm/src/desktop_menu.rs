//! The menu that opens on a right click on the desktop.
//!
//! Right-clicking the ground is the most discoverable affordance any desktop
//! has, and until now it did nothing here. The window manager owns it rather
//! than the panel, because the desktop is the thing the compositor draws.
//!
//! Drawn directly onto the screen instead of into a window: the compositor
//! already paints every frame from back to front, and a window would need
//! focus, an event queue and a lifetime, none of which a transient list of
//! four rows earns.

use edos_lib::process::spawn;
use edos_render::font::{self, Weight};
use edos_render::graphics::Screen;
use edos_render::icons;
use edos_render::text::Style;
use edos_render::theme::Theme;

/// Width of the menu.
pub const WIDTH: i64 = 200;

/// Height of one row.
const ROW_HEIGHT: i64 = 30;

/// Padding around the rows.
const PAD: i64 = 5;

/// What a row does when it is chosen.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Item {
    NewTerminal,
    Files,
    Editor,
    Widgets,
    NextBackground,
}

struct Row {
    item: Item,
    label: &'static str,
    icon: &'static icons::Mask,
}

const ROWS: &[Row] = &[
    Row {
        item: Item::NewTerminal,
        label: "New terminal",
        icon: &icons::TERMINAL,
    },
    Row {
        item: Item::Files,
        label: "Files",
        icon: &icons::FOLDER,
    },
    Row {
        item: Item::Editor,
        label: "Editor",
        icon: &icons::DOCUMENT,
    },
    Row {
        item: Item::Widgets,
        label: "Widget demo",
        icon: &icons::APPS,
    },
    Row {
        item: Item::NextBackground,
        label: "Change background",
        icon: &icons::MAXIMIZE,
    },
];

pub fn height() -> i64 {
    ROWS.len() as i64 * ROW_HEIGHT + PAD * 2
}

/// What choosing a row asks the compositor to do, when it is not something the
/// menu can do by itself.
pub enum Outcome {
    /// Nothing further; the action already ran.
    Done,
    /// Rebuild the desktop with the next background.
    NextBackground,
}

#[derive(Default)]
pub struct DesktopMenu {
    open_at: Option<(i32, i32)>,
    hovered: Option<usize>,
}

impl DesktopMenu {
    pub fn is_open(&self) -> bool {
        self.open_at.is_some()
    }

    pub fn origin(&self) -> Option<(i32, i32)> {
        self.open_at
    }

    /// Open at the pointer, nudged back on screen if it would hang off an edge.
    pub fn open(&mut self, x: i32, y: i32, screen_w: u32, screen_h: u32) {
        let ox = x.min(screen_w as i32 - WIDTH as i32).max(0);
        let oy = y.min(screen_h as i32 - height() as i32).max(0);
        self.open_at = Some((ox, oy));
        self.hovered = None;
    }

    pub fn close(&mut self) {
        self.open_at = None;
        self.hovered = None;
    }

    /// Whether the point falls inside the open menu.
    pub fn contains(&self, x: i32, y: i32) -> bool {
        let Some((ox, oy)) = self.open_at else {
            return false;
        };
        let (x, y) = (x as i64, y as i64);
        x >= ox as i64 && x < ox as i64 + WIDTH && y >= oy as i64 && y < oy as i64 + height()
    }

    pub fn hover(&mut self, x: i32, y: i32) {
        self.hovered = self.row_at(x, y);
    }

    fn row_at(&self, x: i32, y: i32) -> Option<usize> {
        if !self.contains(x, y) {
            return None;
        }
        let (_, oy) = self.open_at?;
        let local = y as i64 - oy as i64 - PAD;
        let index = (local / ROW_HEIGHT) as usize;
        (local >= 0 && index < ROWS.len()).then_some(index)
    }

    /// Act on a click. Returns `None` if the click was outside, which closes
    /// the menu without choosing anything.
    pub fn click(&mut self, x: i32, y: i32) -> Option<Outcome> {
        let row = self.row_at(x, y);
        self.close();
        let index = row?;
        match ROWS[index].item {
            Item::NewTerminal => {
                let _ = spawn("/bin/edos-terminal", &[], 0, 1, 2);
                Some(Outcome::Done)
            }
            Item::Editor => {
                let _ = spawn("/bin/edos-edit", &[], 0, 1, 2);
                None
            }
            Item::Files => {
                let _ = spawn("/bin/edos-files", &[], 0, 1, 2);
                Some(Outcome::Done)
            }
            Item::Widgets => {
                let _ = spawn("/bin/wintest", &[], 0, 1, 2);
                Some(Outcome::Done)
            }
            Item::NextBackground => Some(Outcome::NextBackground),
        }
    }

    pub fn draw(&self, screen: &mut Screen) {
        let Some((ox, oy)) = self.open_at else {
            return;
        };
        let (ox, oy) = (ox as i64, oy as i64);
        let theme = &Theme::DEFAULT;

        let _ = screen.draw_rect(
            ox as u64,
            oy as u64,
            WIDTH as u64,
            height() as u64,
            theme.background,
        );
        // A one-pixel border rather than a shadow: the menu sits on a ground
        // that is already dark, and a shadow there reads as smudge.
        for edge in 0..1i64 {
            let _ = screen.draw_rect(
                (ox + edge) as u64,
                (oy + edge) as u64,
                WIDTH as u64,
                1,
                theme.input_border,
            );
            let _ = screen.draw_rect(
                (ox + edge) as u64,
                (oy + height() - 1 - edge) as u64,
                WIDTH as u64,
                1,
                theme.input_border,
            );
            let _ = screen.draw_rect(
                (ox + edge) as u64,
                oy as u64,
                1,
                height() as u64,
                theme.input_border,
            );
            let _ = screen.draw_rect(
                (ox + WIDTH - 1 - edge) as u64,
                oy as u64,
                1,
                height() as u64,
                theme.input_border,
            );
        }

        for (index, row) in ROWS.iter().enumerate() {
            let ry = oy + PAD + index as i64 * ROW_HEIGHT;
            let is_hovered = self.hovered == Some(index);
            if is_hovered {
                let _ = screen.draw_rect(
                    (ox + PAD) as u64,
                    ry as u64,
                    (WIDTH - PAD * 2) as u64,
                    ROW_HEIGHT as u64,
                    theme.button_hover,
                );
            }
            let ink = if is_hovered {
                theme.taskbar_text_active
            } else {
                theme.text_primary
            };
            let icon_y = ry + (ROW_HEIGHT - icons::SIZE as i64) / 2;
            draw_icon(screen, ox + PAD + 8, icon_y, row.icon, ink);
            let text_y = ry + (ROW_HEIGHT - font::size::BODY as i64) / 2 - 2;
            let _ = screen.draw_styled_text(
                (ox + PAD + 8 + icons::SIZE as i64 + 10) as i32,
                text_y as i32,
                row.label,
                Style::new(ink.raw()).with_weight(Weight::Regular),
            );
        }
    }
}

fn draw_icon(
    screen: &mut Screen,
    x: i64,
    y: i64,
    mask: &icons::Mask,
    color: edos_render::graphics::Color,
) {
    for (row, bits) in mask.iter().enumerate() {
        for col in 0..icons::SIZE {
            if bits & (1 << (icons::SIZE - 1 - col)) == 0 {
                continue;
            }
            let _ = screen.draw_rect(
                (x + col as i64) as u64,
                (y + row as i64) as u64,
                1,
                1,
                color,
            );
        }
    }
}
