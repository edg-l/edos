//! Where everything sits, and how it is drawn.
//!
//! The geometry is computed by pure functions the event loop calls too, so what
//! the pointer hits and what the eye sees are read from one description of the
//! window rather than from two that drift.

use edos_render::font::{Weight, size};
use edos_render::icons;
use edos_render::metrics::{CONTROL_HEIGHT, TEXT_CELL_HEIGHT, space};
use edos_render::text::{self, Style, Surface};
use edos_render::theme::Theme;
use edos_render::widgets::{Rect, draw_rect};

use crate::model::{Entry, Kind, Place, Volume, format_size};

/// Width of the places rail.
pub const RAIL_W: u32 = 176;
/// Width of the details pane.
pub const DETAILS_W: u32 = 232;
/// Narrower than this and the details pane is dropped rather than squeezed:
/// the listing is the point of the window and it gets the room.
pub const DETAILS_MIN_WIDTH: u32 = 780;
/// Height of the navigation strip.
pub const TOOLBAR_H: u32 = space(2) * 2 + CONTROL_HEIGHT;
/// Height of the column headings.
pub const HEADER_H: u32 = TEXT_CELL_HEIGHT + space(1) * 2;
/// Height of the status strip.
pub const STATUS_H: u32 = TEXT_CELL_HEIGHT + space(1) * 2;
/// One listed row is one control tall, so an inline rename field lands exactly
/// on the row it replaces.
pub const ROW_H: u32 = CONTROL_HEIGHT;
/// Margin from a panel's edge to its contents.
pub const PAD: u32 = space(3);
/// Width of the column the size bars are drawn in.
pub const GAUGE_W: u32 = 64;
/// Length of the bar for the smallest file in view.
const GAUGE_MIN: u32 = 6;
/// Width of the kind column.
pub const KIND_W: u32 = 44;
/// Width of the size column, which clears the widest figure it can hold.
pub const SIZE_W: u32 = 52;
/// Width of the scroll indicator down the right of the listing.
pub const SCROLLBAR_W: u32 = space(1);
/// Height of the meter at the foot of the rail.
pub const METER_H: u32 = space(1);
/// What stands in for the part of a name that did not fit.
const ELLIPSIS: &str = "…";

/// A window divided into its panels.
pub struct Layout {
    pub toolbar: Rect,
    pub rail: Rect,
    pub header: Rect,
    pub list: Rect,
    pub details: Option<Rect>,
    pub status: Rect,
    pub rows_visible: usize,
}

impl Layout {
    pub fn new(width: u32, height: u32) -> Self {
        let body_y = TOOLBAR_H as i32;
        let body_h = height.saturating_sub(TOOLBAR_H + STATUS_H);

        let details = (width >= DETAILS_MIN_WIDTH)
            .then(|| Rect::new((width - DETAILS_W) as i32, body_y, DETAILS_W, body_h));
        let list_x = RAIL_W as i32;
        let list_w = width
            .saturating_sub(RAIL_W)
            .saturating_sub(details.map_or(0, |d| d.width));
        let list_h = body_h.saturating_sub(HEADER_H);

        Self {
            toolbar: Rect::new(0, 0, width, TOOLBAR_H),
            rail: Rect::new(0, body_y, RAIL_W, body_h),
            header: Rect::new(list_x, body_y, list_w, HEADER_H),
            list: Rect::new(list_x, body_y + HEADER_H as i32, list_w, list_h),
            details,
            status: Rect::new(0, height.saturating_sub(STATUS_H) as i32, width, STATUS_H),
            rows_visible: (list_h / ROW_H) as usize,
        }
    }
}

/// Where each column of the listing starts.
pub struct Columns {
    pub icon_x: i32,
    pub name_x: i32,
    pub name_w: u32,
    /// Right edge the size figure is set against.
    pub size_right: i32,
    pub gauge_x: i32,
    pub kind_x: i32,
}

impl Columns {
    pub fn new(list: Rect) -> Self {
        let scrollbar_x = list.x + list.width as i32 - SCROLLBAR_W as i32;
        let right = scrollbar_x - PAD as i32;
        let kind_x = right - KIND_W as i32;
        let gauge_x = kind_x - space(3) as i32 - GAUGE_W as i32;
        let size_right = gauge_x - space(2) as i32;
        let icon_x = list.x + PAD as i32;
        let name_x = icon_x + icons::SIZE as i32 + space(2) as i32;
        Self {
            icon_x,
            name_x,
            name_w: (size_right - space(2) as i32 - SIZE_W as i32 - name_x).max(0) as u32,
            size_right,
            gauge_x,
            kind_x,
        }
    }
}

