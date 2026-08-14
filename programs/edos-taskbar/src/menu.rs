//! The applications menu, and the power controls the shell had no way to reach.
//!
//! Owned by the panel process rather than being its own program: it is a second
//! window, so there is no IPC to invent and nothing to keep alive between
//! openings.
//!
//! It sets `FLAG_UNDECORATED` and *not* `FLAG_NO_FOCUS`, which is the whole
//! reason those two used to be one flag and are now two. A menu has no title
//! bar, and it has to take focus, because losing focus is how it closes.

use edos_lib::io::klog_dump;
use edos_lib::process::{reboot, spawn, REBOOT_HALT, REBOOT_POWER_OFF, REBOOT_RESTART};
use edos_render::icons;
use edos_render::theme::Theme;
use edos_render::widgets::{draw_rect, draw_rect_outline, draw_text, text_height, text_width};
use edos_render::window::{
    flags::FLAG_UNDECORATED, property, window_set, Window, WindowEvent, WindowEventType,
    WindowListEntry,
};

/// Width of the menu panel.
const WIDTH: u32 = 220;

/// Height of one row.
const ROW_HEIGHT: u32 = 32;

/// Padding around the rows.
const PAD: i32 = 6;

/// Height of the rule separating applications from the power controls.
const RULE: u32 = 1;

/// Gap above and below that rule.
const RULE_GAP: i32 = 6;

