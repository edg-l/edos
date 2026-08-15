//! Laying the block list out for a window, and drawing it.
//!
//! Layout is one pass over the blocks producing positioned lines, kept until
//! the window width changes. A line holds fragments rather than a string
//! because emphasis, code and links change appearance partway through a line,
//! and every one of them is measured with the same proportional metrics the
//! blitter draws with -- a character count times a cell width would put a
//! link's underline in the wrong place at the first non-ASCII glyph.

use std::collections::BTreeMap;
use std::sync::Arc;

use edos_render::font::{Family, Weight, size};
use edos_render::metrics::space;
use edos_render::text::{self, Style, Surface, fit_prefix};
use edos_render::theme::Theme;

use crate::css::{self, Align, Sides};
use crate::doc::{Block, BlockKind, Document, Node, Picture, Run};
use taffy::prelude::{auto, fr, length};
use taffy::{
    AlignContent as TaffyAlignContent, AlignItems as TaffyAlignItems, AvailableSpace,
    Display as TaffyDisplay, FlexDirection as TaffyFlexDirection, NodeId, Size as TaffySize,
    Style as TaffyStyle, TaffyError, TaffyTree,
};

/// Margin between the page and the window edge.
pub const PAGE_PAD: u32 = space(4);
/// Indent one list nesting level adds.
const LIST_INDENT: u32 = space(6);
/// Indent a blockquote sits at.
const QUOTE_INDENT: u32 = space(4);
/// Spaces a tab is set as where `white-space` keeps it. A real tab stop is
/// measured from the start of the line, which a word carrying its own leading
/// gap cannot see, so the width is fixed instead.
const TAB_SPACES: u32 = 4;
/// Width of the scrollbar drawn when the page is taller than the viewport.
pub const SCROLLBAR_W: u32 = space(2);
/// A column wide enough that nothing in a box wraps, so a trial layout in it
/// reports the width the content actually wants.
const MAX_CONTENT_PROBE: u32 = 1 << 16;

/// The padding and border on both sides of a box.
///
/// The content extent is measured as a line's own width, so neither inset is in
/// it. Leaving them out makes a padded box narrower than its contents by
/// exactly the padding it asked for, which shows as text wrapping inside a box
/// that looked wide enough.
fn horizontal_inset(node: &Node) -> u32 {
    match node {
        Node::Leaf(block) => {
            block.css.padding.left.unwrap_or(0)
                + block.css.padding.right.unwrap_or(0)
                + block.css.borders.left.px()
                + block.css.borders.right.px()
        }
        Node::Container { children, .. } => {
            children.iter().map(horizontal_inset).max().unwrap_or(0)
        }
    }
}

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
    /// The lines drawn across the text: a link's underline by default, and
    /// whatever `text-decoration` asks for where the page says.
    pub decoration: css::Decorations,
    /// `letter-spacing`, in pixels, added to every character's advance. It is
    /// on the fragment rather than on the style because a `Style` is the face
    /// the whole shell shares, and tracking is a property of this page's run.
    pub letter: i32,
    /// `vertical-align` as pixels off the shared baseline, positive raising the
    /// run. The baseline itself is [`Line::natural`], so a run set smaller than
    /// its neighbours sits on their feet before this moves it.
    pub shift: i32,
    /// `background-color` on the run itself, painted behind this fragment
    /// alone. A block's background is a [`Decor`] spanning all its lines; an
    /// inline one covers the text and nothing else, so it lives here.
    pub background: Option<u32>,
    /// `visibility: hidden` on the run this came from: the fragment keeps the
    /// space it was laid out in and paints nothing, and the hit test steps over
    /// it, since a hidden link is not a link the reader can reach.
    pub hidden: bool,
    /// An `inline-block`'s subtree. Its contents are merged into the page once
    /// the line it sits on has a `y`, which is the first moment the box's own
    /// origin is known.
    pub(crate) boxed: Option<BoxedRun>,
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
    /// The tallest face on the line. Every fragment is set on its bottom, so a
    /// smaller run shares the baseline instead of hanging from the line's top.
    pub natural: u32,
    pub items: Vec<Fragment>,
    pub kind: LineKind,
    /// `visibility: hidden` on the block: the line holds its place and draws
    /// nothing. Text lines carry the flag on their fragments instead, since a
    /// `visible` child inside a hidden block still paints its own words.
    pub hidden: bool,
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
    /// Where each `id` in the document ended up, so a link carrying a fragment
    /// is a scroll rather than a fetch of the page it is already on.
    pub anchors: BTreeMap<String, u32>,
    /// Total page height, which is what scrolling is bounded by.
    pub height: u32,
    /// The width this was laid out for, so a redraw knows when it is stale.
    pub width: u32,
    /// The left edge of the box being laid out, and the width available inside
    /// it. The box engine sets these per block; for a page laid out as one
    /// column they are the page padding and the column it leaves.
    origin_x: u32,
    column: u32,
}