/// The rectangle of the `index`th row of a listing that starts at `scroll`.
pub fn row_rect(list: Rect, index: usize, scroll: usize) -> Rect {
    let offset = (index.saturating_sub(scroll)) as u32 * ROW_H;
    Rect::new(list.x, list.y + offset as i32, list.width, ROW_H)
}

/// Which listed row a point falls in, given the first row on screen.
pub fn row_at(list: Rect, scroll: usize, y: i32) -> Option<usize> {
    if y < list.y || y >= list.y + list.height as i32 {
        return None;
    }
    Some(scroll + ((y - list.y) as u32 / ROW_H) as usize)
}

/// The three navigation buttons, left to right: back, forward, up.
pub fn nav_buttons(toolbar: Rect) -> [Rect; 3] {
    let y = toolbar.y + (TOOLBAR_H - CONTROL_HEIGHT) as i32 / 2;
    let step = (CONTROL_HEIGHT + space(1)) as i32;
    let x = toolbar.x + PAD as i32;
    [
        Rect::new(x, y, CONTROL_HEIGHT, CONTROL_HEIGHT),
        Rect::new(x + step, y, CONTROL_HEIGHT, CONTROL_HEIGHT),
        Rect::new(x + step * 2, y, CONTROL_HEIGHT, CONTROL_HEIGHT),
    ]
}

/// Where the path begins, after the navigation buttons.
fn crumbs_origin(toolbar: Rect) -> i32 {
    let buttons = nav_buttons(toolbar);
    buttons[2].x + buttons[2].width as i32 + PAD as i32
}

/// The rectangle of each path segment, in order. Segments that would run past
/// the toolbar are dropped from the front, so the directory you are in is the
/// one that always survives.
pub fn crumb_rects(toolbar: Rect, crumbs: &[(String, String)]) -> Vec<Rect> {
    let widths: Vec<u32> = crumbs
        .iter()
        .enumerate()
        .map(|(index, (label, _))| {
            let last = index + 1 == crumbs.len();
            text::width(label, crumb_style(last)) + space(2) * 2
        })
        .collect();

    let origin = crumbs_origin(toolbar);
    let available = (toolbar.x + toolbar.width as i32 - PAD as i32 - origin).max(0) as u32;
    let separator = space(3);

    let mut first = 0;
    while first < widths.len() {
        let total: u32 = widths[first..].iter().sum::<u32>()
            + separator * (widths.len() - first).saturating_sub(1) as u32;
        if total <= available {
            break;
        }
        first += 1;
    }

    let y = toolbar.y + (TOOLBAR_H - ROW_H) as i32 / 2;
    let mut x = origin;
    let mut rects = Vec::with_capacity(crumbs.len());
    for (index, width) in widths.iter().enumerate() {
        if index < first {
            // Off the front of the strip: kept in the list so indices still
            // match the path, but placed where nothing can hit it.
            rects.push(Rect::new(i32::MIN / 2, y, 0, ROW_H));
            continue;
        }
        rects.push(Rect::new(x, y, *width, ROW_H));
        x += *width as i32 + separator as i32;
    }
    rects
}

/// The rectangle of the `index`th place in the rail.
pub fn place_rect(rail: Rect, index: usize) -> Rect {
    Rect::new(
        rail.x + space(1) as i32,
        rail.y
            + PAD as i32
            + TEXT_CELL_HEIGHT as i32
            + space(1) as i32
            + (index as u32 * ROW_H) as i32,
        rail.width - space(2),
        ROW_H,
    )
}

/// Which place a point falls in, if any.
pub fn place_at(rail: Rect, count: usize, x: i32, y: i32) -> Option<usize> {
    (0..count).find(|&index| place_rect(rail, index).contains(x, y))
}

/// The scroll indicator's track, or None when everything fits.
pub fn scrollbar(list: Rect, total: usize, visible: usize) -> Option<Rect> {
    (total > visible).then(|| {
        Rect::new(
            list.x + list.width as i32 - SCROLLBAR_W as i32,
            list.y,
            SCROLLBAR_W,
            list.height,
        )
    })
}

