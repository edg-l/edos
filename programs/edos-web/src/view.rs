//! Laying the block list out for a window, and drawing it.
//!
//! Layout is one pass over the blocks producing positioned lines, kept until
//! the window width changes. A line holds fragments rather than a string
//! because emphasis, code and links change appearance partway through a line,
//! and every one of them is measured with the same proportional metrics the
//! blitter draws with -- a character count times a cell width would put a
//! link's underline in the wrong place at the first non-ASCII glyph.

use edos_render::font::{Family, Weight, size};
use edos_render::metrics::space;
use edos_render::text::{self, Style, Surface};
use edos_render::theme::Theme;

use crate::css::{self, Align, Sides};
use crate::doc::{Block, BlockKind, Document, Marker, Picture, Run};

/// Margin between the page and the window edge.
pub const PAGE_PAD: u32 = space(4);
/// Indent one list nesting level adds.
const LIST_INDENT: u32 = space(6);
/// Indent a blockquote sits at.
const QUOTE_INDENT: u32 = space(4);
/// Width of the scrollbar drawn when the page is taller than the viewport.
pub const SCROLLBAR_W: u32 = space(2);
/// Tallest an image is drawn. A page's hero picture is often taller than the
/// window, and one that has to be scrolled past to reach the first paragraph
/// reads as a page that failed to load.
const MAX_IMAGE_H: u32 = space(60);

/// A run of text on one line, already positioned and styled.
pub struct Fragment {
    pub x: i32,
    pub width: u32,
    pub text: String,
    pub style: Style,
    /// Index into [`Layout::links`], for the colour and the click.
    pub link: Option<usize>,
    /// Whether a rule is drawn under the text, which is a link's default and
    /// what `text-decoration` overrides.
    pub underline: bool,
}

/// What a line draws.
pub enum LineKind {
    Text,
    /// `hr`, a hairline across the box it was laid out in, which is the column
    /// unless the page narrowed it.
    Rule {
        x: i32,
        width: u32,
    },
    /// A picture rasterised for this column: the pixels and the size they were
    /// rendered at, which is also the line's own size.
    Image {
        pixels: Vec<u32>,
        width: u32,
    },
}

/// One laid-out line: the fragments sharing a baseline, a horizontal rule, or
/// an image.
///
/// An image still carries `items`, holding one empty fragment over its box when
/// it is inside a link, so the hit test needs to know nothing about pictures.
pub struct Line {
    pub y: i32,
    pub height: u32,
    /// How far below the line's top the glyph boxes sit. CSS splits the
    /// difference between the leading and the face's own height above and
    /// below the text, so a page that asks for open leading gets it around
    /// its lines rather than under them.
    pub lead: u32,
    pub items: Vec<Fragment>,
    pub kind: LineKind,
}

/// One edge of a box as it is painted: its thickness and its resolved colour.
#[derive(Clone, Copy, Default)]
pub struct Edge {
    pub px: u32,
    pub color: u32,
}

/// The box a block paints behind and around its own lines.
///
/// It is a separate list rather than a field on a line because it spans every
/// line the block produced, and because it is painted first: a background is
/// under the text of its own block, and blocks do not overlap.
pub struct Decor {
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
    pub background: Option<u32>,
    pub border: Sides<Edge>,
}

/// A document laid out for one column width.
pub struct Layout {
    pub lines: Vec<Line>,
    /// The boxes to paint before the lines, in document order.
    pub decor: Vec<Decor>,
    /// Every link target on the page, in document order.
    pub links: Vec<String>,
    /// Total page height, which is what scrolling is bounded by.
    pub height: u32,
    /// The width this was laid out for, so a redraw knows when it is stale.
    pub width: u32,
}