/// A word to place, carrying the appearance it was written in.
struct Word {
    text: String,
    style: Style,
    link: Option<usize>,
    decoration: css::Decorations,
    /// The leading this word asks for: the page's `line-height` where it set
    /// one, the face's own height otherwise.
    height: u32,
    /// True when no space separates this word from the one before it, which is
    /// what `<b>bold</b>face` gives.
    glued: bool,
    /// How many spaces stand before this word. One in a collapsing box, however
    /// many the source wrote where `white-space` keeps them.
    spaces: u32,
    /// How many line breaks stand before this word: `<br>`, or a newline in a
    /// box whose `white-space` keeps them.
    breaks: u32,
    /// `letter-spacing` and `word-spacing` as the run wrote them. Both ride on
    /// the word because a `<span>` can set either partway through a line.
    letter: i32,
    word_gap: i32,
    /// `vertical-align`, resolved: what the page asked for, or what the element
    /// that opened the run asks for by being a `<sup>` or a `<sub>`.
    shift: i32,
    /// `background-color` the run set on itself, or the highlight a `<mark>`
    /// wears.
    background: Option<u32>,
    /// `visibility: hidden` on the run, which lays the word out and paints
    /// nothing where it stands.
    hidden: bool,
    /// An `inline-block`'s subtree, when this "word" is a box rather than text.
    /// It never splits and its size is its own, not its text's.
    boxed: Option<BoxedRun>,
}

/// An atomic inline box: the subtree, and the width it laid out at.
///
/// The height is not kept: it reaches the line through the word's own `height`,
/// which is what makes the line grow to hold the box.
#[derive(Clone)]
pub(crate) struct BoxedRun {
    node: Arc<Node>,
    width: u32,
}

impl Layout {
    /// Lay `document` out in a column `width` pixels wide.
    ///
    /// The box tree is arranged by `taffy`, which stacks and sizes the boxes;
    /// what goes *inside* a box is this file's own line breaker, reached
    /// through [`Layout::lay_block`] and handed to taffy as its measure
    /// function. A leaf is measured by laying it out into a scratch `Layout`
    /// and taking the height it ends at, so there is one line breaker rather
    /// than a measuring copy of it that can drift.
    pub fn build(document: &Document, width: u32) -> Layout {
        Layout::build_tree(&document.root, width, PAGE_PAD)
    }

    /// Lay a box tree out in `width` pixels, inset by `pad` on every side.
    ///
    /// The page is one of these with the page padding; an `inline-block` is one
    /// with none, laid out on its own and then merged into the line it sits in.
    fn build_tree(root_node: &Node, width: u32, pad: u32) -> Layout {
        let column = width.saturating_sub(pad * 2).max(1);
        let mut out = Layout {
            lines: Vec::new(),
            decor: Vec::new(),
            links: Vec::new(),
            anchors: BTreeMap::new(),
            height: 0,
            width,
            origin_x: pad,
            column,
        };

        let mut tree: TaffyTree<&Block> = TaffyTree::new();
        let Ok(root) = add_node(&mut tree, root_node) else {
            return out;
        };
        let space = TaffySize {
            width: AvailableSpace::Definite(column as f32),
            height: AvailableSpace::MaxContent,
        };
        let arranged =
            tree.compute_layout_with_measure(root, space, |known, avail, _id, ctx, _| {
                let Some(block) = ctx else {
                    return TaffySize::ZERO;
                };
                if let TaffySize {
                    width: Some(w),
                    height: Some(h),
                } = known
                {
                    return TaffySize {
                        width: w,
                        height: h,
                    };
                }
                // The width offered for the trial layout. `MinContent` asks
                // how narrow the block can be, so it is laid out in the
                // narrowest column that can hold anything at all.
                let offer = match avail.width {
                    AvailableSpace::Definite(w) if w > 0.0 => w as u32,
                    AvailableSpace::MinContent => 1,
                    _ => column,
                };
                let (content, height) = measure_block(block, offer);
                // **The width reported is the content's own, not the width it
                // was offered.** A flex item sized at whatever space happened
                // to be available claims the whole line and its siblings are
                // pushed onto rows of their own, which is a column wearing the
                // name of a row.
                TaffySize {
                    width: content as f32,
                    height: height as f32,
                }
            });
        if arranged.is_err() {
            return out;
        }

        out.emit(&tree, root, pad as i32, pad as i32);
        // As tall as the engine made the tree, plus the padding below it.
        // Taking it from the last leaf emitted would miss a box that a
        // container placed beside a taller sibling rather than after it.
        let arranged_h = tree.layout(root).map(|l| l.size.height).unwrap_or(0.0);
        out.height = (arranged_h as u32).saturating_add(pad * 2);
        out
    }

    /// Walk the arranged tree and lay each leaf out where taffy put it.
    ///
    /// Both axes come from the engine. Threading a running `y` down the walk
    /// instead would stack every box in document order, which is a block
    /// layout wearing the name of whatever the container asked for -- three
    /// flex items each on a row of their own.
    ///
    /// Taffy reports a location relative to the parent, so the walk carries the
    /// parent's absolute corner and adds to it.
    fn emit(&mut self, tree: &TaffyTree<&Block>, node: NodeId, x: i32, y: i32) {
        let Ok(layout) = tree.layout(node) else {
            return;
        };
        let x = x + layout.location.x as i32;
        let y = y + layout.location.y as i32;
        if let Some(block) = tree.get_node_context(node) {
            self.origin_x = x.max(0) as u32;
            self.column = (layout.size.width as u32).max(1);
            self.lay_block(block, y);
            return;
        }
        let Ok(children) = tree.children(node) else {
            return;
        };
        for child in children {
            self.emit(tree, child, x, y);
        }
    }