// --- Text styles -------------------------------------------------------------
//
// Two faces with one rule between them: the proportional face says what a thing
// is, the monospaced face says how much of it there is. So names, labels and
// prose are set in Sans, and every figure, tag and path in Mono, where a column
// of them lines up.

fn sans(color: u32) -> Style {
    Style::new(color)
}

fn sans_strong(color: u32) -> Style {
    Style::new(color).with_weight(Weight::Semibold)
}

fn sans_small(color: u32) -> Style {
    Style::new(color).with_px(size::CAPTION)
}

fn heading(color: u32) -> Style {
    Style::new(color)
        .with_px(size::TITLE)
        .with_weight(Weight::Semibold)
}

fn mono(color: u32) -> Style {
    Style::mono(color)
}

fn mono_small(color: u32) -> Style {
    Style::mono(color).with_px(size::CAPTION)
}

fn crumb_style(current: bool) -> Style {
    if current {
        sans_strong(Theme::DEFAULT.text_primary.raw())
    } else {
        sans(Theme::DEFAULT.label_text.raw())
    }
}

/// The ink a name is written in, which is how a listing says what each row is
/// before its icon is read.
fn entry_color(entry: &Entry) -> u32 {
    match entry.kind {
        Kind::Parent => Theme::DEFAULT.text_placeholder.raw(),
        Kind::Dir => Theme::DEFAULT.entry_dir.raw(),
        Kind::Link => Theme::DEFAULT.entry_link.raw(),
        Kind::Special => Theme::DEFAULT.entry_special.raw(),
        Kind::File => Theme::DEFAULT.text_primary.raw(),
    }
}

/// The icon for a row.
fn entry_icon(entry: &Entry) -> &'static icons::Mask {
    match entry.kind {
        Kind::Parent => &icons::ARROW_UP,
        Kind::Dir => &icons::FOLDER,
        Kind::Link => &icons::LINK,
        Kind::Special => &icons::CHIP,
        Kind::File => &icons::FILE,
    }
}

// --- Canvas ------------------------------------------------------------------

/// A window's back buffer, with the handful of operations this program draws
/// with.
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

    pub fn hline(&mut self, x: i32, y: i32, width: u32, color: u32) {
        self.fill(Rect::new(x, y, width, 1), color);
    }

    pub fn icon(&mut self, x: i32, y: i32, mask: &icons::Mask, color: u32) {
        icons::draw(self.buf, self.width, self.height, x, y, mask, color);
    }

    /// Draw a line of text with its top edge at `y`, and report its width.
    pub fn text(&mut self, x: i32, y: i32, string: &str, style: Style) -> u32 {
        let mut surface = Surface {
            pixels: self.buf,
            width: self.width,
            height: self.height,
        };
        text::draw(&mut surface, x, y, string, style);
        text::width(string, style)
    }

    /// Draw text vertically centred in `rect`, starting at `x`.
    pub fn text_in(&mut self, x: i32, rect: Rect, string: &str, style: Style) -> u32 {
        let y = rect.y + (rect.height as i32 - text::line_height(style) as i32) / 2;
        self.text(x, y, string, style)
    }

    /// Draw text ending at `right`, vertically centred in `rect`.
    pub fn text_right(&mut self, right: i32, rect: Rect, string: &str, style: Style) {
        let width = text::width(string, style) as i32;
        self.text_in(right - width, rect, string, style);
    }
}

/// Shorten `string` until it fits `available` pixels, marking what was cut.
pub fn elide(string: &str, available: u32, style: Style) -> String {
    if text::width(string, style) <= available {
        return string.to_string();
    }
    let room = available.saturating_sub(text::width(ELLIPSIS, style));
    let mut kept = String::new();
    for ch in string.chars() {
        let mut candidate = kept.clone();
        candidate.push(ch);
        if text::width(&candidate, style) > room {
            break;
        }
        kept = candidate;
    }
    kept.push_str(ELLIPSIS);
    kept
}

// --- Panels ------------------------------------------------------------------