/// A word to place, carrying the appearance it was written in.
struct Word {
    text: String,
    style: Style,
    link: Option<usize>,
    underline: bool,
    /// The leading this word asks for: the page's `line-height` where it set
    /// one, the face's own height otherwise.
    height: u32,
    /// True when no space separates this word from the one before it, which is
    /// what `<b>bold</b>face` gives.
    glued: bool,
}

impl Layout {
    /// Lay `document` out in a column `width` pixels wide.
    pub fn build(document: &Document, width: u32) -> Layout {
        let mut out = Layout {
            lines: Vec::new(),
            decor: Vec::new(),
            links: Vec::new(),
            height: 0,
            width,
        };
        let column = width.saturating_sub(PAGE_PAD * 2).max(1);
        let mut y = PAGE_PAD as i32;

        for block in &document.blocks {
            let mut plan = plan(block);
            y += plan.gap_before as i32;

            // The measure the page asked for, never wider than the column it
            // sits in: there is no horizontal scroll, so the window is the
            // outer bound whatever the document says.
            let mut box_w = column.saturating_sub(plan.indent).max(1);
            if let Some(measure) = plan.measure {
                box_w = box_w.min(measure).max(1);
            }
            // `margin: 0 auto` centres the box in what is left of the column.
            if plan.center {
                plan.indent += column.saturating_sub(plan.indent + box_w) / 2;
            }

            // The measure bounds the border box, the way `box-sizing:
            // border-box` behaves: a padded box sized to the column would
            // otherwise run past the edge it was told to stop at.
            let box_x = (PAGE_PAD + plan.indent) as i32;
            let box_top = y;
            let left = plan.border.left.px + plan.pad.left;
            let right = plan.border.right.px + plan.pad.right;
            let avail = box_w.saturating_sub(left + right).max(1);
            // Everything below places content at `PAGE_PAD + indent`, so the
            // inset the box wears is folded into the indent once here.
            plan.indent += left;
            y += (plan.border.top.px + plan.pad.top) as i32;

            let picture_drawn = match &block.picture {
                // A picture that rasterises is the block; one that does not
                // falls through to the alt text it carries, which is what the
                // block would have been had the fetch failed.
                Some(picture) => out.picture(picture, block, &plan, avail, &mut y),
                None => false,
            };
            if block.kind == BlockKind::Rule {
                let height = space(1);
                out.lines.push(Line {
                    y,
                    height,
                    lead: 0,
                    items: Vec::new(),
                    kind: LineKind::Rule {
                        x: box_x + left as i32,
                        width: avail,
                    },
                });
                y += height as i32;
            } else if !picture_drawn {
                let marker = plan.marker.as_ref().map(|text| Fragment {
                    x: 0,
                    width: text::width(text, plan.style),
                    text: text.clone(),
                    style: plan.style,
                    link: None,
                    underline: false,
                });

                let start = out.lines.len();
                if plan.preformatted {
                    out.preformatted(block, &plan, avail, &mut y);
                } else {
                    let words = out.words(&block.runs, plan.style);
                    out.flow(words, &plan, avail, &mut y);
                }

                // The marker hangs in the left margin of the first line, which
                // is what makes a wrapped list item's continuation align under
                // its text rather than under its bullet.
                if let (Some(marker), Some(line)) = (marker, out.lines.get_mut(start)) {
                    let x = (PAGE_PAD + plan.indent).saturating_sub(marker.width) as i32;
                    line.items.insert(0, Fragment { x, ..marker });
                }
            }

            y += (plan.pad.bottom + plan.border.bottom.px) as i32;
            let edges = [
                plan.border.top,
                plan.border.right,
                plan.border.bottom,
                plan.border.left,
            ];
            if plan.background.is_some() || edges.iter().any(|edge| edge.px > 0) {
                out.decor.push(Decor {
                    x: box_x,
                    y: box_top,
                    width: box_w,
                    height: (y - box_top).max(0) as u32,
                    background: plan.background,
                    border: plan.border,
                });
            }
            y += plan.gap_after as i32;
        }

        out.height = (y + PAGE_PAD as i32).max(0) as u32;
        out
    }