    /// Lay one block out at the origin and column the caller set, returning the
    /// `y` it ends at and the inset standing to the right of its content.
    ///
    /// The box engine calls this twice per block: once to learn how tall the
    /// block is at a given width, and once to emit it where that engine decided
    /// it goes. Measuring by laying out into a scratch [`Layout`] is what keeps
    /// one line breaker rather than a measuring copy of it that can drift.
    ///
    /// The trailing inset is returned because nothing in the emitted lines
    /// records it: content is placed from the box's left edge, so the padding,
    /// border and margin on that side leave no fragment behind to measure. A
    /// caller sizing the box from its content has to add it back.
    fn lay_block(&mut self, block: &Block, mut y: i32) -> (i32, u32) {
        let mut plan = plan(block);
        y += plan.gap_before as i32;
        // Recorded before the block is laid out, since a fragment names the
        // top of what it points at. The margin above it is not part of it.
        if let Some(anchor) = &block.anchor {
            self.anchors.insert(anchor.clone(), y.max(0) as u32);
        }

        // The measure the page asked for, never wider than the column it
        // sits in: there is no horizontal scroll, so the window is the
        // outer bound whatever the document says.
        // The measure and `margin: 0 auto` settle the container this block
        // sits in; `margin-right` then takes from the right of it, which is
        // what keeps a block with one from moving off the left edge its
        // centred neighbours share.
        let mut container = self.column.saturating_sub(plan.indent).max(1);
        if let Some(measure) = plan.measure {
            container = container.min(measure).max(1);
        }
        // css-sizing-3 §5.1: the floor wins over the ceiling, so it is
        // applied last and only the column bounds it.
        if let Some(min) = plan.min_width {
            container = container
                .max(min)
                .min(self.column.saturating_sub(plan.indent).max(1));
        }
        if plan.center {
            plan.indent += self.column.saturating_sub(plan.indent + container) / 2;
        }
        let box_w = container.saturating_sub(plan.trail).max(1);

        // The measure bounds the border box, the way `box-sizing:
        // border-box` behaves: a padded box sized to the column would
        // otherwise run past the edge it was told to stop at.
        let box_x = (self.origin_x + plan.indent) as i32;
        let box_top = y;
        let left = plan.border.left.px + plan.pad.left;
        let right = plan.border.right.px + plan.pad.right;
        let avail = box_w.saturating_sub(left + right).max(1);
        // Everything below places content at `origin_x + indent`, so the
        // inset the box wears is folded into the indent once here.
        plan.indent += left;
        y += (plan.border.top.px + plan.pad.top) as i32;

        let picture_drawn = match &block.picture {
            // A picture that rasterises is the block; one that does not
            // falls through to the alt text it carries, which is what the
            // block would have been had the fetch failed.
            Some(picture) => self.picture(picture, block, &plan, avail, &mut y),
            None => false,
        };
        if block.kind == BlockKind::Rule {
            let height = space(1);
            self.lines.push(Line {
                y,
                height,
                lead: 0,
                natural: height,
                items: Vec::new(),
                kind: LineKind::Rule {
                    x: box_x + left as i32,
                    width: avail,
                },
                hidden: plan.invisible,
            });
            y += height as i32;
        } else if !picture_drawn {
            let marker = plan.marker.as_ref().map(|text| Fragment {
                x: 0,
                width: text::width_tracked(text, plan.style, plan.letter),
                text: text.clone(),
                style: plan.style,
                link: None,
                decoration: css::Decorations::default(),
                letter: plan.letter,
                shift: 0,
                background: None,
                hidden: plan.invisible,
                boxed: None,
            });

            let start = self.lines.len();
            let words = self.words(&block.runs, &plan, avail);
            self.flow(words, &plan, avail, &mut y);

            // The marker hangs in the left margin of the first line, which
            // is what makes a wrapped list item's continuation align under
            // its text rather than under its bullet.
            if let (Some(marker), Some(line)) = (marker, self.lines.get_mut(start)) {
                let x = (self.origin_x + plan.indent).saturating_sub(marker.width) as i32;
                line.items.insert(0, Fragment { x, ..marker });
            }
        }

        y += (plan.pad.bottom + plan.border.bottom.px) as i32;

        // The declared height sizes the border box, the way the measure
        // sizes its width, and the clamps apply in the order css-sizing-3
        // §5.4 gives: the maximum first, then the minimum over it, so a
        // box asked for both keeps the floor. Taller content is left
        // overflowing rather than cut, since nothing here clips.
        let box_h = block_height(&plan, (y - box_top).max(0) as u32);
        y = box_top + box_h as i32;

        let edges = [
            plan.border.top,
            plan.border.right,
            plan.border.bottom,
            plan.border.left,
        ];
        if !plan.invisible && (plan.background.is_some() || edges.iter().any(|edge| edge.px > 0)) {
            self.decor.push(Decor {
                x: box_x,
                y: box_top,
                width: box_w,
                height: (y - box_top).max(0) as u32,
                background: plan.background,
                border: plan.border,
            });
        }
        y += plan.gap_after as i32;
        (y, right + plan.trail)
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
        let shown = || line.items.iter().filter(|item| !item.hidden);
        if let Some(item) = shown().find(|item| x >= item.x && x < item.x + item.width as i32) {
            return self.links.get(item.link?).map(String::as_str);
        }
        // The space between two words of one link belongs to it. A fragment is
        // a word, so without this a multi-word link has a dead gap between
        // every pair of words, and the reader -- who sees one underlined
        // phrase -- has clicked the link and had nothing happen.
        let before = shown()
            .filter(|item| item.x + item.width as i32 <= x)
            .next_back()?;
        let after = shown().find(|item| item.x > x)?;
        let link = before.link?;
        (after.link == Some(link))
            .then(|| self.links.get(link))
            .flatten()
            .map(String::as_str)
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
            x: (self.origin_x + plan.indent + align_offset(plan.align, avail, width)) as i32,
            width,
            text: String::new(),
            style: plan.style,
            link,
            decoration: css::Decorations::default(),
            letter: 0,
            shift: 0,
            background: None,
            hidden: plan.invisible,
            boxed: None,
        }];
        self.lines.push(Line {
            y: *y,
            height,
            lead: 0,
            natural: height,
            items,
            kind: LineKind::Image { pixels, width },
            hidden: plan.invisible,
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
    ///
    /// The separators are counted rather than thrown away, since a box whose
    /// `white-space` keeps them needs the source's own spacing back: the gap
    /// before a word and the breaks before it are carried on the word itself,
    /// which is also how a separator that falls on a run boundary survives.
    fn words(&mut self, runs: &[Run], plan: &Plan, avail: u32) -> Vec<Word> {
        let (base, ws) = (plan.style, plan.ws);
        let mut words = Vec::new();
        let mut spaces = 0;
        let mut breaks = 0;
        for run in runs {
            let style = run_style(run, base);
            let link = run.link.as_ref().map(|target| self.link_index(target));
            // A link is underlined unless the document says otherwise, so
            // emphasis is not carried by colour alone.
            let decoration = run.css.decoration.unwrap_or(css::Decorations {
                underline: link.is_some(),
                ..css::Decorations::default()
            });
            let height = leading(run.css.line, style);
            let letter = run.css.letter_spacing;
            let word_gap = run.css.word_spacing;
            // A script's rise is a fraction of the size it would have been set
            // at, not of the size it ends up at, so it is measured against the
            // base the run shrank from.
            let shift = run.css.shift.unwrap_or(run.script.shift(base.px));
            // The block's own background is painted once, behind every line it
            // produced. A run carrying that same colour is the block's own
            // text rather than a highlighted span inside it, so only a colour
            // the run set for itself is painted per word.
            let background = run.css.background.filter(|&c| Some(c) != plan.background);
            // An inline-block is laid out on its own, once, and then joins the
            // line as a single item that never splits. Its width is its
            // content's, bounded by the column, which is what makes it shrink
            // to fit the way the box model says.
            if let Some(node) = &run.boxed {
                // Shrink to fit: the box is as wide as its content wants, up to
                // the room left on the line. Its max-content width is taken
                // from a trial layout in a column nothing can wrap in; laying
                // it out at `avail` and measuring that would just report
                // `avail` back, since a line fills the column it is given.
                let probe = Layout::build_tree(node, MAX_CONTENT_PROBE, 0);
                // **Measured as each line's extent, not as its rightmost edge,
                // and not as the width the engine arranged the tree in.**
                // `text-align` is inherited, so a box inside a centred
                // paragraph has its fragments shifted half the probe column to
                // the right; reading `x + width` then measures the shift and
                // reports a box as wide as the column it was probed in. The
                // arranged root is no better -- a block box fills the column it
                // is given, so it reports the probe back.
                let intrinsic = probe
                    .lines
                    .iter()
                    .filter_map(|line| {
                        let left = line.items.iter().map(|i| i.x).min()?;
                        let right = line.items.iter().map(|i| i.x + i.width as i32).max()?;
                        Some((right - left).max(0) as u32)
                    })
                    .max()
                    .unwrap_or(0)
                    + horizontal_inset(node);
                let content = intrinsic.min(avail).max(1);
                let laid = Layout::build_tree(node, content, 0);
                // The separators standing before the box belong to it, the way
                // they do to a word. Leaving them on the running counters
                // instead drops the space between two adjacent boxes and then
                // hands it to whatever text follows them.
                let box_spaces = std::mem::take(&mut spaces);
                let box_breaks = std::mem::take(&mut breaks);
                words.push(Word {
                    text: String::new(),
                    style,
                    link,
                    decoration: css::Decorations::default(),
                    height: laid.height.max(height),
                    glued: box_spaces == 0 && box_breaks == 0 && !words.is_empty(),
                    spaces: box_spaces,
                    breaks: box_breaks,
                    letter: 0,
                    word_gap,
                    shift,
                    background: None,
                    hidden: run.css.invisible,
                    boxed: Some(BoxedRun {
                        node: Arc::clone(node),
                        width: content,
                    }),
                });
                continue;
            }
            let mut word = String::new();
            let mut flush = |word: &mut String, spaces: &mut u32, breaks: &mut u32| {
                if word.is_empty() {
                    return;
                }
                let glued = *spaces == 0 && *breaks == 0;
                // A run glued to a word already on the block continues it
                // rather than starting one, so `capitalize` leaves it alone:
                // the letter it would raise is in the middle of the word the
                // reader sees. The block's own first word is glued to nothing.
                let continues = glued && !words.is_empty();
                let text = match run.css.transform {
                    css::Transform::Capitalize if continues => std::mem::take(word),
                    transform => transform.apply(word).into_owned(),
                };
                words.push(Word {
                    text,
                    style,
                    link,
                    decoration,
                    height,
                    glued,
                    spaces: *spaces,
                    breaks: *breaks,
                    letter,
                    word_gap,
                    shift,
                    background,
                    hidden: run.css.invisible,
                    boxed: None,
                });
                word.clear();
                *spaces = 0;
                *breaks = 0;
            };
            for ch in run.text.chars() {
                match ch {
                    // Every newline that reaches here is a break: a collapsing
                    // box turned its own into spaces while parsing, so the ones
                    // left came from `<br>` or from a box that keeps them.
                    '\n' => {
                        flush(&mut word, &mut spaces, &mut breaks);
                        breaks += 1;
                        spaces = 0;
                    }
                    // A tab is set as a fixed run of spaces rather than to a
                    // tab stop: the stops would have to be measured from the
                    // start of the line, which a word carrying its own gap
                    // cannot see.
                    _ if ch.is_whitespace() => {
                        flush(&mut word, &mut spaces, &mut breaks);
                        let width = if ch == '\t' { TAB_SPACES } else { 1 };
                        spaces = if ws.keeps_spaces() { spaces + width } else { 1 };
                    }
                    _ => word.push(ch),
                }
            }
            flush(&mut word, &mut spaces, &mut breaks);
        }
        words
    }

    /// Greedy wrap: place words until one does not fit, then start a line.
    fn flow(&mut self, words: Vec<Word>, plan: &Plan, avail: u32, y: &mut i32) {
        let base_height = leading(plan.line, plan.style);
        let mut items: Vec<Fragment> = Vec::new();
        // The first line starts at the indent the page asked for, and the wrap
        // below resets the pen to zero for every line after it. An indent wider
        // than the column would leave nothing to set the line in, so it stops a
        // pixel short of one.
        let mut pen = plan.first_indent.min(avail.saturating_sub(1));
        // The tallest word on the line sets its height. CSS can put a size on
        // a span, and a line measured from the block's own style alone would
        // then overlap the one above it.
        let mut line_height = base_height;

        for word in words {
            // A break the source asked for ends the line wherever it stands,
            // and a run of them leaves empty lines behind, which is what a
            // blank line inside a `pre` block is.
            for _ in 0..word.breaks {
                let line = std::mem::take(&mut items);
                self.push_aligned(line, line_height, plan, avail, pen, y);
                line_height = base_height;
                pen = 0;
            }
            // A space at the head of a line is dropped where spaces collapse
            // and kept where they do not, since an indented `pre` line is
            // nothing but its leading spaces.
            // `word-spacing` widens the space itself, and `letter-spacing`
            // reaches it too: a space is a character, so a tracked line opens
            // between its words as well as inside them.
            let mut gap = if word.glued || (items.is_empty() && !plan.ws.keeps_spaces()) {
                0
            } else {
                let advance = text::width(" ", word.style) as i32 + word.letter + word.word_gap;
                word.spaces * advance.max(0) as u32
            };
            // The word is placed whole unless it does not fit, and what happens
            // then is either a cut inside it or a fresh line to try again on.
            // Each pass either shortens the text or empties the line, so the
            // loop cannot run twice without making progress.
            let mut text = word.text;
            loop {
                let width = match &word.boxed {
                    Some(b) => b.width,
                    None => text::width_tracked(&text, word.style, word.letter),
                };
                if plan.ws.wraps() && pen + gap + width > avail {
                    let room = avail.saturating_sub(pen + gap);
                    // Without a break property a word wider than the column
                    // gets its own line rather than being cut, since a URL
                    // broken across lines is worse than a ragged edge.
                    // A box is atomic: it moves to the next line whole rather
                    // than being cut through.
                    let cut = plan
                        .wrap
                        .breaks(width > avail)
                        .then(|| fit_prefix(&text, word.style, room, word.letter))
                        .flatten()
                        .filter(|_| word.boxed.is_none());
                    if let Some(cut) = cut {
                        pen += gap;
                        let head = text[..cut].to_string();
                        let head_width = text::width_tracked(&head, word.style, word.letter);
                        line_height = line_height.max(word.height);
                        items.push(Fragment {
                            x: (plan.indent + self.origin_x + pen) as i32,
                            width: head_width,
                            text: head,
                            style: word.style,
                            link: word.link,
                            decoration: word.decoration,
                            letter: word.letter,
                            shift: word.shift,
                            background: word.background,
                            hidden: word.hidden,
                            boxed: None,
                        });
                        pen += head_width;
                        let line = std::mem::take(&mut items);
                        self.push_aligned(line, line_height, plan, avail, pen, y);
                        line_height = base_height;
                        pen = 0;
                        // The tail continues the same word, so no gap precedes
                        // it however the source spaced the word itself.
                        gap = 0;
                        text = text[cut..].to_string();
                        continue;
                    }
                    if !items.is_empty() {
                        let line = std::mem::take(&mut items);
                        self.push_aligned(line, line_height, plan, avail, pen, y);
                        line_height = base_height;
                        pen = 0;
                        gap = 0;
                        continue;
                    }
                }
                pen += gap;
                line_height = line_height.max(word.height);
                items.push(Fragment {
                    x: (plan.indent + self.origin_x + pen) as i32,
                    width,
                    text,
                    style: word.style,
                    link: word.link,
                    decoration: word.decoration,
                    letter: word.letter,
                    shift: word.shift,
                    background: word.background,
                    hidden: word.hidden,
                    boxed: word.boxed.clone(),
                });
                pen += width;
                break;
            }
        }
        if !items.is_empty() {
            self.push_aligned(items, line_height, plan, avail, pen, y);
        }
    }

    /// Lay an `inline-block`'s subtree out and merge it into the page at
    /// `(x, y)`.
    ///
    /// The box is laid out in its own coordinates and then translated, rather
    /// than being laid out at its final position, because the engine that
    /// arranges it knows nothing about the line it will land in. Links are
    /// re-indexed on the way, since the box's indices are its own.
    fn place_box(&mut self, boxed: &BoxedRun, x: i32, y: i32) {
        let mut inner = Layout::build_tree(&boxed.node, boxed.width, 0);
        let link_base = self.links.len();
        self.links.append(&mut inner.links);
        for decor in &mut inner.decor {
            decor.x += x;
            decor.y += y;
        }
        for line in &mut inner.lines {
            line.y += y;
            for item in &mut line.items {
                item.x += x;
                if let Some(index) = item.link.as_mut() {
                    *index += link_base;
                }
            }
        }
        self.decor.append(&mut inner.decor);
        self.lines.append(&mut inner.lines);
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
        // A shifted run leaves the band the line's own faces occupy, so the
        // line grows by whatever the tallest rise and the deepest drop ask for
        // rather than letting a superscript print over the line above.
        let shifts = || items.iter().map(|item| item.shift);
        let rise = shifts().max().unwrap_or(0).max(0) as u32;
        let drop = shifts().min().unwrap_or(0).min(0).unsigned_abs();
        let height = height.max(natural + rise + drop);
        // A box's contents are placed now and not before: this is the first
        // moment its origin is known, since the line's `y` is settled here and
        // its `x` was settled when the fragment was pushed.
        let boxes: Vec<(i32, BoxedRun)> = items
            .iter()
            .filter_map(|item| item.boxed.clone().map(|b| (item.x, b)))
            .collect();
        let line_y = *y;
        self.lines.push(Line {
            y: line_y,
            height,
            lead: (height.saturating_sub(natural) / 2).max(rise),
            natural,
            items,
            kind: LineKind::Text,
            hidden: false,
        });
        for (x, boxed) in boxes {
            self.place_box(&boxed, x, line_y);
        }
        *y += height as i32;
    }
}

/// Lay `block` out in a column `offer` wide and report what it actually took.
///
/// The width is the content's own extent -- the furthest right edge any
/// fragment or box reached -- rather than the column it was offered, because
/// that is what intrinsic sizing means and what a flex or grid track is sized
/// from. Reporting the offer instead makes every item as wide as the space it
/// was shown, which is how a row collapses into a column.
///
/// **The width reported is the border box's, not the content's.** A leaf wears
/// its own padding, border and margin -- taffy is told about none of them -- so
/// a width that stopped at the last fragment names a box narrower than the one
/// that will be drawn. The engine then hands the leaf that width back, the
/// re-layout subtracts the insets from it a second time, and the content wraps
/// a word early inside a box it fits in.
fn measure_block(block: &Block, offer: u32) -> (u32, u32) {
    let mut scratch = Layout {
        lines: Vec::new(),
        decor: Vec::new(),
        links: Vec::new(),
        anchors: BTreeMap::new(),
        height: 0,
        width: offer,
        origin_x: 0,
        column: offer,
    };
    let (bottom, trailing) = scratch.lay_block(block, 0);
    let height = bottom.max(0) as u32;
    let text = scratch
        .lines
        .iter()
        .flat_map(|line| line.items.iter())
        .map(|item| item.x.max(0) as u32 + item.width)
        .max()
        .unwrap_or(0);
    let rules = scratch
        .lines
        .iter()
        .filter_map(|line| match line.kind {
            LineKind::Rule { x, width } => Some(x.max(0) as u32 + width),
            LineKind::Image { width, .. } => Some(width),
            LineKind::Text => None,
        })
        .max()
        .unwrap_or(0);
    // Deliberately not the decor. A block's background and border are drawn at
    // the width the box was *given*, so taking them as evidence of intrinsic
    // width answers "as wide as you offered" to every question, and a box that
    // should shrink to fit fills the line instead.
    (text.max(rules) + trailing, height)
}

/// Mirror the document's box tree into taffy's.
///
/// A leaf carries its [`Block`] as taffy's node context, which is how the
/// measure function knows what to lay out; a container carries none, which is
/// what tells the emit walk to recurse rather than to draw.
fn add_node<'a>(tree: &mut TaffyTree<&'a Block>, node: &'a Node) -> Result<NodeId, TaffyError> {
    match node {
        Node::Leaf(block) => tree.new_leaf_with_context(TaffyStyle::DEFAULT, block),
        Node::Container { css, children } => {
            let kids: Result<Vec<NodeId>, TaffyError> =
                children.iter().map(|child| add_node(tree, child)).collect();
            tree.new_with_children(container_style(css), &kids?)
        }
    }
}