/// Draw the navigation strip: the three buttons and the path.
pub fn draw_toolbar(
    canvas: &mut Canvas,
    layout: &Layout,
    crumbs: &[(String, String)],
    can_go_back: bool,
    can_go_forward: bool,
    can_go_up: bool,
    hovered_button: Option<usize>,
    hovered_crumb: Option<usize>,
) {
    let theme = &Theme::DEFAULT;
    canvas.fill(layout.toolbar, theme.title_active_bottom.raw());
    canvas.hline(
        layout.toolbar.x,
        layout.toolbar.y + layout.toolbar.height as i32 - 1,
        layout.toolbar.width,
        theme.input_border.raw(),
    );

    let buttons = nav_buttons(layout.toolbar);
    let masks = [
        &icons::CHEVRON_LEFT,
        &icons::CHEVRON_RIGHT,
        &icons::ARROW_UP,
    ];
    let enabled = [can_go_back, can_go_forward, can_go_up];
    for (index, rect) in buttons.iter().enumerate() {
        if enabled[index] && hovered_button == Some(index) {
            canvas.fill(*rect, theme.button_hover.raw());
        }
        let ink = if !enabled[index] {
            theme.text_disabled
        } else if hovered_button == Some(index) {
            theme.text_primary
        } else {
            theme.label_text
        };
        canvas.icon(
            rect.x + (rect.width - icons::SIZE as u32) as i32 / 2,
            rect.y + (rect.height - icons::SIZE as u32) as i32 / 2,
            masks[index],
            ink.raw(),
        );
    }

    let rects = crumb_rects(layout.toolbar, crumbs);
    for (index, ((label, _), rect)) in crumbs.iter().zip(&rects).enumerate() {
        if rect.width == 0 {
            continue;
        }
        let current = index + 1 == crumbs.len();
        let mut style = crumb_style(current);
        if !current && hovered_crumb == Some(index) {
            style = sans(theme.text_primary.raw());
        }
        canvas.text_in(rect.x + space(2) as i32, *rect, label, style);

        if current {
            canvas.fill(
                Rect::new(
                    rect.x + space(2) as i32,
                    rect.y + rect.height as i32 - space(1) as i32,
                    rect.width - space(4),
                    2,
                ),
                theme.focus_ring.raw(),
            );
        } else if index + 1 < crumbs.len() && rects[index + 1].width > 0 {
            let gap = rect.x + rect.width as i32;
            canvas.text_in(gap + 1, *rect, "›", sans(theme.text_placeholder.raw()));
        }
    }
}

/// Draw the places rail and the meter for the volume in view.
pub fn draw_rail(
    canvas: &mut Canvas,
    layout: &Layout,
    places: &[Place],
    active: Option<usize>,
    hovered: Option<usize>,
    volume: Option<&Volume>,
) {
    let theme = &Theme::DEFAULT;
    canvas.fill(layout.rail, theme.input_bg.raw());
    canvas.fill(
        Rect::new(
            layout.rail.x + layout.rail.width as i32 - 1,
            layout.rail.y,
            1,
            layout.rail.height,
        ),
        theme.input_border.raw(),
    );

    canvas.text(
        layout.rail.x + PAD as i32,
        layout.rail.y + PAD as i32,
        "PLACES",
        mono_small(theme.text_placeholder.raw()),
    );

    for (index, place) in places.iter().enumerate() {
        let rect = place_rect(layout.rail, index);
        if rect.y + rect.height as i32 > layout.rail.y + layout.rail.height as i32 {
            break;
        }
        if active == Some(index) {
            canvas.fill(rect, theme.list_selected.raw());
            canvas.fill(
                Rect::new(rect.x, rect.y, 2, rect.height),
                theme.focus_ring.raw(),
            );
        } else if hovered == Some(index) {
            canvas.fill(rect, theme.button_hover.raw());
        }

        let icon = if place.is_home {
            &icons::HOUSE
        } else if place.persistent {
            &icons::DISK
        } else {
            &icons::CHIP
        };
        let ink = if active == Some(index) {
            theme.text_primary
        } else {
            theme.label_text
        };
        let icon_x = rect.x + space(2) as i32;
        canvas.icon(
            icon_x,
            rect.y + (rect.height - icons::SIZE as u32) as i32 / 2,
            icon,
            ink.raw(),
        );

        let note_width = text::width(&place.note, mono_small(0));
        let label_x = icon_x + icons::SIZE as i32 + space(2) as i32;
        let label_room = (rect.x + rect.width as i32
            - space(2) as i32
            - note_width as i32
            - space(2) as i32
            - label_x)
            .max(0) as u32;
        let style = sans(ink.raw());
        canvas.text_in(
            label_x,
            rect,
            &elide(&place.label, label_room, style),
            style,
        );
        canvas.text_right(
            rect.x + rect.width as i32 - space(2) as i32,
            rect,
            &place.note,
            mono_small(theme.text_placeholder.raw()),
        );
    }

    let Some(volume) = volume else {
        return;
    };
    let foot = layout.rail.y + layout.rail.height as i32 - PAD as i32;
    let line_h = TEXT_CELL_HEIGHT as i32;
    let x = layout.rail.x + PAD as i32;
    let width = layout.rail.width - PAD * 2;

    canvas.hline(
        x,
        foot - line_h * 3 - space(3) as i32,
        width,
        theme.input_border.raw(),
    );
    canvas.text(
        x,
        foot - line_h * 3 + space(1) as i32,
        volume.filesystem.name(),
        mono_small(theme.text_primary.raw()),
    );
    canvas.text_right(
        x + width as i32,
        Rect::new(x, foot - line_h * 3 + space(1) as i32, width, line_h as u32),
        &volume.mount_point,
        mono_small(theme.text_placeholder.raw()),
    );

    match volume.used_fraction() {
        Some(used) => {
            let track = Rect::new(x, foot - line_h * 2 + space(1) as i32, width, METER_H);
            canvas.fill(track, theme.slider_track.raw());
            let filled = (width as f32 * used) as u32;
            canvas.fill(
                Rect::new(track.x, track.y, filled.max(1), METER_H),
                theme.checkbox_check.raw(),
            );
            canvas.text(
                x,
                foot - line_h,
                &format!(
                    "{} free of {}",
                    format_size(volume.free_bytes),
                    format_size(volume.total_bytes)
                ),
                mono_small(theme.text_placeholder.raw()),
            );
        }
        None => {
            let style = mono_small(theme.text_placeholder.raw());
            canvas.text(
                x,
                foot - line_h,
                &elide(volume.filesystem.nature(), width, style),
                style,
            );
        }
    }
}