/// What a row does when it is chosen.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Item {
    /// A program and the arguments it is launched with. The arguments are not
    /// decoration: a viewer or a player with no file, and a terminal program
    /// with no terminal, are menu entries that do nothing when chosen.
    Launch(&'static str, &'static [&'static str]),
    PowerOff,
    Restart,
    Halt,
}

struct Row {
    item: Item,
    label: &'static str,
    icon: &'static icons::Mask,
}

/// Applications first, then the power controls, because the destructive
/// actions belong where the hand does not land by accident.
const ROWS: &[Row] = &[
    Row {
        item: Item::Launch("/bin/edos-terminal", &[]),
        label: "Terminal",
        icon: &icons::TERMINAL,
    },
    Row {
        item: Item::Launch("/bin/edos-files", &[]),
        label: "Files",
        icon: &icons::FOLDER,
    },
    Row {
        item: Item::Launch("/bin/edos-edit", &[]),
        label: "Editor",
        icon: &icons::DOCUMENT,
    },
    Row {
        item: Item::Launch("/bin/edos-web", &["https://edos.edgl.dev/"]),
        label: "Web",
        icon: &icons::LINK,
    },
    Row {
        item: Item::Launch("/bin/imgview", &["/share/wallpapers/dusk.bmp"]),
        label: "Images",
        icon: &icons::FILE,
    },
    Row {
        item: Item::Launch("/bin/play", &["/share/sounds/chime.wav"]),
        label: "Play a sound",
        icon: &icons::VOLUME,
    },
    Row {
        // Snake draws with terminal escapes, so it is launched inside one.
        item: Item::Launch("/bin/edos-terminal", &["/bin/snake"]),
        label: "Snake",
        icon: &icons::APPS,
    },
    Row {
        item: Item::Launch("/bin/edos-grab", &[]),
        label: "Packages",
        icon: &icons::DISK,
    },
    Row {
        item: Item::Launch("/bin/wintest", &[]),
        label: "Widgets",
        icon: &icons::APPS,
    },
    Row {
        item: Item::PowerOff,
        label: "Shut down",
        icon: &icons::POWER,
    },
    Row {
        item: Item::Restart,
        label: "Restart",
        icon: &icons::POWER,
    },
    Row {
        item: Item::Halt,
        label: "Halt",
        icon: &icons::POWER,
    },
];

/// Index of the first power row, which is where the rule goes.
const FIRST_POWER_ROW: usize = 9;

/// Prefix on every line of a menu dump, so a host-side reader can pick the
/// block out of an interleaved serial log.
const MENU_DUMP_TAG: &str = "menu|";

/// Header starting each block, which is also how a reader tells where one
/// block ends and the next begins: it is the only line whose first field is
/// not a number.
const MENU_DUMP_HEADER: &str = "X Y W H LABEL";

/// Publish where each row sits on screen, once per opening.
///
/// The menu exists only while it is open and its rows are not windows, so this
/// is the only way something driving the machine from outside can choose one
/// by its label instead of by a pixel copied out of this file.
fn dump_rows(window_x: i32, window_y: i32) {
    let lines = ROWS.iter().enumerate().map(|(index, row)| {
        format!(
            "{} {} {} {} {}",
            window_x + PAD,
            window_y + row_y(index),
            WIDTH - PAD as u32 * 2,
            ROW_HEIGHT,
            row.label
        )
    });
    klog_dump(
        MENU_DUMP_TAG,
        core::iter::once(MENU_DUMP_HEADER.to_string()).chain(lines),
    );
}

fn height() -> u32 {
    ROWS.len() as u32 * ROW_HEIGHT + (PAD * 2) as u32 + RULE + (RULE_GAP * 2) as u32
}

/// Vertical offset of a row, allowing for the rule above the power group.
fn row_y(index: usize) -> i32 {
    let mut y = PAD + index as i32 * ROW_HEIGHT as i32;
    if index >= FIRST_POWER_ROW {
        y += RULE_GAP * 2 + RULE as i32;
    }
    y
}

pub struct Menu {
    window: Option<Window>,
    hovered: Option<usize>,
    /// Set when the menu opened this frame, so the click that opened it is not
    /// also read as the click that closes it.
    just_opened: bool,
}

impl Menu {
    pub fn new() -> Self {
        Self {
            window: None,
            hovered: None,
            just_opened: false,
        }
    }

    pub fn is_open(&self) -> bool {
        self.window.is_some()
    }

    pub fn window_id(&self) -> Option<u64> {
        self.window.as_ref().map(|w| w.id)
    }

    /// Open the menu above the panel, or close it if it is already open.
    pub fn toggle(&mut self, anchor_x: i32, panel_y: i32) {
        if self.window.is_some() {
            self.close();
            return;
        }
        let y = panel_y - height() as i32;
        let mut window = match Window::new(anchor_x, y, WIDTH, height()) {
            Ok(w) => w,
            Err(e) => {
                eprintln!("[panel] could not open the menu: {e:?}");
                return;
            }
        };
        // Undecorated, but focusable: a menu with no focus cannot know when to
        // close, and one with a title bar is not a menu.
        if let Err(e) = window_set(window.id, property::FLAGS, FLAG_UNDECORATED) {
            eprintln!("[panel] could not set menu flags: {e:?}");
        }
        let _ = window.set_title("Applications");
        if window.show().is_err() {
            return;
        }
        dump_rows(anchor_x, y);
        self.window = Some(window);
        self.hovered = None;
        self.just_opened = true;
    }

    pub fn close(&mut self) {
        self.window = None;
        self.hovered = None;
    }

    /// Poll the menu's own events and repaint it.
    ///
    /// `windows` is the panel's window list, used only to notice that the menu
    /// was destroyed from underneath us.
    pub fn tick(&mut self, windows: &[WindowListEntry]) {
        let Some(window) = self.window.as_mut() else {
            return;
        };
        // The list was taken before this menu existed on the frame it opened,
        // so its absence there means "not listed yet", not "destroyed".
        if !self.just_opened && !windows.iter().any(|w| w.id == window.id) {
            self.close();
            return;
        }

        let mut events = [WindowEvent::default(); 16];
        let mut chosen: Option<Item> = None;
        let mut dismiss = false;

        if let Ok(count) = window.poll_events(&mut events) {
            for event in &events[..count] {
                match event.event_type() {
                    Some(WindowEventType::CloseRequested) => dismiss = true,
                    Some(WindowEventType::FocusLost) => {
                        // Clicking anywhere else closes the menu, which is the
                        // behaviour every menu has and the reason it needs focus.
                        if !self.just_opened {
                            dismiss = true;
                        }
                    }
                    Some(WindowEventType::MouseMove) => {
                        self.hovered = row_at(event.y);
                    }
                    Some(WindowEventType::MouseButton) if event.data == 0 => {
                        if let Some(index) = row_at(event.y) {
                            chosen = Some(ROWS[index].item);
                        }
                    }
                    Some(WindowEventType::KeyPress) if event.code == 1 => dismiss = true,
                    _ => {}
                }
            }
        }
        self.just_opened = false;

        let w = window.width;
        let h = window.height;
        if let Some(buf) = window.buffer_mut() {
            draw_rect(buf, w, h, 0, 0, w, h, Theme::DEFAULT.background.raw());
            draw_rect_outline(buf, w, h, 0, 0, w, h, Theme::DEFAULT.input_border.raw());

            for (index, row) in ROWS.iter().enumerate() {
                let y = row_y(index);
                let is_hovered = self.hovered == Some(index);
                if is_hovered {
                    draw_rect(
                        buf,
                        w,
                        h,
                        PAD,
                        y,
                        w - PAD as u32 * 2,
                        ROW_HEIGHT,
                        Theme::DEFAULT.button_hover.raw(),
                    );
                }
                let ink = if is_hovered {
                    Theme::DEFAULT.taskbar_text_active
                } else {
                    Theme::DEFAULT.text_primary
                };
                let icon_y = y + (ROW_HEIGHT as i32 - icons::SIZE as i32) / 2;
                icons::draw(buf, w, h, PAD + 10, icon_y, row.icon, ink.raw());
                let text_y = y + (ROW_HEIGHT as i32 - text_height() as i32) / 2;
                draw_text(
                    buf,
                    w,
                    h,
                    PAD + 10 + icons::SIZE as i32 + 10,
                    text_y,
                    row.label,
                    ink.raw(),
                );
                let _ = text_width(row.label);
            }

            let rule_y = row_y(FIRST_POWER_ROW) - RULE_GAP - RULE as i32;
            draw_rect(
                buf,
                w,
                h,
                PAD,
                rule_y,
                w - PAD as u32 * 2,
                RULE,
                Theme::DEFAULT.input_border.raw(),
            );
        }
        window.swap_buffers();

        if let Some(item) = chosen {
            // The window goes away before the action runs, so a shutdown does
            // not leave a menu painted over the last frame.
            self.close();
            match item {
                Item::Launch(path, args) => {
                    let _ = spawn(path, args, 0, 1, 2);
                }
                Item::PowerOff => {
                    let _ = reboot(REBOOT_POWER_OFF);
                }
                Item::Restart => {
                    let _ = reboot(REBOOT_RESTART);
                }
                Item::Halt => {
                    let _ = reboot(REBOOT_HALT);
                }
            }
            return;
        }
        if dismiss {
            self.close();
        }
    }
}

/// Which row a y coordinate falls in, if any.
fn row_at(y: i32) -> Option<usize> {
    (0..ROWS.len()).find(|&index| {
        let top = row_y(index);
        y >= top && y < top + ROW_HEIGHT as i32
    })
}