/// The box properties of a container, as taffy takes them.
///
/// Only what arranges children is read here. Everything a *leaf* wears -- its
/// margins, padding, border and measure -- stays with `lay_block`, which drew
/// them before there was a box engine and still does; setting them here as well
/// would count them twice.
fn container_style(css: &css::Computed) -> TaffyStyle {
    let mut style = TaffyStyle {
        display: match css.display {
            Some(css::Display::Flex) => TaffyDisplay::Flex,
            Some(css::Display::Grid) => TaffyDisplay::Grid,
            _ => TaffyDisplay::Block,
        },
        ..TaffyStyle::DEFAULT
    };
    if let Some(dir) = css.flex_direction {
        style.flex_direction = match dir {
            css::FlexDirection::Row => TaffyFlexDirection::Row,
            css::FlexDirection::RowReverse => TaffyFlexDirection::RowReverse,
            css::FlexDirection::Column => TaffyFlexDirection::Column,
            css::FlexDirection::ColumnReverse => TaffyFlexDirection::ColumnReverse,
        };
    }
    if let Some(justify) = css.justify {
        style.justify_content = Some(match justify {
            css::Justify::Start => TaffyAlignContent::Start,
            css::Justify::End => TaffyAlignContent::End,
            css::Justify::Center => TaffyAlignContent::Center,
            css::Justify::SpaceBetween => TaffyAlignContent::SpaceBetween,
            css::Justify::SpaceAround => TaffyAlignContent::SpaceAround,
            css::Justify::SpaceEvenly => TaffyAlignContent::SpaceEvenly,
        });
    }
    if let Some(align) = css.align_items {
        style.align_items = Some(match align {
            css::AlignItems::Start => TaffyAlignItems::Start,
            css::AlignItems::End => TaffyAlignItems::End,
            css::AlignItems::Center => TaffyAlignItems::Center,
            css::AlignItems::Stretch => TaffyAlignItems::Stretch,
        });
    }
    if let Some(gap) = css.gap {
        style.gap = TaffySize {
            width: length(gap as f32),
            height: length(gap as f32),
        };
    }
    if let Some(tracks) = &css.grid_columns {
        style.grid_template_columns = tracks
            .as_slice()
            .iter()
            .map(|track| match track {
                css::Track::Px(px) => length(*px as f32),
                css::Track::Fr(f) => fr(*f),
                css::Track::Auto => auto(),
            })
            .collect();
    }
    style
}