    /// The link target at page coordinates, if a link's text is under them.
    ///
    /// Page space is window space with the chrome removed and the scroll added
    /// back, so a hit test uses the same fragment rectangles the draw pass
    /// measured -- which is why a proportional face needs no separate metric.
    pub fn link_at(&self, x: i32, y: i32) -> Option<&str> {
        let line = self
            .lines
            .iter()
            .find(|line| y >= line.y && y < line.y + line.height as i32)?;
        let item = line
            .items
            .iter()
            .find(|item| x >= item.x && x < item.x + item.width as i32)?;
        self.links.get(item.link?).map(String::as_str)
    }

    /// Place a decoded picture as one line, scaled to fit the column. Returns
    /// false when it cannot be rasterised at all.
    fn picture(
        &mut self,
        picture: &Picture,
        block: &Block,
        plan: &Plan,
        avail: u32,
        y: &mut i32,
    ) -> bool {
        let (own_w, own_h) = picture.intrinsic_size();
        // Shrunk to the column but never enlarged past its own size: a picture
        // blown up to the measure of the text is detail a raster has not got,
        // and it is not the size the page asked for either.
        let mut width = own_w.min(avail).max(1);
        let mut height = (own_h as u64 * width as u64 / own_w as u64).max(1) as u32;
        if height > MAX_IMAGE_H {
            height = MAX_IMAGE_H;
            width = ((own_w as u64 * height as u64 / own_h as u64).max(1) as u32)
                .min(avail)
                .max(1);
        }
        let Some(pixels) = picture.render(width, height, Theme::DEFAULT.background) else {
            return false;
        };

        // An image inside a link is the link, so it gets a fragment of its own
        // size carrying no text: the hit test measures fragments, and the draw
        // pass has nothing to write.
        let link = block
            .runs
            .first()
            .and_then(|run| run.link.clone())
            .map(|target| self.link_index(&target));
        // An image is an inline box, so `text-align` places it the way it
        // places a line of text: a centred figure is written that way far more
        // often than with margins.
        let items = vec![Fragment {
            x: (PAGE_PAD + plan.indent + align_offset(plan.align, avail, width)) as i32,
            width,
            text: String::new(),
            style: plan.style,
            link,
            underline: false,
        }];
        self.lines.push(Line {
            y: *y,
            height,
            lead: 0,
            items,
            kind: LineKind::Image { pixels, width },
        });
        *y += height as i32;
        true
    }

    /// The index of a link target, adding it in document order on first sight.
    fn link_index(&mut self, target: &str) -> usize {
        match self.links.iter().position(|existing| existing == target) {
            Some(index) => index,
            None => {
                self.links.push(target.to_string());
                self.links.len() - 1
            }
        }
    }

    /// Split the runs into words, recording each link target once.
    fn words(&mut self, runs: &[Run], base: Style) -> Vec<Word> {
        let mut words = Vec::new();
        // The space between two links is its own run, carrying neither link, so
        // whether a word is glued to the one before it cannot be read off the
        // run it belongs to alone.
        let mut space_pending = false;
        for run in runs {
            let style = run_style(run, base);
            let link = run.link.as_ref().map(|target| self.link_index(target));
            // A link is underlined unless the document says otherwise, so
            // emphasis is not carried by colour alone.
            let underline = run.css.underline.unwrap_or(link.is_some());
            let height = leading(run.css.line, style);
            let space_before = run.text.starts_with(char::is_whitespace);
            let mut first = true;
            for word in run.text.split_whitespace() {
                words.push(Word {
                    text: word.to_string(),
                    style,
                    link,
                    underline,
                    height,
                    glued: first && !space_before && !space_pending,
                });
                first = false;
            }
            // `first` still set means the run was whitespace only, which is
            // exactly the separator case.
            space_pending =
                !run.text.is_empty() && (first || run.text.ends_with(char::is_whitespace));
        }
        words
    }