/// Draw the column headings above the listing.
pub fn draw_header(canvas: &mut Canvas, layout: &Layout) {
    let theme = &Theme::DEFAULT;
    let columns = Columns::new(layout.list);
    canvas.fill(layout.header, theme.input_bg.raw());
    canvas.hline(
        layout.header.x,
        layout.header.y + layout.header.height as i32 - 1,
        layout.header.width,
        theme.input_border.raw(),
    );

    let style = mono_small(theme.text_placeholder.raw());
    canvas.text_in(columns.name_x, layout.header, "NAME", style);
    canvas.text_right(columns.size_right, layout.header, "SIZE", style);
    canvas.text_in(columns.kind_x, layout.header, "KIND", style);
}

/// The span the size bars are measured against: the smallest and largest file
/// in view.
#[derive(Clone, Copy)]
pub struct Span {
    pub smallest: u64,
    pub largest: u64,
}

impl Span {
    /// The span of a listing, or None when nothing in it has a size.
    pub fn of(entries: &[Entry]) -> Option<Self> {
        let mut sizes = entries.iter().filter_map(|entry| entry.size);
        let first = sizes.next()?;
        let (smallest, largest) = sizes.fold((first, first), |(low, high), size| {
            (low.min(size), high.max(size))
        });
        Some(Self { smallest, largest })
    }

    /// Where `size` falls in the span, from 0 at the smallest file to 1 at the
    /// largest.
    ///
    /// Logarithmic, and measured from the smallest file rather than from zero.
    /// A directory of binaries between 60K and 160K is the ordinary case, and
    /// measured from zero every one of those bars comes out the same length:
    /// the column would be decoration. Measured across the range that is
    /// actually present, it ranks what is in front of you.
    fn position(&self, size: u64) -> f32 {
        let low = ((self.smallest + 1) as f32).ln();
        let high = ((self.largest + 1) as f32).ln();
        if high - low < f32::EPSILON {
            return 1.0;
        }
        ((((size + 1) as f32).ln() - low) / (high - low)).clamp(0.0, 1.0)
    }
}