/// How one block is set: its base style, where it sits, and what it wears.
struct Plan {
    style: Style,
    /// The block's `line-height`, which its words inherit unless they set one.
    line: css::LineHeight,
    indent: u32,
    /// `text-indent`: how far into the box the first line starts, the rest of
    /// them being flush with its left edge.
    first_indent: u32,
    /// `letter-spacing` for the block's own text, which is also what its list
    /// marker is set with.
    letter: i32,
    gap_before: u32,
    gap_after: u32,
    marker: Option<String>,
    /// `white-space`: whether the source's spaces and newlines are the layout,
    /// and whether a line may be broken to fit the column.
    ws: css::WhiteSpace,
    /// `word-break`/`overflow-wrap`: whether a word too wide for the space left
    /// may be cut there rather than pushed whole onto the next line.
    wrap: css::Wrap,
    /// The measure `width`/`max-width` asked for, already resolved to pixels
    /// by the cascade. `None` is the whole column.
    measure: Option<u32>,
    /// `height`, `min-height` and `max-height`, in pixels, sizing the border
    /// box the way the measure does. `None` in all three is content height.
    height: Option<u32>,
    min_height: Option<u32>,
    max_height: Option<u32>,
    /// `min-width`: a floor under the box, applied after the measure has
    /// narrowed it. It cannot push the box past the column, since there is no
    /// horizontal scroll to reach what would sit outside.
    min_width: Option<u32>,
    /// `margin-right`: how much of the column the box gives back on its right,
    /// which narrows it without moving where it starts.
    trail: u32,
    center: bool,
    align: Align,
    background: Option<u32>,
    pad: Sides<u32>,
    border: Sides<Edge>,
    /// `visibility: hidden` on the element that opened the block: it is laid
    /// out and measured as it would have been, and nothing it owns is painted.
    invisible: bool,
}