    /// Greedy wrap: place words until one does not fit, then start a line.
    fn flow(&mut self, words: Vec<Word>, plan: &Plan, avail: u32, y: &mut i32) {
        let base_height = leading(plan.line, plan.style);
        let mut items: Vec<Fragment> = Vec::new();
        let mut pen = 0u32;
        // The tallest word on the line sets its height. CSS can put a size on
        // a span, and a line measured from the block's own style alone would
        // then overlap the one above it.
        let mut line_height = base_height;

        for word in words {
            let width = text::width(&word.text, word.style);
            let gap = if items.is_empty() || word.glued {
                0
            } else {
                text::width(" ", word.style)
            };
            // A word wider than the column gets its own line rather than being
            // cut: a URL broken across lines is worse than a ragged edge.
            if !items.is_empty() && pen + gap + width > avail {
                let line = std::mem::take(&mut items);
                self.push_aligned(line, line_height, plan, avail, pen, y);
                line_height = base_height;
                pen = 0;
            } else {
                pen += gap;
            }
            line_height = line_height.max(word.height);
            items.push(Fragment {
                x: (plan.indent + PAGE_PAD + pen) as i32,
                width,
                text: word.text,
                style: word.style,
                link: word.link,
                underline: word.underline,
            });
            pen += width;
        }
        if !items.is_empty() {
            self.push_aligned(items, line_height, plan, avail, pen, y);
        }
    }

    /// `pre`, whose newlines are the layout and whose overflow is clipped
    /// rather than wrapped, the way `white-space: pre` behaves.
    fn preformatted(&mut self, block: &Block, plan: &Plan, avail: u32, y: &mut i32) {
        let line_height = leading(plan.line, plan.style);
        for line in block.text().lines() {
            let mut used = 0;
            let items = if line.trim().is_empty() {
                Vec::new()
            } else {
                used = text::width(line, plan.style);
                vec![Fragment {
                    x: (plan.indent + PAGE_PAD) as i32,
                    width: used,
                    text: line.to_string(),
                    style: plan.style,
                    link: None,
                    underline: false,
                }]
            };
            self.push_aligned(items, line_height, plan, avail, used, y);
        }
    }

    /// Push a line with its fragments moved to where `text-align` puts them.
    /// A line is measured left-aligned first, so the shift is one offset
    /// applied to every fragment rather than a second pass over the words.
    fn push_aligned(
        &mut self,
        mut items: Vec<Fragment>,
        height: u32,
        plan: &Plan,
        avail: u32,
        used: u32,
        y: &mut i32,
    ) {
        let shift = align_offset(plan.align, avail, used) as i32;
        for item in &mut items {
            item.x += shift;
        }
        self.push_line(items, height, y);
    }

    fn push_line(&mut self, items: Vec<Fragment>, height: u32, y: &mut i32) {
        // The half-leading is measured against the tallest face on the line,
        // so a line whose height came from the page's `line-height` centres
        // its text and a line that took the face's own height is unmoved.
        let natural = items
            .iter()
            .map(|item| text::line_height(item.style))
            .max()
            .unwrap_or(height);
        self.lines.push(Line {
            y: *y,
            height,
            lead: height.saturating_sub(natural) / 2,
            items,
            kind: LineKind::Text,
        });
        *y += height as i32;
    }
}

/// How one block is set: its base style, where it sits, and what it wears.
struct Plan {
    style: Style,
    /// The block's `line-height`, which its words inherit unless they set one.
    line: css::LineHeight,
    indent: u32,
    gap_before: u32,
    gap_after: u32,
    marker: Option<String>,
    preformatted: bool,
    /// The measure `width`/`max-width` asked for, already resolved to pixels
    /// by the cascade. `None` is the whole column.
    measure: Option<u32>,
    center: bool,
    align: Align,
    background: Option<u32>,
    pad: Sides<u32>,
    border: Sides<Edge>,
}

