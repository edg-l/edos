//! Where everything sits, and how it is drawn.
//!
//! The geometry is computed by pure functions the event loop calls too, so
//! what the pointer hits and what the eye sees come from one description of
//! the window rather than from two that drift. This is what `edos-files` does
//! and the reason its rows and its inline rename field land on each other
//! exactly.

use edos_render::font::Weight;
use edos_render::icons;
use edos_render::metrics::{CONTROL_HEIGHT, space};
use edos_render::surface::Surface;
use edos_render::text::{self, Style, elide};
use edos_render::theme::Theme;
use edos_render::widgets::{Rect, char_width, text_width};

use crate::buffer::{Buffer, Position};
use crate::syntax::{self, TokenKind};
use crate::tree::Tree;

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
/// Row height shared by every list in the shell: the tree and the tabs.
pub const ROW_H: u32 = CONTROL_HEIGHT;
/// Indent per tree level.
pub const TREE_INDENT: u32 = space(3);
/// Width of the change ribbon, between the gutter and the text.
pub const RIBBON_W: u32 = 2;
/// Narrower than this and the sidebar is dropped rather than squeezed:
/// squeezing a tree to nothing is worse than not showing it.
pub const SIDEBAR_MIN_WIDTH: u32 = 640;
/// Margin from a panel's edge to its contents.
pub const PAD: u32 = space(3);
/// Widest a tab's label draws before it is elided; past this the tab stops
/// growing rather than crowding out the others.
const TAB_MAX_W: u32 = space(48);
/// Width of the close cell at a tab's right edge, where the dirty dot or the
/// × sits.
const TAB_CLOSE_W: u32 = space(6);
/// What stands in for the part of a name that did not fit.
/// Reserved width from the prompt field's right edge to the bar's own edge,
/// for the report — enough for "Not a line number", the longest string the
/// bar prints.
const PROMPT_REPORT_W: u32 = space(48);

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

/// Where the prompt bar's field sits: after its label, leaving room on the
/// right for the report. The field is constructed with this rectangle once,
/// when the prompt opens, and draws itself from then on — there is only one
/// field at a time, so unlike the tree and the tabs this geometry has no
/// separate hit-testing copy to stay in step with.
pub fn prompt_field_rect(rect: Rect, label: &str) -> Rect {
    let label_w = text_width(label);
    let field_x = rect.x + PAD as i32 * 2 + label_w as i32;
    let field_y = rect.y + (rect.height as i32 - CONTROL_HEIGHT as i32) / 2;
    let field_w = (rect.width as i32 - (field_x - rect.x) - PROMPT_REPORT_W as i32 - PAD as i32)
        .max(0) as u32;
    Rect::new(field_x, field_y, field_w, CONTROL_HEIGHT)
}

/// Height of the sidebar's header — the root directory's name — that the
/// tree's rows start clear of.
fn sidebar_header_h() -> u32 {
    PAD + text::line_height(sans_strong(0)) + PAD
}

/// Pixel rectangle of the `index`th tree row in a sidebar scrolled to
/// `scroll`. The same arithmetic `tree_row_at` reads a point back against, so
/// a click and the row under it come from one description — the reason
/// `edos-files`' rows and its inline rename field land on each other exactly.
pub fn tree_row_rect(sidebar: Rect, index: usize, scroll: usize) -> Rect {
    let offset = (index.saturating_sub(scroll)) as u32 * ROW_H;
    Rect::new(
        sidebar.x,
        sidebar.y + sidebar_header_h() as i32 + offset as i32,
        sidebar.width,
        ROW_H,
    )
}

/// Which tree row a point falls in, given the first row on screen.
pub fn tree_row_at(sidebar: Rect, scroll: usize, y: i32) -> Option<usize> {
    let top = sidebar.y + sidebar_header_h() as i32;
    if y < top || y >= sidebar.y + sidebar.height as i32 {
        return None;
    }
    Some(scroll + ((y - top) as u32 / ROW_H) as usize)
}

