//! Where everything sits, and how it is drawn.
//!
//! The geometry is computed by pure functions the event loop calls too, so
//! what the pointer hits and what the eye sees come from one description of
//! the window rather than from two that drift. This is what `edos-files` does
//! and the reason its rows and its inline rename field land on each other
//! exactly.

use edos_render::font::Weight;
use edos_render::metrics::{CONTROL_HEIGHT, space};
use edos_render::text::{self, Style, Surface};
use edos_render::theme::Theme;
use edos_render::widgets::{Rect, char_width, draw_rect};

use crate::buffer::Buffer;

/// Width of the sidebar tree.
pub const SIDEBAR_W: u32 = space(56);
/// Height of the tab strip: a tab is a control, and sits on the shared row
/// rhythm.
pub const TAB_H: u32 = CONTROL_HEIGHT;
/// Height of the status strip: shorter than a control, because nothing in it
/// is clicked.
pub const STATUS_H: u32 = space(6);
/// Height of the prompt bar: a field with the shell's standard breathing room
/// around it.
pub const PROMPT_H: u32 = CONTROL_HEIGHT + space(2) * 2;
/// Row height shared by every list in the shell: the tree and the tabs. Read
/// once the sidebar draws rows rather than just its panel.
#[allow(dead_code)]
pub const ROW_H: u32 = CONTROL_HEIGHT;
/// Indent per tree level. Read once the sidebar has a tree.
#[allow(dead_code)]
pub const TREE_INDENT: u32 = space(3);
/// Width of the change ribbon, between the gutter and the text.
pub const RIBBON_W: u32 = 2;
/// Narrower than this and the sidebar is dropped rather than squeezed:
/// squeezing a tree to nothing is worse than not showing it.
pub const SIDEBAR_MIN_WIDTH: u32 = 640;
/// Margin from a panel's edge to its contents.
pub const PAD: u32 = space(3);
/// What stands in for the part of a name that did not fit.
const ELLIPSIS: &str = "…";

/// Number of decimal digits in `n`, with 0 counting as one digit.
fn digits(n: usize) -> u32 {
    let mut n = n;
    let mut count = 1;
    while n >= 10 {
        n /= 10;
        count += 1;
    }
    count
}

/// A window divided into its panels. Derived state: rebuilt at the top of
/// every `draw()` rather than only on resize, because the gutter width, the
/// sidebar and the prompt all change during ordinary use and a stale `Layout`
/// puts clicks on the wrong character with nothing on screen to say why.
pub struct Layout {
    pub sidebar: Option<Rect>,
    pub tabs: Rect,
    pub pane: Rect,
    pub gutter: Rect,
    /// Left edge of the change ribbon.
    pub ribbon_x: i32,
    /// Left edge of the first text column.
    pub text_x: i32,
    /// Set only while the find/go-to-line/open prompt is showing.
    #[allow(dead_code)]
    pub prompt: Option<Rect>,
    pub status: Rect,
    pub rows_visible: usize,
    pub cols_visible: usize,
    pub line_h: u32,
}

impl Layout {
    pub fn new(
        width: u32,
        height: u32,
        sidebar_open: bool,
        prompt_open: bool,
        last_line_number: usize,
    ) -> Self {
        let line_h = text::line_height(Style::mono(0)).max(1);

        let status = Rect::new(0, height.saturating_sub(STATUS_H) as i32, width, STATUS_H);
        let body_h = status.y as u32;

        let sidebar = (sidebar_open && width >= SIDEBAR_MIN_WIDTH)
            .then(|| Rect::new(0, 0, SIDEBAR_W, body_h));
        let body_x = sidebar.map_or(0, |rect| rect.x + rect.width as i32);
        let body_w = width.saturating_sub(body_x as u32);

        let tabs = Rect::new(body_x, 0, body_w, TAB_H);

        let prompt_h = if prompt_open { PROMPT_H } else { 0 };
        let pane_y = TAB_H as i32;
        let pane_h = body_h.saturating_sub(TAB_H).saturating_sub(prompt_h);
        let pane = Rect::new(body_x, pane_y, body_w, pane_h);

        let prompt =
            prompt_open.then(|| Rect::new(body_x, status.y - PROMPT_H as i32, body_w, PROMPT_H));

        // The gutter's width is the one place the character grid decides a
        // rectangle rather than the other way around: a 5-digit file gets a
        // wider gutter, and nothing is reserved for digits that do not exist.
        let gutter_w = digits(last_line_number) * char_width() + space(2) * 2;
        let gutter = Rect::new(pane.x, pane.y, gutter_w, pane.height);
        let ribbon_x = gutter.x + gutter.width as i32;
        let text_x = ribbon_x + RIBBON_W as i32 + space(2) as i32;

        let rows_visible = (pane.height / line_h) as usize;
        let cols_visible =
            ((pane.x + pane.width as i32 - text_x).max(0) as u32 / char_width()) as usize;

        Self {
            sidebar,
            tabs,
            pane,
            gutter,
            ribbon_x,
            text_x,
            prompt,
            status,
            rows_visible,
            cols_visible,
            line_h,
        }
    }
}