/// The height a line of `style` occupies: what the page asked for with
/// `line-height`, or the face's own metrics when it asked for nothing.
fn leading(line: css::LineHeight, style: Style) -> u32 {
    line.px(style.px)
        .unwrap_or_else(|| text::line_height(style))
}

/// How far into a box of `avail` pixels a line of `used` pixels starts.
fn align_offset(align: Align, avail: u32, used: u32) -> u32 {
    let slack = avail.saturating_sub(used);
    match align {
        Align::Left => 0,
        Align::Center => slack / 2,
        Align::Right => slack,
    }
}

fn plan(block: &Block) -> Plan {
    let mut plan = default_plan(block);
    // The document's own CSS wins over the plan the tag alone implies, but
    // only where it said something: a page that sets nothing keeps the reader
    // typography, which is the whole reason the defaults exist.
    let css = &block.css;
    if let Some(color) = css.color {
        plan.style.color = color;
    }
    if let Some(px) = css.font_px {
        plan.style.px = px;
    }
    if css.bold == Some(true) {
        plan.style.weight = Weight::Semibold;
    }
    if css.mono == Some(true) {
        plan.style.family = Family::Mono;
    }
    plan.line = css.line;
    if let Some(top) = css.margin_top {
        plan.gap_before = top;
    }
    if let Some(bottom) = css.margin_bottom {
        plan.gap_after = bottom;
    }
    plan.indent += css.margin_left.unwrap_or(0);
    plan.measure = css.measure;
    plan.center = css.center;
    plan.align = css.align;
    plan.background = css.background;
    plan.pad = Sides {
        top: css.padding.top.unwrap_or(0),
        right: css.padding.right.unwrap_or(0),
        bottom: css.padding.bottom.unwrap_or(0),
        left: css.padding.left.unwrap_or(0),
    };
    // A border written without a colour is `currentColor`, which is the text
    // colour the block ended up with rather than the one it inherited.
    let edge = |border: css::Border| Edge {
        px: border.px(),
        color: border.color.unwrap_or(plan.style.color),
    };
    plan.border = Sides {
        top: edge(css.borders.top),
        right: edge(css.borders.right),
        bottom: edge(css.borders.bottom),
        left: edge(css.borders.left),
    };
    plan
}

fn default_plan(block: &Block) -> Plan {
    let text_color = Theme::DEFAULT.text_primary.raw();
    let base = Plan {
        style: Style::new(text_color),
        line: css::LineHeight::Normal,
        indent: 0,
        gap_before: 0,
        gap_after: space(2),
        marker: None,
        preformatted: false,
        measure: None,
        center: false,
        align: Align::Left,
        background: None,
        pad: Sides::default(),
        border: Sides::default(),
    };
    match block.kind {
        BlockKind::Heading(level) => Plan {
            style: Style::new(text_color)
                .with_px(heading_px(level))
                .with_weight(Weight::Semibold),
            gap_before: space(if level <= 2 { 5 } else { 4 }),
            gap_after: space(2),
            ..base
        },
        BlockKind::ListItem { depth, marker } => Plan {
            indent: LIST_INDENT * (depth as u32 + 1),
            gap_after: space(1),
            marker: Some(match marker {
                Marker::Bullet => "\u{2022} ".to_string(),
                Marker::Number(n) => format!("{n}. "),
            }),
            ..base
        },
        BlockKind::Pre => Plan {
            style: Style::mono(Theme::DEFAULT.syn_string.raw()),
            indent: QUOTE_INDENT,
            gap_before: space(2),
            preformatted: true,
            ..base
        },
        BlockKind::Quote => Plan {
            style: Style::new(Theme::DEFAULT.label_text.raw()),
            indent: QUOTE_INDENT,
            ..base
        },
        BlockKind::Rule | BlockKind::Paragraph | BlockKind::Image => Plan {
            gap_before: space(1),
            ..base
        },
    }
}

