//! Where everything sits, and how it is drawn.
//!
//! The geometry is pure functions the event loop calls too, so what the
//! pointer hits and what the eye sees come from one description of the window
//! rather than from two that drift.

use edos_render::font::Weight;
use edos_render::icons;
use edos_render::metrics::{CONTROL_HEIGHT, space};
use edos_render::text::Style;
use edos_render::theme::Theme;
use edos_render::widgets::{
    Rect, draw_rect, draw_rect_outline, draw_text_styled, text_height, text_width,
};

/// Margin from a panel's edge to its contents.
pub const PAD: u32 = space(3);
/// Height of one package row: an icon with breathing room above and below.
pub const ROW_H: u32 = space(12);
/// Side of a package icon, square.
pub const ICON: u32 = space(8);
/// Height of the progress strip along the foot.
pub const STATUS_H: u32 = space(7);
/// Width of an action button.
pub const BUTTON_W: u32 = space(22);
/// Share of the window the package list takes, in percent.
const LIST_PERCENT: u32 = 55;
/// What stands in for the part of a string that did not fit.
const ELLIPSIS: &str = "…";

/// A window divided into its panels. Derived state: rebuilt whenever it is
/// needed rather than kept, so a resize cannot leave a click landing on a
/// rectangle that has moved.
pub struct Layout {
    pub search: Rect,
    pub refresh: Rect,
    pub list: Rect,
    pub detail: Rect,
    pub install: Rect,
    pub remove: Rect,
    pub status: Rect,
}

impl Layout {
    pub fn new(width: u32, height: u32) -> Self {
        let inner_w = width.saturating_sub(PAD * 3).max(2);
        let list_w = (inner_w * LIST_PERCENT / 100).max(1);
        let detail_w = inner_w.saturating_sub(list_w).max(1);

        let header_y = PAD as i32;
        let refresh_x = (PAD + list_w).saturating_sub(BUTTON_W) as i32;
        let search_w = list_w.saturating_sub(BUTTON_W + space(2)).max(1);

        let body_y = header_y + CONTROL_HEIGHT as i32 + PAD as i32;
        let body_h = height
            .saturating_sub(body_y as u32 + STATUS_H + PAD)
            .max(ROW_H);

        let detail_x = (PAD * 2 + list_w) as i32;
        let buttons_y = body_y + body_h as i32 - (PAD + CONTROL_HEIGHT) as i32;

        Self {
            search: Rect::new(PAD as i32, header_y, search_w, CONTROL_HEIGHT),
            refresh: Rect::new(refresh_x, header_y, BUTTON_W, CONTROL_HEIGHT),
            list: Rect::new(PAD as i32, body_y, list_w, body_h),
            detail: Rect::new(detail_x, body_y, detail_w, body_h),
            install: Rect::new(
                detail_x + PAD as i32,
                buttons_y,
                BUTTON_W.min(detail_w.saturating_sub(PAD * 2).max(1)),
                CONTROL_HEIGHT,
            ),
            remove: Rect::new(
                detail_x + (PAD + BUTTON_W + space(2)) as i32,
                buttons_y,
                BUTTON_W.min(detail_w.saturating_sub(PAD * 2).max(1)),
                CONTROL_HEIGHT,
            ),
            status: Rect::new(0, height.saturating_sub(STATUS_H) as i32, width, STATUS_H),
        }
    }

    /// How many rows the list shows at once.
    pub fn visible_rows(&self) -> usize {
        (self.list.height / ROW_H) as usize
    }

    /// The rectangle of the `slot`-th row currently on screen.
    pub fn row(&self, slot: usize) -> Rect {
        Rect::new(
            self.list.x,
            self.list.y + (slot as u32 * ROW_H) as i32,
            self.list.width,
            ROW_H,
        )
    }

    /// Which visible slot a pointer at `y` is over, if any.
    pub fn row_at(&self, x: i32, y: i32) -> Option<usize> {
        if !self.list.contains(x, y) {
            return None;
        }
        let slot = ((y - self.list.y) as u32 / ROW_H) as usize;
        (slot < self.visible_rows()).then_some(slot)
    }
}

/// A pixel buffer with the drawing this program does hung off it.
pub struct Canvas<'a> {
    pub buf: &'a mut [u32],
    pub width: u32,
    pub height: u32,
}