/// Draw the listing.
///
/// `span` is the range of sizes present, which is what the bars are measured
/// against: the column answers "what is big in here", not "what is big in the
/// abstract".
pub fn draw_list(
    canvas: &mut Canvas,
    layout: &Layout,
    entries: &[Entry],
    selected: usize,
    scroll: usize,
    hovered: Option<usize>,
    span: Option<Span>,
    renaming: bool,
) {
    let theme = &Theme::DEFAULT;
    canvas.fill(layout.list, theme.background.raw());
    let columns = Columns::new(layout.list);

    for index in scroll..(scroll + layout.rows_visible).min(entries.len()) {
        let entry = &entries[index];
        let rect = row_rect(layout.list, index, scroll);

        if index == selected {
            canvas.fill(rect, theme.list_selected.raw());
            canvas.fill(
                Rect::new(rect.x, rect.y, 2, rect.height),
                theme.focus_ring.raw(),
            );
        } else if hovered == Some(index) {
            canvas.fill(rect, theme.button_hover.raw());
        }

        let ink = entry_color(entry);
        canvas.icon(
            columns.icon_x,
            rect.y + (rect.height - icons::SIZE as u32) as i32 / 2,
            entry_icon(entry),
            ink,
        );

        // The row being renamed keeps its icon and its columns; only the name
        // is replaced, by the field the container draws over it.
        if !(renaming && index == selected) {
            let style = sans(ink);
            let mut room = columns.name_w;
            let mut name = entry.name.clone();
            if let Some(target) = &entry.target {
                let arrow = format!("  → {target}");
                let arrow_width = text::width(&arrow, sans_small(0));
                if arrow_width < room / 2 {
                    room -= arrow_width;
                    canvas.text_in(
                        columns.name_x + text::width(&name, style) as i32,
                        rect,
                        &arrow,
                        sans_small(theme.text_placeholder.raw()),
                    );
                }
            }
            name = elide(&name, room, style);
            canvas.text_in(columns.name_x, rect, &name, style);
        }

        match entry.size {
            Some(size) => {
                canvas.text_right(
                    columns.size_right,
                    rect,
                    &format_size(size),
                    mono(theme.label_text.raw()),
                );
                if let Some(span) = span {
                    draw_gauge(canvas, rect, columns.gauge_x, span.position(size));
                }
            }
            None => {
                canvas.text_right(
                    columns.size_right,
                    rect,
                    "—",
                    mono(theme.text_placeholder.raw()),
                );
            }
        }

        canvas.text_in(
            columns.kind_x,
            rect,
            &entry.kind_label(),
            mono_small(theme.text_placeholder.raw()),
        );
    }

    if entries.is_empty() {
        let rect = Rect::new(layout.list.x, layout.list.y, layout.list.width, ROW_H * 2);
        canvas.text_in(
            columns.name_x,
            rect,
            "This folder is empty.",
            sans(Theme::DEFAULT.text_placeholder.raw()),
        );
    }

    if let Some(track) = scrollbar(layout.list, entries.len(), layout.rows_visible) {
        canvas.fill(track, theme.input_bg.raw());
        let span = entries.len() as f32;
        let thumb_h = ((layout.rows_visible as f32 / span) * track.height as f32) as u32;
        let thumb_y = ((scroll as f32 / span) * track.height as f32) as i32;
        canvas.fill(
            Rect::new(
                track.x,
                track.y + thumb_y,
                track.width,
                thumb_h.clamp(space(4), track.height),
            ),
            theme.input_border.raw(),
        );
    }
}

/// Draw one size bar: a track showing the column's full span, and a fill at
/// `position` along it.
fn draw_gauge(canvas: &mut Canvas, row: Rect, x: i32, position: f32) {
    let theme = &Theme::DEFAULT;
    let track_y = row.y + (row.height as i32 - 1) / 2;
    canvas.hline(x, track_y, GAUGE_W, theme.input_border.raw());

    // The smallest file in view still gets a bar. Being at the bottom of the
    // range is not the same as having no size, and a row with nothing drawn
    // reads as the second.
    let filled = GAUGE_MIN + ((GAUGE_W - GAUGE_MIN) as f32 * position) as u32;
    canvas.fill(Rect::new(x, track_y - 1, filled, 3), theme.size_gauge.raw());
}