/// The type scale for headings. `h5` and `h6` are body size set heavy, which
/// is what they are for: a label above a paragraph, not another rung.
fn heading_px(level: u8) -> u32 {
    match level {
        1 => size::BODY * 2,
        2 => size::BODY * 3 / 2,
        3 => size::BODY * 5 / 4,
        4 => size::BODY + 2,
        _ => size::BODY,
    }
}

fn run_style(run: &Run, base: Style) -> Style {
    let mut style = tag_style(run, base);
    let css = &run.css;
    if css.mono == Some(true) {
        style.family = Family::Mono;
    } else if css.mono == Some(false) {
        style.family = Family::Sans;
    }
    match css.bold {
        Some(true) => style.weight = Weight::Semibold,
        Some(false) => style.weight = Weight::Regular,
        None => {}
    }
    if let Some(px) = css.font_px {
        style.px = px;
    }
    // The theme has no italic face, so CSS italic borrows the accent colour on
    // the same terms `<em>` does, and only where the run is not already saying
    // something with weight or with a colour of its own.
    if css.italic == Some(true) && style.weight == Weight::Regular && css.color.is_none() {
        style.color = Theme::DEFAULT.title_accent.raw();
    }
    if let Some(color) = css.color {
        style.color = color;
    }
    style
}

fn tag_style(run: &Run, base: Style) -> Style {
    let mut style = base;
    if run.code {
        style.family = Family::Mono;
        style.color = Theme::DEFAULT.syn_string.raw();
    }
    if run.bold {
        style.weight = Weight::Semibold;
    }
    // The theme has no italic face, so emphasis without weight borrows the
    // accent colour rather than being silently dropped.
    if run.italic && !run.bold {
        style.color = Theme::DEFAULT.title_accent.raw();
    }
    if run.link.is_some() {
        style.color = Theme::DEFAULT.entry_link.raw();
    }
    style
}

/// Draw the page into `buffer`, clipped to the viewport, scrolled by `scroll`.
///
/// `top` is where the page area begins, below whatever chrome the caller drew.
pub fn draw(layout: &Layout, buffer: &mut [u32], width: u32, height: u32, top: u32, scroll: u32) {
    let view_h = height.saturating_sub(top);
    let mut surface = Surface::new(buffer, width, height);
    surface.clip = Some((0, top as i32, width as i32, height as i32));
    let rule_color = Theme::DEFAULT.window_border_highlight.raw();

    for decor in &layout.decor {
        let y = decor.y - scroll as i32 + top as i32;
        if y + decor.height as i32 <= top as i32 || y >= height as i32 {
            continue;
        }
        let (w, h) = (decor.width, decor.height);
        if let Some(color) = decor.background {
            fill(surface.pixels, width, height, top, decor.x, y, w, h, color);
        }
        // Borders sit inside the box, so a background and a border of the same
        // colour are one shape rather than two.
        let border = &decor.border;
        let sides = [
            (decor.x, y, w, border.top.px, border.top.color),
            (
                decor.x,
                y + h.saturating_sub(border.bottom.px) as i32,
                w,
                border.bottom.px,
                border.bottom.color,
            ),
            (decor.x, y, border.left.px, h, border.left.color),
            (
                decor.x + w.saturating_sub(border.right.px) as i32,
                y,
                border.right.px,
                h,
                border.right.color,
            ),
        ];
        for (x, y, w, h, color) in sides {
            if w > 0 && h > 0 {
                fill(surface.pixels, width, height, top, x, y, w, h, color);
            }
        }
    }

    for line in &layout.lines {
        let y = line.y - scroll as i32 + top as i32;
        if y + line.height as i32 <= top as i32 || y >= height as i32 {
            continue;
        }
        match &line.kind {
            LineKind::Rule { x, width: rule_w } => {
                fill(
                    surface.pixels,
                    width,
                    height,
                    top,
                    *x,
                    y + line.height as i32 / 2,
                    *rule_w,
                    1,
                    rule_color,
                );
                continue;
            }
            LineKind::Image {
                pixels,
                width: image_w,
            } => {
                let x = line.items.first().map_or(PAGE_PAD as i32, |item| item.x);
                blit(
                    surface.pixels,
                    width,
                    height,
                    top,
                    x,
                    y,
                    pixels,
                    *image_w,
                    line.height,
                );
                continue;
            }
            LineKind::Text => {}
        }
        for item in &line.items {
            text::draw(
                &mut surface,
                item.x,
                y + line.lead as i32,
                &item.text,
                item.style,
            );
            if item.underline {
                // Under the text, not under the line: open leading would
                // otherwise leave the rule floating below the words it marks.
                let underline = y + (line.height - line.lead) as i32 - 3;
                fill(
                    surface.pixels,
                    width,
                    height,
                    top,
                    item.x,
                    underline,
                    item.width,
                    1,
                    item.style.color,
                );
            }
        }
    }

    draw_scrollbar(buffer, width, height, top, view_h, layout.height, scroll);
}

