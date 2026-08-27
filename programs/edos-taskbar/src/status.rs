//! The popups behind the panel's status icons.
//!
//! Both are the same window: undecorated, focusable, anchored above the panel,
//! dismissed by losing focus. That is the menu's shape too, and the reason it
//! is repeated here rather than shared is that a menu is a list of actions and
//! these are a control and a report; what they have in common is the window,
//! which `Popup` owns.

use std::fs;

use edos_lib::io as eio;
use edos_render::metrics::{CONTROL_HEIGHT, space};
use edos_render::surface::Surface;
use edos_render::text::Style;
use edos_render::theme::Theme;
use edos_render::widgets::{Slider, Widget, WidgetEvent, text_height};
use edos_render::window::{
    Window, WindowEvent, WindowEventType, WindowListEntry, flags::FLAG_UNDECORATED, property,
    window_set,
};

/// `/dev/dsp` ioctls, mirroring `kernel/src/drivers/hda/mod.rs`.
const AUDIO_IOCTL_SET_VOLUME: u64 = 4;
const AUDIO_IOCTL_GET_VOLUME: u64 = 5;

/// Padding inside a popup.
const PAD: i32 = 10;

/// Width of both popups. The volume slider wants room to be draggable and the
/// network report wants room for an address, and one width means the two
/// popups do not jump about as the user moves between them.
const WIDTH: u32 = 260;

/// Which status control a popup belongs to.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Volume,
    Network,
}

/// The window shared by both popups, and the bookkeeping that dismisses it.
struct Popup {
    window: Window,
    kind: Kind,
    /// Set on the frame it opened, so the click that opened it is not also read
    /// as the click that closes it.
    just_opened: bool,
}

pub struct StatusPopups {
    popup: Option<Popup>,
    volume: Slider,
    /// Volume the codec is set to, so a failed write is visible rather than
    /// leaving the slider showing a position the hardware never took.
    volume_available: bool,
    net: Vec<(String, String)>,
}

impl StatusPopups {
    pub fn new() -> Self {
        let level = read_volume();
        Self {
            popup: None,
            volume: Slider::with_value(
                PAD,
                PAD + text_height() as i32 + space(2) as i32,
                WIDTH - PAD as u32 * 2,
                0,
                100,
                level.unwrap_or(0) as i32,
            ),
            volume_available: level.is_some(),
            net: Vec::new(),
        }
    }

    pub fn open_kind(&self) -> Option<Kind> {
        self.popup.as_ref().map(|p| p.kind)
    }

    pub fn window_id(&self) -> Option<u64> {
        self.popup.as_ref().map(|p| p.window.id)
    }

    /// Open the popup for `kind` above the panel, right-aligned on the control
    /// that owns it, or close it if it is already the one showing.
    pub fn toggle(&mut self, kind: Kind, anchor_x: i32, anchor_width: u32, panel_y: i32) {
        if self.open_kind() == Some(kind) {
            self.close();
            return;
        }
        self.close();

        if kind == Kind::Volume {
            if let Some(level) = read_volume() {
                self.volume.set_value(level as i32);
                self.volume_available = true;
            } else {
                self.volume_available = false;
            }
        } else {
            self.net = read_net();
        }

        let height = self.height(kind);
        // Right-aligned on its control, then pulled back inside the screen: a
        // popup anchored at the clock's left edge would hang off the display.
        let x = (anchor_x + anchor_width as i32 - WIDTH as i32).max(0);
        let mut window = match Window::new(x, panel_y - height as i32, WIDTH, height) {
            Ok(w) => w,
            Err(e) => {
                eprintln!("[panel] could not open the status popup: {e:?}");
                return;
            }
        };
        if let Err(e) = window_set(window.id, property::FLAGS, FLAG_UNDECORATED) {
            eprintln!("[panel] could not set popup flags: {e:?}");
        }
        let _ = window.set_title(match kind {
            Kind::Volume => "Volume",
            Kind::Network => "Network",
        });
        if window.show().is_err() {
            return;
        }
        self.popup = Some(Popup {
            window,
            kind,
            just_opened: true,
        });
    }

    pub fn close(&mut self) {
        self.popup = None;
    }

    fn height(&self, kind: Kind) -> u32 {
        let title = text_height() + space(2);
        match kind {
            Kind::Volume => PAD as u32 * 2 + title + CONTROL_HEIGHT + space(2) + text_height(),
            Kind::Network => {
                let rows = self.net.len().max(1) as u32;
                PAD as u32 * 2 + title + rows * (text_height() + space(1))
            }
        }
    }

