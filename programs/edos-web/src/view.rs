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

use crate::doc::{Block, BlockKind, Document, Marker, Run};

/// Margin between the page and the window edge.
pub const PAGE_PAD: u32 = space(4);
/// Indent one list nesting level adds.
const LIST_INDENT: u32 = space(6);
/// Indent a blockquote sits at.
const QUOTE_INDENT: u32 = space(4);
/// Width of the scrollbar drawn when the page is taller than the viewport.
pub const SCROLLBAR_W: u32 = space(2);

/// A run of text on one line, already positioned and styled.
pub struct Fragment {
    pub x: i32,
    pub width: u32,
    pub text: String,
    pub style: Style,
    /// Index into [`Layout::links`], for the colour and, later, the click.
    pub link: Option<usize>,
}

/// One laid-out line: the fragments sharing a baseline, or a horizontal rule.
pub struct Line {
    pub y: i32,
    pub height: u32,
    pub items: Vec<Fragment>,
    /// `hr`, which draws a hairline across the column instead of text.
    pub rule: bool,
}

/// A document laid out for one column width.
pub struct Layout {
    pub lines: Vec<Line>,
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
    /// True when no space separates this word from the one before it, which is
    /// what `<b>bold</b>face` gives.
    glued: bool,
}

impl Layout {
    /// Lay `document` out in a column `width` pixels wide.
    pub fn build(document: &Document, width: u32) -> Layout {
        let mut out = Layout {
            lines: Vec::new(),
            links: Vec::new(),
            height: 0,
            width,
        };
        let column = width.saturating_sub(PAGE_PAD * 2).max(1);
        let mut y = PAGE_PAD as i32;

        for block in &document.blocks {
            let plan = plan(block);
            y += plan.gap_before as i32;

            if block.kind == BlockKind::Rule {
                let height = space(1);
                out.lines.push(Line {
                    y,
                    height,
                    items: Vec::new(),
                    rule: true,
                });
                y += height as i32 + plan.gap_after as i32;
                continue;
            }

            let avail = column.saturating_sub(plan.indent).max(1);
            let marker = plan.marker.as_ref().map(|text| Fragment {
                x: 0,
                width: text::width(text, plan.style),
                text: text.clone(),
                style: plan.style,
                link: None,
            });

            let start = out.lines.len();
            if plan.preformatted {
                out.preformatted(block, &plan, &mut y);
            } else {
                let words = out.words(&block.runs, plan.style);
                out.flow(words, &plan, avail, &mut y);
            }

            // The marker hangs in the left margin of the first line, which is
            // what makes a wrapped list item's continuation align under its
            // text rather than under its bullet.
            if let (Some(marker), Some(line)) = (marker, out.lines.get_mut(start)) {
                let x = (PAGE_PAD + plan.indent).saturating_sub(marker.width) as i32;
                line.items.insert(0, Fragment { x, ..marker });
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

    /// Split the runs into words, recording each link target once.
    fn words(&mut self, runs: &[Run], base: Style) -> Vec<Word> {
        let mut words = Vec::new();
        // The space between two links is its own run, carrying neither link, so
        // whether a word is glued to the one before it cannot be read off the
        // run it belongs to alone.
        let mut space_pending = false;
        for run in runs {
            let style = run_style(run, base);
            let link = run.link.as_ref().map(|target| {
                match self.links.iter().position(|existing| existing == target) {
                    Some(index) => index,
                    None => {
                        self.links.push(target.clone());
                        self.links.len() - 1
                    }
                }
            });
            let leading = run.text.starts_with(char::is_whitespace);
            let mut first = true;
            for word in run.text.split_whitespace() {
                words.push(Word {
                    text: word.to_string(),
                    style,
                    link,
                    glued: first && !leading && !space_pending,
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
        let line_height = text::line_height(plan.style);
        let mut items: Vec<Fragment> = Vec::new();
        let mut pen = 0u32;

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
                self.push_line(std::mem::take(&mut items), line_height, y);
                pen = 0;
            } else {
                pen += gap;
            }
            items.push(Fragment {
                x: (plan.indent + PAGE_PAD + pen) as i32,
                width,
                text: word.text,
                style: word.style,
                link: word.link,
            });
            pen += width;
        }
        if !items.is_empty() {
            self.push_line(items, line_height, y);
        }
    }

    /// `pre`, whose newlines are the layout and whose overflow is clipped
    /// rather than wrapped, the way `white-space: pre` behaves.
    fn preformatted(&mut self, block: &Block, plan: &Plan, y: &mut i32) {
        let line_height = text::line_height(plan.style);
        for line in block.text().lines() {
            let items = if line.trim().is_empty() {
                Vec::new()
            } else {
                vec![Fragment {
                    x: (plan.indent + PAGE_PAD) as i32,
                    width: text::width(line, plan.style),
                    text: line.to_string(),
                    style: plan.style,
                    link: None,
                }]
            };
            self.push_line(items, line_height, y);
        }
    }

    fn push_line(&mut self, items: Vec<Fragment>, height: u32, y: &mut i32) {
        self.lines.push(Line {
            y: *y,
            height,
            items,
            rule: false,
        });
        *y += height as i32;
    }
}

/// How one block is set: its base style, where it sits, and what it wears.
struct Plan {
    style: Style,
    indent: u32,
    gap_before: u32,
    gap_after: u32,
    marker: Option<String>,
    preformatted: bool,
}

fn plan(block: &Block) -> Plan {
    let text_color = Theme::DEFAULT.text_primary.raw();
    let base = Plan {
        style: Style::new(text_color),
        indent: 0,
        gap_before: 0,
        gap_after: space(2),
        marker: None,
        preformatted: false,
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
        BlockKind::Rule | BlockKind::Paragraph => Plan {
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

    for line in &layout.lines {
        let y = line.y - scroll as i32 + top as i32;
        if y + line.height as i32 <= top as i32 || y >= height as i32 {
            continue;
        }
        if line.rule {
            fill(
                surface.pixels,
                width,
                height,
                0,
                y + line.height as i32 / 2,
                width.saturating_sub(PAGE_PAD * 2).max(1),
                1,
                rule_color,
                PAGE_PAD as i32,
            );
            continue;
        }
        for item in &line.items {
            text::draw(&mut surface, item.x, y, &item.text, item.style);
            if item.link.is_some() {
                // Underlined, so a link is not carried by colour alone.
                let underline = y + line.height as i32 - 3;
                fill(
                    surface.pixels,
                    width,
                    height,
                    item.x,
                    underline,
                    item.width,
                    1,
                    item.style.color,
                    0,
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
        track_x,
        top as i32,
        SCROLLBAR_W,
        view_h,
        Theme::DEFAULT.slider_track.raw(),
        0,
    );
    let thumb_h = (view_h as u64 * view_h as u64 / content_h as u64).max(space(4) as u64) as u32;
    let span = content_h.saturating_sub(view_h).max(1);
    let offset =
        (scroll.min(span) as u64 * view_h.saturating_sub(thumb_h) as u64 / span as u64) as u32;
    fill(
        buffer,
        width,
        height,
        track_x,
        (top + offset) as i32,
        SCROLLBAR_W,
        thumb_h,
        Theme::DEFAULT.slider_thumb.raw(),
        0,
    );
}

/// Fill a rectangle, clipped to the buffer. `x_offset` shifts the whole rect,
/// which is how a full-width rule keeps the page's own margin.
#[allow(clippy::too_many_arguments)]
fn fill(
    buffer: &mut [u32],
    width: u32,
    height: u32,
    x: i32,
    y: i32,
    w: u32,
    h: u32,
    color: u32,
    x_offset: i32,
) {
    for row in 0..h as i32 {
        let py = y + row;
        if py < 0 || py >= height as i32 {
            continue;
        }
        for col in 0..w as i32 {
            let px = x + col + x_offset;
            if px < 0 || px >= width as i32 {
                continue;
            }
            buffer[py as usize * width as usize + px as usize] = color;
        }
    }
}