impl Canvas<'_> {
    pub fn fill(&mut self, rect: Rect, color: u32) {
        draw_rect(
            self.buf,
            self.width,
            self.height,
            rect.x,
            rect.y,
            rect.width,
            rect.height,
            color,
        );
    }

    pub fn outline(&mut self, rect: Rect, color: u32) {
        draw_rect_outline(
            self.buf,
            self.width,
            self.height,
            rect.x,
            rect.y,
            rect.width,
            rect.height,
            color,
        );
    }

    pub fn text(&mut self, x: i32, y: i32, text: &str, style: Style) {
        draw_text_styled(self.buf, self.width, self.height, x, y, text, style);
    }

    /// Draw `pixels`, a `w` x `h` opaque image, with its top-left at (`x`, `y`).
    pub fn blit(&mut self, x: i32, y: i32, w: u32, h: u32, pixels: &[u32]) {
        for row in 0..h {
            let py = y + row as i32;
            if py < 0 || py >= self.height as i32 {
                continue;
            }
            for col in 0..w {
                let px = x + col as i32;
                if px < 0 || px >= self.width as i32 {
                    continue;
                }
                let src = (row * w + col) as usize;
                let dst = (py as u32 * self.width + px as u32) as usize;
                if let (Some(&value), Some(slot)) = (pixels.get(src), self.buf.get_mut(dst)) {
                    *slot = value;
                }
            }
        }
    }
}

/// `text` cut to `limit` pixels, with an ellipsis where it was cut.
pub fn elide(text: &str, limit: u32) -> String {
    if text_width(text) <= limit {
        return text.to_string();
    }
    let room = limit.saturating_sub(text_width(ELLIPSIS));
    let mut out = String::new();
    for ch in text.chars() {
        let mut candidate = out.clone();
        candidate.push(ch);
        if text_width(&candidate) > room {
            break;
        }
        out = candidate;
    }
    out.push_str(ELLIPSIS);
    out
}

/// Break `text` into lines no wider than `limit` pixels, on word boundaries.
pub fn wrap(text: &str, limit: u32) -> Vec<String> {
    let mut lines = Vec::new();
    let mut line = String::new();
    for word in text.split_whitespace() {
        let candidate = if line.is_empty() {
            word.to_string()
        } else {
            format!("{} {}", line, word)
        };
        if text_width(&candidate) > limit && !line.is_empty() {
            lines.push(std::mem::take(&mut line));
            line = word.to_string();
        } else {
            line = candidate;
        }
    }
    if !line.is_empty() {
        lines.push(line);
    }
    lines
}

/// Vertical offset that centres one line of text in a `height`-tall box.
fn centre(height: u32) -> i32 {
    (height.saturating_sub(text_height()) / 2) as i32
}

/// Draw a button, and say nothing about whether it was hit — that is the
/// layout's job, from the same rectangle.
pub fn draw_button(canvas: &mut Canvas<'_>, rect: Rect, label: &str, enabled: bool, hover: bool) {
    let theme = Theme::DEFAULT;
    let background = match (enabled, hover) {
        (false, _) => theme.control_disabled,
        (true, true) => theme.button_hover,
        (true, false) => theme.button_normal,
    };
    canvas.fill(rect, background.raw());
    canvas.outline(rect, theme.input_border.raw());

    let colour = if enabled {
        theme.text_primary
    } else {
        theme.text_disabled
    };
    let x = rect.x + (rect.width.saturating_sub(text_width(label)) / 2) as i32;
    canvas.text(
        x,
        rect.y + centre(rect.height),
        label,
        Style::new(colour.raw()),
    );
}