/// How many tree rows fit in the sidebar at once, for clamping the scroll.
pub fn tree_rows_visible(sidebar: Rect) -> usize {
    (sidebar.height.saturating_sub(sidebar_header_h()) / ROW_H) as usize
}

/// Pixel rectangle of each tab in the strip, left to right. A label widens
/// its tab up to `TAB_MAX_W`; past that it is elided at draw time rather than
/// growing the tab further. The same rectangles `draw_tabs` fills.
pub fn tab_rects(tabs: Rect, labels: &[String]) -> Vec<Rect> {
    let mut x = tabs.x;
    labels
        .iter()
        .map(|label| {
            let label_w = text_width(label).min(TAB_MAX_W);
            let width = label_w + PAD * 2 + TAB_CLOSE_W;
            let rect = Rect::new(x, tabs.y, width, tabs.height);
            x += width as i32;
            rect
        })
        .collect()
}

/// The close cell at a tab's right edge, where the dirty dot or the × sits.
pub fn tab_close_rect(tab: Rect) -> Rect {
    Rect::new(
        tab.x + tab.width as i32 - TAB_CLOSE_W as i32,
        tab.y,
        TAB_CLOSE_W,
        tab.height,
    )
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

/// The dirty dot or the × in a tab's close cell.
fn close_glyph(color: u32) -> Style {
    sans_small(color)
}

// --- Panels ------------------------------------------------------------------

/// Draw the tab strip: one tab per open buffer, the active one filled with a
/// `focus_ring` hairline on top. Each tab's close cell carries the
/// `editor_change` dot while its buffer is dirty, or an elided `×` — bright
/// while hovered, dim otherwise — so the control is always reachable even on
/// a tab that never stops being dirty.
pub fn draw_tabs(
    canvas: &mut Surface,
    rect: Rect,
    labels: &[String],
    dirty: &[bool],
    active: usize,
    hovered: Option<usize>,
    hovered_close: bool,
) {
    let theme = &Theme::DEFAULT;
    canvas.fill(rect, theme.taskbar_bg_top.raw());
    canvas.hline(
        rect.x,
        rect.y + rect.height as i32 - 1,
        rect.width,
        theme.input_border.raw(),
    );

    let rects = tab_rects(rect, labels);
    for (index, (label, tab)) in labels.iter().zip(&rects).enumerate() {
        let is_active = index == active;
        let is_hovered = hovered == Some(index);
        if is_active {
            canvas.fill(*tab, theme.background.raw());
            canvas.fill(
                Rect::new(tab.x, tab.y, tab.width, 2),
                theme.focus_ring.raw(),
            );
        } else if is_hovered {
            canvas.fill(*tab, theme.button_hover.raw());
        }

        let text_style = sans(if is_active {
            theme.text_primary.raw()
        } else {
            theme.label_text.raw()
        });
        let available = tab.width.saturating_sub(PAD * 2 + TAB_CLOSE_W);
        let text = elide(label, available, text_style);
        canvas.text_in(tab.x + PAD as i32, *tab, &text, text_style);

        let close = tab_close_rect(*tab);
        let close_hovered = is_hovered && hovered_close;
        let (glyph, ink) = if close_hovered {
            ("×", theme.text_primary.raw())
        } else if dirty[index] {
            ("\u{2022}", theme.editor_change.raw())
        } else {
            ("×", theme.text_placeholder.raw())
        };
        let glyph_style = close_glyph(ink);
        let glyph_w = text::width(glyph, glyph_style) as i32;
        canvas.text_in(
            close.x + (close.width as i32 - glyph_w) / 2,
            close,
            glyph,
            glyph_style,
        );
    }
}

/// Draw the sidebar: the panel, the root's name, and one row per node
/// currently visible in `tree`. `open_path`, when it names a row, gets the
/// `list_selected` fill the way a listing marks the selected row.
pub fn draw_sidebar(
    canvas: &mut Surface,
    rect: Rect,
    root_name: &str,
    tree: &Tree,
    hovered: Option<usize>,
    open_path: Option<&str>,
) {
    let theme = &Theme::DEFAULT;
    canvas.fill(rect, theme.input_bg.raw());
    canvas.fill(
        Rect::new(rect.x + rect.width as i32 - 1, rect.y, 1, rect.height),
        theme.input_border.raw(),
    );

    let header_style = sans_strong(theme.text_primary.raw());
    let name = elide(root_name, rect.width.saturating_sub(PAD * 2), header_style);
    canvas.text(
        rect.x + PAD as i32,
        rect.y + PAD as i32,
        &name,
        header_style,
    );

    for (index, node) in tree.rows.iter().enumerate().skip(tree.scroll) {
        let row = tree_row_rect(rect, index, tree.scroll);
        if row.y + row.height as i32 > rect.y + rect.height as i32 {
            break;
        }

        if open_path == Some(node.path.as_str()) {
            canvas.fill(row, theme.list_selected.raw());
            canvas.fill(
                Rect::new(row.x, row.y, 2, row.height),
                theme.focus_ring.raw(),
            );
        } else if hovered == Some(index) {
            canvas.fill(row, theme.button_hover.raw());
        }

        let glyph_x = row.x + PAD as i32 + (node.depth as u32 * TREE_INDENT) as i32;
        let glyph_y = row.y + (row.height as i32 - icons::SIZE as i32) / 2;
        let ink = if node.is_dir {
            theme.entry_dir
        } else {
            theme.text_primary
        };
        // `icons::MINIMIZE` and `icons::CHEVRON_RIGHT` are already a
        // down-pointing and a right-pointing chevron; the shell's Sans face
        // carries no triangle glyph to draw the expand/collapse marker as
        // text, so the tree draws it as the icon it already is.
        if node.is_dir {
            let chevron = if node.expanded {
                &icons::MINIMIZE
            } else {
                &icons::CHEVRON_RIGHT
            };
            canvas.icon(glyph_x, glyph_y, chevron, ink.raw());
        } else {
            canvas.icon(glyph_x, glyph_y, &icons::DOCUMENT, ink.raw());
        }

        let name_x = glyph_x + icons::SIZE as i32 + space(2) as i32;
        let style = sans(ink.raw());
        let room = (rect.x + rect.width as i32 - PAD as i32 - name_x).max(0) as u32;
        let name = elide(&node.name, room, style);
        canvas.text_in(name_x, row, &name, style);
    }
}

/// Draw the editor pane: background, current-line highlight, gutter, text,
/// indent guides and caret. One `text::draw` per non-space character, at
/// `text_x + col * char_width()`, so the pen advance matches every rectangle
/// in this file instead of the face's true fractional advance. Each cell's
/// colour comes from the token covering it; only the colour, never the
/// geometry, moves.
pub fn draw_pane(
    canvas: &mut Surface,
    layout: &Layout,
    buffer: &mut Buffer,
    find_matches: &[(Position, usize)],
    find_current: Option<usize>,
) {
    let theme = &Theme::DEFAULT;
    canvas.fill(layout.pane, theme.background.raw());

    let visible_rows = layout.rows_visible.max(1);
    let visible_cols = layout.cols_visible.max(1);
    let selection = buffer.selection_range();
    let guide_step = syntax::indent_guide_step(buffer.lang);

    for row in 0..visible_rows {
        let line_index = buffer.scroll_line + row;
        if line_index >= buffer.lines.len() {
            break;
        }
        let tokens = buffer.tokens_for(line_index).to_vec();
        let line = &buffer.lines[line_index];
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

        // Indent guides: one hairline per whole step of the line's own
        // leading whitespace, ending right where its text begins.
        {
            let lead = line.text.chars().take_while(|c| c.is_whitespace()).count();
            for level in 1..=lead.checked_div(guide_step).unwrap_or(0) {
                let col = level * guide_step;
                if col < buffer.scroll_col || col >= buffer.scroll_col + visible_cols {
                    continue;
                }
                let x = layout.text_x + ((col - buffer.scroll_col) as u32 * char_width()) as i32;
                canvas.fill(
                    Rect::new(x, y, 1, layout.line_h),
                    theme.editor_indent_guide.raw(),
                );
            }
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

        // Selection fill, clipped to the visible columns, behind the glyphs
        // it sits under.
        if let Some((from, to)) = selection
            && line_index >= from.line
            && line_index <= to.line
        {
            let line_len = line.text.chars().count();
            let start_col = if line_index == from.line { from.col } else { 0 };
            let end_col = if line_index == to.line {
                to.col
            } else {
                line_len
            };
            let start_col = start_col.max(buffer.scroll_col);
            let end_col = end_col.min(buffer.scroll_col + visible_cols);
            if end_col > start_col {
                let rect = cell_rect(
                    layout,
                    buffer.scroll_line,
                    buffer.scroll_col,
                    line_index,
                    start_col,
                );
                let width = (end_col - start_col) as u32 * char_width();
                canvas.fill(
                    Rect::new(rect.x, rect.y, width, layout.line_h),
                    theme.editor_selection.raw(),
                );
            }
        }

        // Find matches on this line, clipped to the visible columns: every
        // one gets `editor_selection`, and the one the field has walked to
        // gets `focus_ring` instead, so it reads apart from the rest.
        for (index, &(pos, match_len)) in find_matches.iter().enumerate() {
            if pos.line != line_index {
                continue;
            }
            let start_col = pos.col.max(buffer.scroll_col);
            let end_col = (pos.col + match_len).min(buffer.scroll_col + visible_cols);
            if end_col <= start_col {
                continue;
            }
            let rect = cell_rect(
                layout,
                buffer.scroll_line,
                buffer.scroll_col,
                line_index,
                start_col,
            );
            let width = (end_col - start_col) as u32 * char_width();
            let color = if Some(index) == find_current {
                theme.focus_ring.raw()
            } else {
                theme.editor_selection.raw()
            };
            canvas.fill(Rect::new(rect.x, rect.y, width, layout.line_h), color);
        }

        for (col, ch) in line
            .text
            .chars()
            .enumerate()
            .skip(buffer.scroll_col)
            .take(visible_cols)
        {
            if ch.is_whitespace() {
                continue;
            }
            let kind = tokens
                .iter()
                .find(|t| col >= t.start && col < t.start + t.len)
                .map_or(TokenKind::Text, |t| t.kind);
            let rect = cell_rect(
                layout,
                buffer.scroll_line,
                buffer.scroll_col,
                line_index,
                col,
            );
            canvas.text(rect.x, rect.y, &ch.to_string(), mono(syntax::color(kind)));
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
    canvas: &mut Surface,
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

/// Draw the shared prompt bar: the fill, the leading label (`Find` / `Line` /
/// `Open`) and the right-aligned report. The field between them is a real
/// `TextInput` and draws itself, through `WidgetContainer::draw_all`. Read
/// as a count rather than an error even when it names a failure — the bar
/// has no warning colour of its own — so it draws in `label_text`.
pub fn draw_prompt(canvas: &mut Surface, rect: Rect, label: &str, report: &str) {
    let theme = &Theme::DEFAULT;
    canvas.fill(rect, theme.input_bg.raw());
    canvas.hline(rect.x, rect.y, rect.width, theme.input_border.raw());
    canvas.text_in(
        rect.x + PAD as i32,
        rect,
        label,
        sans_strong(theme.text_primary.raw()),
    );
    canvas.text_right(
        rect.x + rect.width as i32 - PAD as i32,
        rect,
        report,
        sans_small(theme.label_text.raw()),
    );
}