/// Pixel rectangle of one character cell in the pane, given where the view is
/// scrolled to. The same arithmetic `draw_pane` places a glyph with, so a
/// click and the character under it are read from one description.
pub fn cell_rect(
    layout: &Layout,
    scroll_line: usize,
    scroll_col: usize,
    line: usize,
    col: usize,
) -> Rect {
    let row = line.saturating_sub(scroll_line);
    let column = col.saturating_sub(scroll_col);
    Rect::new(
        layout.text_x + (column as u32 * char_width()) as i32,
        layout.pane.y + (row as u32 * layout.line_h) as i32,
        char_width(),
        layout.line_h,
    )
}

/// Which line a point falls on, given the first line on screen. None outside
/// the pane vertically.
pub fn line_at(layout: &Layout, scroll_line: usize, y: i32) -> Option<usize> {
    if y < layout.pane.y || y >= layout.pane.y + layout.pane.height as i32 {
        return None;
    }
    Some(scroll_line + ((y - layout.pane.y) as u32 / layout.line_h) as usize)
}

/// Which column a point falls in, given the first column on screen. Not
/// clamped to a line's length — the caller knows how long the line is and
/// this function does not.
pub fn col_at(layout: &Layout, scroll_col: usize, x: i32) -> usize {
    let rel = (x - layout.text_x).max(0) as u32;
    scroll_col + (rel / char_width()) as usize
}

// --- Text styles -------------------------------------------------------------
//
// The type rule: monospaced inside the pane, proportional everywhere else.
// The tree, the tabs and the status bar say things *about* the document and
// are set in Sans; the document itself is a character grid and is set in
// Mono. `char_width()` belongs to the pane group only — see `cell_rect`,
// `line_at`, `col_at` and this module's gutter arithmetic above.

fn sans(color: u32) -> Style {
    Style::new(color)
}

fn sans_strong(color: u32) -> Style {
    Style::new(color).with_weight(Weight::Semibold)
}

fn sans_small(color: u32) -> Style {
    Style::new(color).with_px(edos_render::font::size::CAPTION)
}