/// The height a line of `style` occupies: what the page asked for with
/// `line-height`, or the face's own metrics when it asked for nothing.
fn leading(line: css::LineHeight, style: Style) -> u32 {
    line.px(style.px)
        .unwrap_or_else(|| text::line_height(style))
}

/// The border box's height: what the page declared, clamped, or the `content`
/// it came out to when it declared nothing.
fn block_height(plan: &Plan, content: u32) -> u32 {
    let mut used = plan.height.unwrap_or(content);
    if let Some(max) = plan.max_height {
        used = used.min(max);
    }
    if let Some(min) = plan.min_height {
        used = used.max(min);
    }
    used
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
    plan.trail = css.margin_right.unwrap_or(0);
    plan.first_indent = css.indent;
    plan.letter = css.letter_spacing;
    plan.measure = css.measure;
    plan.height = css.height;
    plan.min_height = css.min_height;
    plan.max_height = css.max_height;
    plan.min_width = css.min_width;
    plan.center = css.center;
    plan.align = css.align;
    // `<pre>` reaches here with the UA default already on it, so the block's
    // own value is the whole answer.
    plan.ws = css.white_space.unwrap_or_default();
    plan.wrap = css.wrap();
    plan.background = css.background;
    plan.invisible = css.invisible;
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
        first_indent: 0,
        letter: 0,
        gap_before: 0,
        gap_after: space(2),
        marker: None,
        ws: css::WhiteSpace::Normal,
        wrap: css::Wrap::Word,
        measure: None,
        height: None,
        min_height: None,
        max_height: None,
        min_width: None,
        trail: 0,
        center: false,
        align: Align::Left,
        background: None,
        pad: Sides::default(),
        border: Sides::default(),
        invisible: false,
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
            // `list-style-type: none` leaves the indent and drops the marker,
            // which is what a page styling a navigation list expects.
            marker: Some(marker.text()).filter(|text| !text.is_empty()),
            ..base
        },
        BlockKind::Pre => Plan {
            style: Style::mono(Theme::DEFAULT.syn_string.raw()),
            indent: QUOTE_INDENT,
            gap_before: space(2),
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
    // A `<sup>` is set smaller than its surroundings unless the page put a size
    // on it, which is the one thing that keeps a script from breaking the line
    // it sits in.
    match css.font_px {
        Some(px) => style.px = px,
        None => style.px = run.script.px(style.px),
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
            LineKind::Rule { x, width: rule_w } if !line.hidden => {
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
            } if !line.hidden => {
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
            // A hidden rule or picture holds its place and draws nothing; the
            // line has no text to fall through to either way.
            LineKind::Rule { .. } | LineKind::Image { .. } => continue,
            LineKind::Text => {}
        }
        for (index, item) in line.items.iter().enumerate() {
            if item.hidden {
                continue;
            }
            // Fragments share a baseline rather than a top edge: a smaller face
            // is dropped to the tallest one's feet, and `vertical-align` then
            // moves it off that baseline.
            let own = text::line_height(item.style);
            let text_top =
                y + line.lead as i32 + line.natural.saturating_sub(own) as i32 - item.shift;
            // A decoration and a highlight both run through the spaces between
            // the words they cover, reaching the next fragment when that one is
            // set the same way. Drawn per word either would come out dashed,
            // since a word is a fragment of its own.
            let next = line.items.get(index + 1).filter(|next| !next.hidden);
            let joined = |same: bool| match next {
                Some(next) if same && next.x >= item.x + item.width as i32 => {
                    (next.x - item.x) as u32
                }
                _ => item.width,
            };
            // An inline background covers the text and not the leading around
            // it: the box a run paints is its own, and taking the line's height
            // would make a highlighted word in an airy paragraph a tall block.
            if let Some(color) = item.background {
                let highlight = joined(
                    next.is_some_and(|n| n.background == item.background && n.shift == item.shift),
                );
                fill(
                    surface.pixels,
                    width,
                    height,
                    top,
                    item.x,
                    text_top,
                    highlight,
                    own,
                    color,
                );
            }
            text::draw_tracked(
                &mut surface,
                item.x,
                text_top,
                &item.text,
                item.style,
                item.letter,
            );
            let span = joined(next.is_some_and(|n| {
                n.decoration == item.decoration
                    && n.style.px == item.style.px
                    && n.style.color == item.style.color
                    && n.shift == item.shift
            }));
            // Against the text, not against the line: open leading would
            // otherwise leave the rules floating away from the words they mark,
            // and a superscript's rule would stay behind on the baseline it
            // left.
            let underline = text_top + own as i32 - 3;
            let rules = [
                (item.decoration.underline, underline),
                // A strike sits through the lowercase, which is about a third
                // of the font size above where the underline sits.
                (
                    item.decoration.line_through,
                    underline - (item.style.px as i32) * 3 / 10,
                ),
                (item.decoration.overline, text_top),
            ];
            for (drawn, rule_y) in rules {
                if drawn {
                    fill(
                        surface.pixels,
                        width,
                        height,
                        top,
                        item.x,
                        rule_y,
                        span,
                        1,
                        item.style.color,
                    );
                }
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