/// Draw the details pane: what the selected row is, and what it is worth
/// knowing about it.
pub fn draw_details(
    canvas: &mut Canvas,
    pane: Rect,
    title: &str,
    subtitle: &str,
    facts: &[(&str, String)],
    thumbnail: Option<(&[u32], u32, u32)>,
) {
    let theme = &Theme::DEFAULT;
    canvas.fill(pane, theme.input_bg.raw());
    canvas.fill(
        Rect::new(pane.x, pane.y, 1, pane.height),
        theme.input_border.raw(),
    );

    let x = pane.x + PAD as i32;
    let width = pane.width - PAD * 2;
    let mut y = pane.y + PAD as i32;

    let title_style = heading(theme.text_primary.raw());
    canvas.text(x, y, &elide(title, width, title_style), title_style);
    y += text::line_height(title_style) as i32 + space(1) as i32;
    canvas.text(x, y, subtitle, mono_small(theme.text_placeholder.raw()));
    y += TEXT_CELL_HEIGHT as i32 + space(2) as i32;

    canvas.hline(x, y, width, theme.input_border.raw());
    y += space(3) as i32;

    if let Some((pixels, thumb_w, thumb_h)) = thumbnail {
        let origin_x = x + (width.saturating_sub(thumb_w) / 2) as i32;
        for row in 0..thumb_h {
            for col in 0..thumb_w {
                let dst_x = origin_x + col as i32;
                let dst_y = y + row as i32;
                if dst_x < 0 || dst_y < 0 || dst_x >= canvas.width as i32 {
                    continue;
                }
                let index = dst_y as usize * canvas.width as usize + dst_x as usize;
                if let Some(pixel) = canvas.buf.get_mut(index) {
                    *pixel = pixels[(row * thumb_w + col) as usize];
                }
            }
        }
        y += thumb_h as i32 + space(3) as i32;
        canvas.hline(x, y, width, theme.input_border.raw());
        y += space(3) as i32;
    }

    let label_w = space(14);
    for (label, value) in facts {
        let row = Rect::new(x, y, width, TEXT_CELL_HEIGHT);
        canvas.text_in(x, row, label, sans_small(theme.text_placeholder.raw()));
        let style = mono_small(theme.text_primary.raw());
        canvas.text_in(
            x + label_w as i32,
            row,
            &elide(value, width - label_w, style),
            style,
        );
        y += TEXT_CELL_HEIGHT as i32 + space(1) as i32;
    }

    draw_hints(canvas, pane);
}

/// The keys, at the foot of the details pane. A machine with no manual should
/// say what it responds to.
fn draw_hints(canvas: &mut Canvas, pane: Rect) {
    const HINTS: [(&str, &str); 6] = [
        ("Enter", "open"),
        ("Backspace", "up one level"),
        ("F2", "rename"),
        ("Del", "delete"),
        ("N", "new folder"),
        ("F5", "reload"),
    ];

    /// Width of the key column, which clears the longest key name.
    const HINT_KEY_W: u32 = space(21);

    let theme = &Theme::DEFAULT;
    let x = pane.x + PAD as i32;
    let width = pane.width - PAD * 2;
    let line = TEXT_CELL_HEIGHT as i32;
    let mut y = pane.y + pane.height as i32 - PAD as i32 - line * HINTS.len() as i32;

    canvas.hline(x, y - space(3) as i32, width, theme.input_border.raw());
    for (key, action) in HINTS {
        let row = Rect::new(x, y, width, TEXT_CELL_HEIGHT);
        canvas.text_in(x, row, key, mono_small(theme.label_text.raw()));
        canvas.text_in(
            x + HINT_KEY_W as i32,
            row,
            action,
            sans_small(theme.text_placeholder.raw()),
        );
        y += line;
    }
}

/// Draw the status strip: what is in the directory on the left, what it is
/// stored on at the right.
pub fn draw_status(
    canvas: &mut Canvas,
    layout: &Layout,
    message: &str,
    warning: bool,
    volume_note: &str,
) {
    let theme = &Theme::DEFAULT;
    canvas.fill(layout.status, theme.taskbar_bg_bottom.raw());
    canvas.hline(
        layout.status.x,
        layout.status.y,
        layout.status.width,
        theme.input_border.raw(),
    );

    let ink = if warning {
        theme.warning
    } else {
        theme.label_text
    };
    canvas.text_in(
        layout.status.x + PAD as i32,
        layout.status,
        message,
        sans_small(ink.raw()),
    );
    canvas.text_right(
        layout.status.x + layout.status.width as i32 - PAD as i32,
        layout.status,
        volume_note,
        mono_small(theme.text_placeholder.raw()),
    );
}