    /// Poll the popup's own events and repaint it.
    ///
    /// `windows` is the panel's window list, used only to notice that the popup
    /// was destroyed from underneath us.
    pub fn tick(&mut self, windows: &[WindowListEntry]) {
        let Some(popup) = self.popup.as_mut() else {
            return;
        };
        // The list was taken before this window existed on the frame it opened,
        // so its absence there means "not listed yet", not "destroyed".
        if !popup.just_opened && !windows.iter().any(|w| w.id == popup.window.id) {
            self.close();
            return;
        }

        let mut events = [WindowEvent::default(); 16];
        let mut dismiss = false;
        let mut new_volume = None;

        if let Ok(count) = popup.window.poll_events(&mut events) {
            for event in &events[..count] {
                match event.event_type() {
                    Some(WindowEventType::CloseRequested) => dismiss = true,
                    Some(WindowEventType::FocusLost) => {
                        if !popup.just_opened {
                            dismiss = true;
                        }
                    }
                    Some(WindowEventType::KeyPress) if event.code == 1 => dismiss = true,
                    Some(WindowEventType::MouseMove) if popup.kind == Kind::Volume => {
                        // `on_mouse_move` moves the thumb while dragging but
                        // reports no event, so the value it left behind is what
                        // says whether anything happened.
                        let before = self.volume.value();
                        self.volume.on_mouse_move(event.x, event.y);
                        if self.volume.value() != before {
                            new_volume = Some(self.volume.value().clamp(0, 100) as u8);
                        }
                    }
                    Some(WindowEventType::MouseButton) if popup.kind == Kind::Volume => {
                        let pressed = event.data != 0;
                        if let Some(WidgetEvent::ValueChanged(level)) =
                            self.volume.on_mouse_button(event.x, event.y, pressed)
                        {
                            new_volume = Some(level.clamp(0, 100) as u8);
                        }
                    }
                    _ => {}
                }
            }
        }
        popup.just_opened = false;

        if let Some(level) = new_volume {
            self.volume_available = write_volume(level);
        }

        let kind = popup.kind;
        let w = popup.window.width;
        let h = popup.window.height;
        if let Some(buf) = popup.window.buffer_mut() {
            let surface = &mut Surface::new(buf, w, h);
            surface.rect(0, 0, w, h, Theme::DEFAULT.background.raw());
            surface.rect_outline(0, 0, w, h, Theme::DEFAULT.input_border.raw());

            let title = match kind {
                Kind::Volume => "Volume",
                Kind::Network => "Network",
            };
            surface.text(
                PAD,
                PAD,
                title,
                Style::new(Theme::DEFAULT.text_placeholder.raw()),
            );

            match kind {
                Kind::Volume => {
                    self.volume.draw(surface);
                    let label = if self.volume_available {
                        format!("{}%", self.volume.value())
                    } else {
                        String::from("no audio device")
                    };
                    surface.text(
                        PAD,
                        PAD + text_height() as i32
                            + space(2) as i32
                            + CONTROL_HEIGHT as i32
                            + space(2) as i32,
                        &label,
                        Style::new(Theme::DEFAULT.text_primary.raw()),
                    );
                }
                Kind::Network => {
                    let mut y = PAD + text_height() as i32 + space(2) as i32;
                    if self.net.is_empty() {
                        surface.text(
                            PAD,
                            y,
                            "no interface",
                            Style::new(Theme::DEFAULT.text_placeholder.raw()),
                        );
                    }
                    for (key, value) in &self.net {
                        surface.text(
                            PAD,
                            y,
                            key,
                            Style::new(Theme::DEFAULT.text_placeholder.raw()),
                        );
                        surface.text(
                            PAD + 80,
                            y,
                            value,
                            Style::new(Theme::DEFAULT.text_primary.raw()),
                        );
                        y += text_height() as i32 + space(1) as i32;
                    }
                }
            }
        }
        popup.window.swap_buffers();

        if dismiss {
            self.close();
        }
    }
}

/// Open `/dev/dsp` for the volume ioctls.
///
/// Reopened per call rather than held: the HDA driver registers `/dev/dsp`
/// asynchronously, so a panel that opened it once at start would never see an
/// audio device that arrived a moment later.
fn dsp() -> Option<u64> {
    eio::open("/dev/dsp", 0).ok()
}

fn read_volume() -> Option<u8> {
    let fd = dsp()?;
    let level = eio::ioctl(fd, AUDIO_IOCTL_GET_VOLUME, 0);
    let _ = eio::close(fd);
    Some(level.ok()?.min(100) as u8)
}

fn write_volume(level: u8) -> bool {
    let Some(fd) = dsp() else {
        return false;
    };
    let result = eio::ioctl(fd, AUDIO_IOCTL_SET_VOLUME, level as u64);
    let _ = eio::close(fd);
    result.is_ok()
}

/// The interface fields worth showing, in the order they are worth reading.
///
/// `/proc/net` rather than `SYS_NETINFO`: that syscall renders the same state
/// for a terminal, ANSI colour codes and all, which is not something to parse.
fn read_net() -> Vec<(String, String)> {
    const SHOWN: &[(&str, &str)] = &[
        ("interface", "Interface"),
        ("link", "Link"),
        ("inet", "Address"),
        ("gateway", "Gateway"),
        ("dns", "DNS"),
        ("mac", "MAC"),
    ];

    let Ok(text) = fs::read_to_string("/proc/net") else {
        return Vec::new();
    };
    // One blank-line-separated block per interface, loopback first. The last
    // block is the real one, and a machine with no network stack has a last
    // block naming no interface at all.
    let Some(block) = text.split("\n\n").filter(|b| !b.trim().is_empty()).last() else {
        return Vec::new();
    };
    let fields: Vec<(&str, &str)> = block
        .lines()
        .filter_map(|line| line.split_once(": "))
        .collect();
    let field = |name: &str| fields.iter().find(|(k, _)| *k == name).map(|(_, v)| *v);
    if field("interface") == Some("none") {
        return Vec::new();
    }
    let prefix = field("prefix").unwrap_or_default();

    SHOWN
        .iter()
        .filter_map(|(key, label)| {
            let value = field(key)?;
            let value = if *key == "inet" && !prefix.is_empty() {
                format!("{value}/{prefix}")
            } else {
                value.to_string()
            };
            Some((label.to_string(), value))
        })
        .collect()
}