/// The thumb, drawn only when there is something to scroll.
fn draw_scrollbar(
    buffer: &mut [u32],
    width: u32,
    height: u32,
    top: u32,
    view_h: u32,
    content_h: u32,
    scroll: u32,
) {
    if content_h <= view_h || view_h == 0 {
        return;
    }
    let track_x = width.saturating_sub(SCROLLBAR_W) as i32;
    fill(
        buffer,
        width,
        height,
        top,
        track_x,
        top as i32,
        SCROLLBAR_W,
        view_h,
        Theme::DEFAULT.slider_track.raw(),
    );
    let thumb_h = (view_h as u64 * view_h as u64 / content_h as u64).max(space(4) as u64) as u32;
    let span = content_h.saturating_sub(view_h).max(1);
    let offset =
        (scroll.min(span) as u64 * view_h.saturating_sub(thumb_h) as u64 / span as u64) as u32;
    fill(
        buffer,
        width,
        height,
        top,
        track_x,
        (top + offset) as i32,
        SCROLLBAR_W,
        thumb_h,
        Theme::DEFAULT.slider_thumb.raw(),
    );
}

/// Copy an image's pixels into the buffer, clipped to the page area: a picture
/// scrolled under the chrome is cut at `top`, not drawn over it.
#[allow(clippy::too_many_arguments)]
fn blit(
    buffer: &mut [u32],
    width: u32,
    height: u32,
    top: u32,
    x: i32,
    y: i32,
    pixels: &[u32],
    image_w: u32,
    image_h: u32,
) {
    for row in 0..image_h as i32 {
        let py = y + row;
        if py < top as i32 || py >= height as i32 {
            continue;
        }
        let src = row as usize * image_w as usize;
        for col in 0..image_w as i32 {
            let px = x + col;
            if px < 0 || px >= width as i32 {
                continue;
            }
            buffer[py as usize * width as usize + px as usize] = pixels[src + col as usize];
        }
    }
}

/// Fill a rectangle, clipped to the buffer and to the page area: a background
/// scrolled under the chrome is cut at `top` rather than painted over it.
#[allow(clippy::too_many_arguments)]
fn fill(
    buffer: &mut [u32],
    width: u32,
    height: u32,
    top: u32,
    x: i32,
    y: i32,
    w: u32,
    h: u32,
    color: u32,
) {
    for row in 0..h as i32 {
        let py = y + row;
        if py < top as i32 || py >= height as i32 {
            continue;
        }
        for col in 0..w as i32 {
            let px = x + col;
            if px < 0 || px >= width as i32 {
                continue;
            }
            buffer[py as usize * width as usize + px as usize] = color;
        }
    }
}