fn mono(color: u32) -> Style {
    Style::mono(color)
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

/// Draw the tab strip. A single active tab in this phase; Phase 5 replaces
/// this with one tab per open buffer.
pub fn draw_tabs(canvas: &mut Canvas, rect: Rect, label: &str) {
    let theme = &Theme::DEFAULT;
    canvas.fill(rect, theme.taskbar_bg_top.raw());
    canvas.hline(
        rect.x,
        rect.y + rect.height as i32 - 1,
        rect.width,
        theme.input_border.raw(),
    );

    let style = sans(theme.text_primary.raw());
    let available = rect.width.saturating_sub(PAD * 2);
    let text = elide(label, available, style);
    let tab_w = (text::width(&text, style) + PAD * 2).min(rect.width);
    let tab = Rect::new(rect.x, rect.y, tab_w, rect.height);
    canvas.fill(tab, theme.background.raw());
    canvas.fill(
        Rect::new(tab.x, tab.y, tab.width, 2),
        theme.focus_ring.raw(),
    );
    canvas.text_in(tab.x + PAD as i32, tab, &text, style);
}

/// Draw the sidebar: the panel and the root's name. Phase 5 adds the tree
/// inside it.
pub fn draw_sidebar(canvas: &mut Canvas, rect: Rect, root_name: &str) {
    let theme = &Theme::DEFAULT;
    canvas.fill(rect, theme.input_bg.raw());
    canvas.fill(
        Rect::new(rect.x + rect.width as i32 - 1, rect.y, 1, rect.height),
        theme.input_border.raw(),
    );

    let style = sans_strong(theme.text_primary.raw());
    let name = elide(root_name, rect.width.saturating_sub(PAD * 2), style);
    canvas.text(rect.x + PAD as i32, rect.y + PAD as i32, &name, style);
}

/// Draw the editor pane: background, current-line highlight, gutter, text and
/// caret. One `text::draw` per non-space character, at `text_x + col *
/// char_width()`, so the pen advance matches every rectangle in this file
/// instead of the face's true fractional advance.
pub fn draw_pane(canvas: &mut Canvas, layout: &Layout, buffer: &Buffer) {
    let theme = &Theme::DEFAULT;
    canvas.fill(layout.pane, theme.background.raw());

    let visible_rows = layout.rows_visible.max(1);
    let visible_cols = layout.cols_visible.max(1);

    for row in 0..visible_rows {
        let line_index = buffer.scroll_line + row;
        let Some(line) = buffer.lines.get(line_index) else {
            break;
        };
        let y = layout.pane.y + (row as u32 * layout.line_h) as i32;
        let current = line_index == buffer.cursor.line;

        if current {
            let width = (layout.pane.x + layout.pane.width as i32 - layout.ribbon_x).max(0) as u32;
            canvas.fill(
                Rect::new(layout.ribbon_x, y, width, layout.line_h),
                theme.editor_line_highlight.raw(),
            );
        }

        if line.changed {
            canvas.fill(
                Rect::new(layout.ribbon_x, y, RIBBON_W, layout.line_h),
                theme.editor_change.raw(),
            );
        }

        // Line number, right-aligned in the gutter, one character at a time
        // so it lands on the grid the text does.
        let number = (line_index + 1).to_string();
        let ink = if current {
            theme.text_primary
        } else {
            theme.editor_gutter
        };
        let number_w = number.chars().count() as u32 * char_width();
        let mut nx =
            layout.gutter.x + layout.gutter.width as i32 - space(2) as i32 - number_w as i32;
        for ch in number.chars() {
            canvas.text(nx, y, &ch.to_string(), mono(ink.raw()));
            nx += char_width() as i32;
        }

        for (col, ch) in line
            .text
            .chars()
            .enumerate()
            .skip(buffer.scroll_col)
            .take(visible_cols)
        {
            if ch == ' ' {
                continue;
            }
            let rect = cell_rect(
                layout,
                buffer.scroll_line,
                buffer.scroll_col,
                line_index,
                col,
            );
            canvas.text(
                rect.x,
                rect.y,
                &ch.to_string(),
                mono(theme.text_primary.raw()),
            );
        }
    }

    let cursor = buffer.cursor;
    if cursor.line >= buffer.scroll_line
        && cursor.line < buffer.scroll_line + visible_rows
        && cursor.col >= buffer.scroll_col
        && cursor.col < buffer.scroll_col + visible_cols
    {
        let rect = cell_rect(
            layout,
            buffer.scroll_line,
            buffer.scroll_col,
            cursor.line,
            cursor.col,
        );
        canvas.fill(
            Rect::new(rect.x, rect.y, 2, layout.line_h),
            theme.focus_ring.raw(),
        );
    }
}

/// Draw the status strip: what the document is, where the cursor sits, and
/// what it is stored on. `note`, when set, replaces that with the result of
/// the last command — save, undo, redo — drawn in `warning` when it is a
/// failure.
pub fn draw_status(
    canvas: &mut Canvas,
    layout: &Layout,
    name: &str,
    language: &str,
    line: usize,
    col: usize,
    indent: &str,
    encoding: &str,
    volume: &str,
    note: Option<(&str, bool)>,
) {
    let theme = &Theme::DEFAULT;
    canvas.fill(layout.status, theme.taskbar_bg_bottom.raw());
    canvas.hline(
        layout.status.x,
        layout.status.y,
        layout.status.width,
        theme.input_border.raw(),
    );

    let position = format!("Ln {line}, Col {col}");
    let composed = [name, language, position.as_str(), indent, encoding].join("   ");
    let (message, warning) = note.unwrap_or((composed.as_str(), false));
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
        volume,
        sans_small(theme.text_placeholder.raw()),
    );
}