/// One package row: icon, name, version, and the summary under them.
#[allow(clippy::too_many_arguments)]
pub fn draw_row(
    canvas: &mut Canvas<'_>,
    rect: Rect,
    name: &str,
    version: &str,
    summary: &str,
    installed: bool,
    icon: Option<(u32, u32, &[u32])>,
    selected: bool,
    hover: bool,
) {
    let theme = Theme::DEFAULT;
    if selected {
        canvas.fill(rect, theme.list_selected.raw());
    } else if hover {
        canvas.fill(rect, theme.button_hover.raw());
    }

    let icon_x = rect.x + PAD as i32;
    let icon_y = rect.y + (rect.height.saturating_sub(ICON) / 2) as i32;
    match icon {
        Some((w, h, pixels)) => {
            let x = icon_x + (ICON.saturating_sub(w) / 2) as i32;
            let y = icon_y + (ICON.saturating_sub(h) / 2) as i32;
            canvas.blit(x, y, w, h, pixels);
        }
        None => icons::draw(
            canvas.buf,
            canvas.width,
            canvas.height,
            icon_x + (ICON.saturating_sub(icons::SIZE as u32) / 2) as i32,
            icon_y + (ICON.saturating_sub(icons::SIZE as u32) / 2) as i32,
            &icons::APPS,
            theme.text_placeholder.raw(),
        ),
    }

    let text_x = icon_x + ICON as i32 + PAD as i32;
    let right = rect.x + rect.width as i32 - PAD as i32;
    let name_style = Style::new(theme.text_primary.raw()).with_weight(Weight::Semibold);
    canvas.text(text_x, rect.y + space(2) as i32, name, name_style);

    let mut mark_x = right;
    if installed {
        let mark = "installed";
        mark_x = right - text_width(mark) as i32;
        canvas.text(
            mark_x,
            rect.y + space(2) as i32,
            mark,
            Style::new(theme.entry_dir.raw()),
        );
        mark_x -= space(2) as i32;
    }

    let version_x = text_x + text_width(name) as i32 + space(2) as i32;
    if version_x < mark_x {
        canvas.text(
            version_x,
            rect.y + space(2) as i32,
            version,
            Style::new(theme.text_placeholder.raw()),
        );
    }

    let room = (right - text_x).max(0) as u32;
    canvas.text(
        text_x,
        rect.y + (space(2) + text_height() + space(1)) as i32,
        &elide(summary, room),
        Style::new(theme.label_text.raw()),
    );
}

/// The detail pane for the selected package, or the invitation to select one.
pub fn draw_detail(
    canvas: &mut Canvas<'_>,
    rect: Rect,
    package: Option<&grab_index::Package>,
    installed: Option<&str>,
) {
    let theme = Theme::DEFAULT;
    canvas.fill(rect, theme.input_bg.raw());
    canvas.outline(rect, theme.input_border.raw());

    let x = rect.x + PAD as i32;
    let room = rect.width.saturating_sub(PAD * 2);
    let mut y = rect.y + PAD as i32;

    let Some(package) = package else {
        canvas.text(
            x,
            y,
            "Select a package",
            Style::new(theme.text_placeholder.raw()),
        );
        return;
    };

    canvas.text(
        x,
        y,
        &elide(&package.name, room),
        Style::new(theme.text_primary.raw()).with_weight(Weight::Semibold),
    );
    y += (text_height() + space(2)) as i32;

    let state = match installed {
        Some(version) if version == package.version => format!("installed {}", version),
        Some(version) => format!("installed {} · {} available", version, package.version),
        None => format!("version {}", package.version),
    };
    canvas.text(
        x,
        y,
        &elide(&state, room),
        Style::new(theme.label_text.raw()),
    );
    y += (text_height() + space(1)) as i32;

    let facts = format!("{} · {} bytes", package.category, package.size);
    canvas.text(
        x,
        y,
        &elide(&facts, room),
        Style::new(theme.label_text.raw()),
    );
    y += (text_height() + space(3)) as i32;

    for line in wrap(&package.summary, room) {
        canvas.text(x, y, &line, Style::new(theme.text_primary.raw()));
        y += (text_height() + space(1)) as i32;
    }

    if !package.installs.is_empty() {
        y += space(2) as i32;
        canvas.text(x, y, "Installs", Style::new(theme.label_text.raw()));
        y += (text_height() + space(1)) as i32;
        for path in &package.installs {
            if y > rect.y + rect.height as i32 - (CONTROL_HEIGHT + PAD * 2) as i32 {
                break;
            }
            canvas.text(
                x,
                y,
                &elide(path, room),
                Style::mono(theme.text_placeholder.raw()),
            );
            y += (text_height() + space(1)) as i32;
        }
    }
}

/// The progress strip along the foot: the last thing an operation said.
pub fn draw_status(canvas: &mut Canvas<'_>, rect: Rect, message: &str, failed: bool) {
    let theme = Theme::DEFAULT;
    canvas.fill(rect, theme.input_bg.raw());
    let colour = if failed {
        theme.warning
    } else {
        theme.label_text
    };
    canvas.text(
        rect.x + PAD as i32,
        rect.y + centre(rect.height),
        &elide(message, rect.width.saturating_sub(PAD * 2)),
        Style::new(colour.raw()),
    );
}
