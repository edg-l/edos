//! HTML to a flat list of blocks.
//!
//! The block list is the whole document model: a browser stage with no CSS has
//! nothing to say about a box that is not either a run of inline text or a
//! break between two of them. Everything a renderer needs -- the text, its
//! emphasis, the link it belongs to -- is on a `Run`, so a text dump and a
//! window draw the same list.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::fmt;
use std::rc::Rc;

use edos_http::url::Url;
use edos_render::graphics::Color;
use edos_render::image::{Image, Svg, decode_bmp, looks_like_svg};
use html5ever::{local_name, parse_document, tendril::TendrilSink};
use markup5ever_rcdom::{Handle, NodeData, RcDom};

use crate::css::{
    self, Computed, Decorations, Element, ListStyle, MediaQueries, Stylesheet, Vars, Viewport,
    WhiteSpace,
};

/// What a block is, which decides its font size and its leading marker.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum BlockKind {
    /// `h1`..`h6`, carrying the level.
    Heading(u8),
    Paragraph,
    /// `li`, carrying its nesting depth and the marker its list wants.
    ListItem {
        depth: usize,
        marker: Marker,
    },
    /// `pre`, whose whitespace survives.
    Pre,
    /// `blockquote` content.
    Quote,
    /// `hr`.
    Rule,
    /// An `img` whose bytes were fetched and decoded, carrying its alt text as
    /// its runs so a rendering that cannot draw it still says what it was.
    Image,
}

/// The marker one `li` wears: the `list-style-type` the cascade left on it, and
/// its position in its list, which only a counting style reads.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Marker {
    pub style: ListStyle,
    pub n: usize,
}

impl Marker {
    /// The marker text, with the space that separates it from the item.
    pub fn text(&self) -> String {
        self.with(ListStyle::marker)
    }

    /// The same for a plain-text rendering.
    pub fn ascii(&self) -> String {
        self.with(ListStyle::ascii_marker)
    }

    fn with(&self, render: fn(ListStyle, usize) -> String) -> String {
        let marker = render(self.style, self.n);
        if marker.is_empty() {
            marker
        } else {
            marker + " "
        }
    }
}

/// A stretch of text sharing one appearance and one link target.
#[derive(Clone, Debug)]
pub struct Run {
    pub text: String,
    /// Absolute URL, already resolved against the document's own.
    pub link: Option<String>,
    pub bold: bool,
    pub italic: bool,
    pub code: bool,
    /// `<sup>`/`<sub>`, which set the run smaller and off the baseline.
    pub script: Script,
    /// What the document's own CSS asked for, which overrides all three flags
    /// above wherever it says anything.
    pub css: Computed,
}

#[derive(Clone, Debug)]
pub struct Block {
    pub kind: BlockKind,
    pub runs: Vec<Run>,
    /// The style of the element that opened the block, for the properties that
    /// belong to a box rather than to a run: its margins, and the colour and
    /// size its runs inherit.
    pub css: Computed,
    /// The decoded picture, on a [`BlockKind::Image`] block only. Shared
    /// because the history keeps a document alive after the next one is
    /// parsed, and a page of photographs is the one thing here worth not
    /// copying.
    pub picture: Option<Rc<Picture>>,
}

/// A decoded image, kept in whichever form it was decoded to.
///
/// A vector document stays a tree rather than becoming a bitmap, so a window
/// resized wider re-renders it at the new column width instead of magnifying
/// what it drew at the old one.
pub enum Picture {
    Raster(Image),
    Vector(Svg),
}

impl Picture {
    /// The size the image asks to be drawn at.
    pub fn intrinsic_size(&self) -> (u32, u32) {
        match self {
            Picture::Raster(image) => (image.width.max(1), image.height.max(1)),
            Picture::Vector(svg) => svg.intrinsic_size(),
        }
    }

    /// Rasterise at exactly `width` x `height`, over `background`.
    ///
    /// The caller sizes the box, since only it knows the column; both axes are
    /// scaled independently, so pass a size that keeps the aspect ratio.
    pub fn render(&self, width: u32, height: u32, background: Color) -> Option<Vec<u32>> {
        if width == 0 || height == 0 {
            return None;
        }
        match self {
            Picture::Raster(image) => Some(image.scaled_to_fit(width, height)),
            Picture::Vector(svg) => svg.render(width, height, background).ok().map(|i| i.pixels),
        }
    }
}

impl fmt::Debug for Picture {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let (width, height) = self.intrinsic_size();
        let kind = match self {
            Picture::Raster(_) => "raster",
            Picture::Vector(_) => "vector",
        };
        write!(f, "Picture({kind} {width}x{height})")
    }
}

/// Decode fetched bytes, sniffing the format rather than trusting the URL's
/// extension or the server's content type.
///
/// BMP and SVG are what this system decodes; a PNG or a JPEG is not an error,
/// it is an image that renders as its alt text.
fn decode(bytes: &[u8]) -> Option<Picture> {
    if looks_like_svg(bytes) {
        Svg::parse(bytes).ok().map(Picture::Vector)
    } else {
        decode_bmp(bytes).ok().map(Picture::Raster)
    }
}

impl Block {
    /// The block's text with its runs joined, for a plain-text rendering.
    pub fn text(&self) -> String {
        self.runs.iter().map(|r| r.text.as_str()).collect()
    }
}

/// Every subresource a document has asked for and what came back.
///
/// Misses are cached alongside hits: a stylesheet that could not be had once
/// cannot be had now either, and a rebuild is not the place to spend a network
/// timeout finding that out again.
type Cache = Rc<RefCell<BTreeMap<String, Option<Vec<u8>>>>>;

/// What a document needs to be built again at a different viewport.
struct Source {
    html: Rc<Vec<u8>>,
    base: Url,
    cache: Cache,
    /// The media queries the build answered, and the viewport it answered them
    /// against.
    media: MediaQueries,
    viewport: Viewport,
}

/// A parsed document: its title and its blocks.
///
/// The document's own URL does not survive parsing because it does not need
/// to: every link on a `Run` was resolved against it already. Its bytes do,
/// because a media query is answered against the window and the window
/// resizes.
pub struct Document {
    pub title: String,
    pub blocks: Vec<Block>,
    source: Source,
}

impl Document {
    /// The title to show for the page, for the documents that carry none.
    pub fn display_title(&self) -> &str {
        if self.title.is_empty() {
            "untitled"
        } else {
            &self.title
        }
    }

    /// The same page built for `viewport`, or `None` when it would come out
    /// identical -- which is every resize of a document that writes no media
    /// query, and every resize too small to move one.
    ///
    /// Nothing already fetched is fetched again. `fetch` is still needed
    /// because a widened window can make a `<link media>` match for the first
    /// time, and that sheet has never been asked for.
    pub fn reflow(
        &self,
        viewport: Viewport,
        fetch: &dyn Fn(&str) -> Option<Vec<u8>>,
    ) -> Option<Document> {
        if !self.source.media.differ(&self.source.viewport, &viewport) {
            return None;
        }
        Some(build(
            Source {
                html: Rc::clone(&self.source.html),
                base: self.source.base.clone(),
                cache: Rc::clone(&self.source.cache),
                media: MediaQueries::default(),
                viewport,
            },
            fetch,
        ))
    }
}

/// The font size relative lengths and the absolute keywords resolve against.
pub const ROOT_PX: u32 = edos_render::font::size::BODY;

/// How many external stylesheets one document may pull in. A page linking more
/// than this is linking print sheets and font sheets; a browser that fetched
/// all of them would spend the page's load time on styles nothing reads.
const MAX_SHEETS: usize = 6;

/// How many images one document may fetch. Each one is fetched serially on the
/// thread about to lay the page out, so this is the page's load time as much as
/// its appearance; a document is readable long before its twentieth picture.
const MAX_IMAGES: usize = 12;

/// Parse `html` as a document fetched from `base`, using `fetch` for the
/// stylesheets it links.
///
/// `fetch` takes an absolute URL and returns the bytes, or `None` when it could
/// not be had: a stylesheet that fails to load leaves the page unstyled, never
/// unparsed.
///
/// `viewport` answers the document's media queries. The cascade is what a
/// media query changes and the cascade runs here, so a window resized
/// afterwards asks [`Document::reflow`] for a document built at its new size.
pub fn parse(
    html: &[u8],
    base: Url,
    fetch: &dyn Fn(&str) -> Option<Vec<u8>>,
    viewport: Viewport,
) -> Document {
    build(
        Source {
            html: Rc::new(html.to_vec()),
            base,
            cache: Cache::default(),
            media: MediaQueries::default(),
            viewport,
        },
        fetch,
    )
}

/// Parse and style `source`'s bytes, fetching what it references through its
/// own cache so a rebuild costs no network.
fn build(mut source: Source, fetch: &dyn Fn(&str) -> Option<Vec<u8>>) -> Document {
    let cache = Rc::clone(&source.cache);
    let cached = |url: &str| -> Option<Vec<u8>> {
        if let Some(hit) = cache.borrow().get(url) {
            return hit.clone();
        }
        let bytes = fetch(url);
        cache.borrow_mut().insert(url.to_string(), bytes.clone());
        bytes
    };

    let dom = parse_document(RcDom::default(), Default::default())
        .from_utf8()
        .read_from(&mut &source.html[..])
        .expect("reading from a slice cannot fail");

    let viewport = source.viewport;
    // The sheet has to be whole before the first element is styled, and a
    // `<style>` or a `<link>` may sit anywhere in the document, so collecting
    // it is its own pass rather than something the walk picks up as it goes.
    let mut sheets = Sheets {
        sheet: Stylesheet::new(viewport),
        base: &source.base,
        fetch: &cached,
        fetched: 0,
        viewport,
    };
    sheets.collect(&dom.document);
    let sheet = sheets.sheet;
    source.media = sheet.media.clone();

    let mut builder = Builder {
        base: source.base.clone(),
        fetch: &cached,
        images: 0,
        title: String::new(),
        blocks: Vec::new(),
        runs: Vec::new(),
        kind: BlockKind::Paragraph,
        style: Style::default(),
        lists: Vec::new(),
        sheet,
        stack: Vec::new(),
        computed: Computed::default(),
        vars: Vars::root(),
        block_css: Computed::default(),
    };
    builder.walk(&dom.document);
    builder.flush();

    Document {
        title: builder.title,
        blocks: builder.blocks,
        source,
    }
}

#[derive(Clone, Default)]
struct Style {
    link: Option<String>,
    bold: bool,
    italic: bool,
    code: bool,
    script: Script,
}

/// Where a run sits against the line's baseline, from the element that opened
/// it. `vertical-align` overrides it wherever the page says anything.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum Script {
    #[default]
    Baseline,
    Super,
    Sub,
}

impl Script {
    /// The shift a run of `px` text takes, in pixels, positive raising it.
    /// Both are fractions of the size the superscript would have been set at
    /// had it not been shrunk, which is why the caller passes the parent's.
    pub fn shift(self, px: u32) -> i32 {
        match self {
            Script::Baseline => 0,
            Script::Super => px as i32 / 3,
            Script::Sub => -(px as i32) / 5,
        }
    }

    /// How large the run is set. A superscript at the size of its surroundings
    /// reads as a broken line rather than as a script, so the UA shrinks it
    /// (html §15.3.3 sets `font-size: smaller` on both).
    pub fn px(self, px: u32) -> u32 {
        match self {
            Script::Baseline => px,
            _ => (px * 5 / 6).max(1),
        }
    }
}

/// A list being built, so `li` knows whether it is bulleted or numbered.
struct List {
    ordered: bool,
    next: usize,
}

struct Builder<'a> {
    base: Url,
    fetch: &'a dyn Fn(&str) -> Option<Vec<u8>>,
    /// How many images have been fetched, against [`MAX_IMAGES`].
    images: usize,
    title: String,
    blocks: Vec<Block>,
    runs: Vec<Run>,
    kind: BlockKind,
    style: Style,
    lists: Vec<List>,
    sheet: Stylesheet,
    /// The open elements, innermost last, which is what a selector matches
    /// against.
    stack: Vec<Element>,
    /// The style of the innermost open element.
    computed: Computed,
    /// The custom properties in scope on it.
    vars: Rc<Vars>,
    /// The style of the element that opened the block being built.
    block_css: Computed,
}

impl Builder<'_> {
    fn walk(&mut self, node: &Handle) {
        match &node.data {
            NodeData::Text { contents } => {
                let text = contents.borrow();
                self.push_text(&text);
            }
            NodeData::Element { name, .. } => {
                let tag = name.local.clone();
                if matches!(
                    tag,
                    local_name!("script")
                        | local_name!("style")
                        | local_name!("head")
                        | local_name!("noscript")
                        | local_name!("template")
                        | local_name!("svg")
                        | local_name!("iframe")
                ) {
                    // `title` lives in `head`, and is the one thing wanted from it.
                    if tag == local_name!("head") {
                        self.title = find_title(node).unwrap_or_default();
                    }
                    return;
                }

                self.stack.push(element_context(node, &tag));
                let saved_computed = self.computed;
                let saved_vars = Rc::clone(&self.vars);
                (self.computed, self.vars) = self.sheet.cascade(
                    &self.stack,
                    attr(node, "style").as_deref(),
                    &saved_computed,
                    &saved_vars,
                    ROOT_PX,
                );
                // A hidden element contributes nothing, subtree included, and
                // must not even open a block: `display: none` on a page's
                // navigation is the difference between a document and a
                // document with its menu spilled across the top of it.
                if self.computed.hidden {
                    self.stack.pop();
                    self.computed = saved_computed;
                    self.vars = saved_vars;
                    return;
                }

                // The UA stylesheet's one whitespace rule, applied after the
                // cascade so an author rule on the same box still wins, and
                // before the block opens so the block carries it.
                if tag == local_name!("pre") && self.computed.white_space.is_none() {
                    self.computed.white_space = Some(WhiteSpace::Pre);
                }

                let saved_style = self.style.clone();
                let saved_block_css = self.block_css;
                let block = block_kind(&tag);

                if let Some(kind) = block {
                    self.flush();
                    self.kind = kind;
                    self.block_css = self.computed;
                }
                match tag {
                    local_name!("ul") => self.lists.push(List {
                        ordered: false,
                        next: 1,
                    }),
                    local_name!("ol") => self.lists.push(List {
                        ordered: true,
                        next: start_attr(node).unwrap_or(1),
                    }),
                    local_name!("li") => {
                        let (depth, marker) = self.marker();
                        self.kind = BlockKind::ListItem { depth, marker };
                    }
                    local_name!("a") => {
                        self.style.link = attr(node, "href").and_then(|h| self.resolve(&h))
                    }
                    local_name!("b") | local_name!("strong") => self.style.bold = true,
                    local_name!("i") | local_name!("em") => self.style.italic = true,
                    local_name!("s") | local_name!("del") | local_name!("strike") => self
                        .ua_decoration(
                            saved_computed.decoration,
                            Decorations {
                                line_through: true,
                                ..Decorations::default()
                            },
                        ),
                    local_name!("u") | local_name!("ins") => self.ua_decoration(
                        saved_computed.decoration,
                        Decorations {
                            underline: true,
                            ..Decorations::default()
                        },
                    ),
                    local_name!("code") | local_name!("kbd") | local_name!("samp") => {
                        self.style.code = true
                    }
                    local_name!("sup") => self.style.script = Script::Super,
                    local_name!("sub") => self.style.script = Script::Sub,
                    // A break is a newline no collapsing may swallow, so it is
                    // appended rather than pushed as text: every other newline
                    // in a collapsing box has become a space by then, which is
                    // what lets the line breaker treat one as a break.
                    local_name!("br") => self.append("\n"),
                    local_name!("img") => self.image(node),
                    _ => {}
                }

                for child in node.children.borrow().iter() {
                    self.walk(child);
                }

                if matches!(tag, local_name!("ul") | local_name!("ol")) {
                    self.lists.pop();
                }
                if block.is_some() {
                    self.flush();
                }
                self.style = saved_style;
                self.block_css = saved_block_css;
                self.computed = saved_computed;
                self.vars = saved_vars;
                self.stack.pop();
            }
            _ => {
                for child in node.children.borrow().iter() {
                    self.walk(child);
                }
            }
        }
    }

    /// The lines an element wears by being the element it is: `<del>` struck
    /// through, `<u>` underlined. They join whatever the element inherited,
    /// since a decoration propagates to a subtree, and they apply only where
    /// the cascade said nothing about this element, so `del { text-decoration:
    /// none }` still wins.
    fn ua_decoration(&mut self, inherited: Option<Decorations>, add: Decorations) {
        if self.computed.decoration != inherited {
            return;
        }
        self.computed.decoration = Some(inherited.unwrap_or_default().merged(add));
    }

    /// An `img`: fetch and decode it, or leave behind what a reader with
    /// images turned off would see.
    ///
    /// A picture is a block of its own, because the block list has no inline
    /// box. An image in the middle of a sentence therefore breaks it in two,
    /// and the text after it resumes in the block it interrupted.
    fn image(&mut self, node: &Handle) {
        let alt = attr(node, "alt").unwrap_or_default().trim().to_string();
        let Some(picture) = self.fetch_image(node) else {
            if !alt.is_empty() {
                self.push_text(&format!("[{}]", alt));
            }
            return;
        };

        let interrupted = self.kind;
        self.flush();
        let style = self.style.clone();
        let runs = (!alt.is_empty())
            .then(|| {
                vec![Run {
                    text: format!("[{}]", alt),
                    link: style.link,
                    bold: style.bold,
                    italic: style.italic,
                    code: style.code,
                    script: style.script,
                    css: self.computed,
                }]
            })
            .unwrap_or_default();
        self.blocks.push(Block {
            kind: BlockKind::Image,
            runs,
            css: self.computed,
            picture: Some(Rc::new(picture)),
        });
        self.kind = interrupted;
    }

    /// Fetch and decode one image, against the document's budget. The budget is
    /// spent on the attempt rather than on the success, since what it bounds is
    /// how long the page takes to arrive.
    fn fetch_image(&mut self, node: &Handle) -> Option<Picture> {
        if self.images >= MAX_IMAGES {
            return None;
        }
        let url = self.resolve(&attr(node, "src")?)?;
        self.images += 1;
        decode(&(self.fetch)(&url)?)
    }

    /// The depth and marker for an `li`, consuming its list's counter.
    ///
    /// Every list counts, ordered or not, since the counter is the item's
    /// position and a `ul` the page asked to number reads it too.
    fn marker(&mut self) -> (usize, Marker) {
        let depth = self.lists.len().saturating_sub(1);
        let ordered = self.lists.last().is_some_and(|list| list.ordered);
        let n = match self.lists.last_mut() {
            Some(list) => {
                let n = list.next;
                list.next += 1;
                n
            }
            None => 1,
        };
        let style = self.computed.list_style.unwrap_or(if ordered {
            ListStyle::Decimal
        } else {
            // The UA stylesheet's nesting rule: a list inside a list wears a
            // hollow bullet, and one inside that a square. HTML Standard
            // §15.3.10 "Lists".
            match depth % 3 {
                0 => ListStyle::Disc,
                1 => ListStyle::Circle,
                _ => ListStyle::Square,
            }
        });
        (depth, Marker { style, n })
    }

    fn resolve(&self, href: &str) -> Option<String> {
        let href = href.trim();
        if href.is_empty() || href.starts_with('#') {
            return None;
        }
        self.base.join(href).ok().map(|u| u.to_string())
    }

    /// Append text to the open block, collapsing whitespace as the box's
    /// `white-space` asks.
    ///
    /// A newline that survives here is a line break for every later stage, so a
    /// collapsing box must not leave one: `pre-line` keeps them and drops the
    /// spaces on either side, and `normal` turns them into spaces like any
    /// other whitespace.
    fn push_text(&mut self, text: &str) {
        let ws = self.white_space();
        if ws.keeps_spaces() {
            self.append(text);
            return;
        }
        let mut out = String::with_capacity(text.len());
        let mut space = self.ends_with_space();
        for ch in text.chars() {
            if ch == '\n' && ws.keeps_newlines() {
                if out.ends_with(' ') {
                    out.pop();
                }
                out.push('\n');
                space = true;
            } else if ch.is_whitespace() {
                if !space {
                    out.push(' ');
                    space = true;
                }
            } else {
                out.push(ch);
                space = false;
            }
        }
        if out.is_empty() {
            return;
        }
        self.append(&out);
    }

    /// The `white-space` in force on the innermost open element.
    fn white_space(&self) -> WhiteSpace {
        self.computed.white_space.unwrap_or_default()
    }

    /// True when the open block would swallow a following space, which is also
    /// true at the start of a block so leading whitespace is dropped.
    fn ends_with_space(&self) -> bool {
        match self.runs.last() {
            Some(run) => run.text.ends_with(char::is_whitespace),
            None => true,
        }
    }

    fn append(&mut self, text: &str) {
        let style = self.style.clone();
        let css = self.computed;
        match self.runs.last_mut() {
            Some(run)
                if run.link == style.link
                    && run.bold == style.bold
                    && run.italic == style.italic
                    && run.code == style.code
                    && run.script == style.script
                    && run.css == css =>
            {
                run.text.push_str(text)
            }
            _ => self.runs.push(Run {
                text: text.to_string(),
                link: style.link,
                bold: style.bold,
                italic: style.italic,
                code: style.code,
                script: style.script,
                css,
            }),
        }
    }

    /// Close the open block, dropping it when it holds nothing but whitespace.
    fn flush(&mut self) {
        let runs = std::mem::take(&mut self.runs);
        let kind = std::mem::replace(&mut self.kind, BlockKind::Paragraph);
        let css = self.block_css;
        if kind == BlockKind::Rule {
            self.blocks.push(Block {
                kind,
                runs: Vec::new(),
                css,
                picture: None,
            });
            return;
        }
        let mut runs = if css.white_space.unwrap_or_default().keeps_spaces() {
            trim_preserved(runs)
        } else {
            trim_edges(runs)
        };
        if runs.is_empty() {
            return;
        }
        runs.retain(|r| !r.text.is_empty());
        self.blocks.push(Block {
            kind,
            runs,
            css,
            picture: None,
        });
    }
}

/// Drop leading and trailing whitespace across the run list, since only the
/// ends of a block are trimmed and a run boundary may fall anywhere.
fn trim_edges(mut runs: Vec<Run>) -> Vec<Run> {
    while let Some(first) = runs.first_mut() {
        let trimmed = first.text.trim_start().to_string();
        if trimmed.is_empty() {
            runs.remove(0);
        } else {
            first.text = trimmed;
            break;
        }
    }
    while let Some(last) = runs.last_mut() {
        let trimmed = last.text.trim_end().to_string();
        if trimmed.is_empty() {
            runs.pop();
        } else {
            last.text = trimmed;
            break;
        }
    }
    runs
}

/// A block whose `white-space` keeps its spacing is trimmed only where HTML
/// itself ignores whitespace: the newline that follows the start tag, and the
/// trailing whitespace that is the closing tag's own indentation. Trimming its
/// leading spaces the way a collapsing block is trimmed would throw away the
/// indentation that is the whole point of the source's own layout.
fn trim_preserved(mut runs: Vec<Run>) -> Vec<Run> {
    if let Some(first) = runs.first_mut()
        && let Some(rest) = first.text.strip_prefix('\n')
    {
        first.text = rest.to_string();
    }
    while let Some(last) = runs.last_mut() {
        let trimmed = last.text.trim_end().to_string();
        if trimmed.is_empty() {
            runs.pop();
        } else {
            last.text = trimmed;
            break;
        }
    }
    runs
}

fn block_kind(tag: &html5ever::LocalName) -> Option<BlockKind> {
    Some(match *tag {
        local_name!("h1") => BlockKind::Heading(1),
        local_name!("h2") => BlockKind::Heading(2),
        local_name!("h3") => BlockKind::Heading(3),
        local_name!("h4") => BlockKind::Heading(4),
        local_name!("h5") => BlockKind::Heading(5),
        local_name!("h6") => BlockKind::Heading(6),
        local_name!("hr") => BlockKind::Rule,
        local_name!("pre") => BlockKind::Pre,
        local_name!("blockquote") => BlockKind::Quote,
        local_name!("p")
        | local_name!("div")
        | local_name!("section")
        | local_name!("article")
        | local_name!("header")
        | local_name!("footer")
        | local_name!("nav")
        | local_name!("main")
        | local_name!("aside")
        | local_name!("ul")
        | local_name!("ol")
        | local_name!("li")
        | local_name!("table")
        | local_name!("tr")
        | local_name!("dl")
        | local_name!("dt")
        | local_name!("dd")
        | local_name!("figure")
        | local_name!("figcaption")
        | local_name!("form")
        | local_name!("body") => BlockKind::Paragraph,
        _ => return None,
    })
}

/// What a selector can ask about an element: its tag, its id, its classes and
/// its attributes.
fn element_context(node: &Handle, tag: &html5ever::LocalName) -> Element {
    Element {
        tag: tag.to_string(),
        id: attr(node, "id").map(|id| id.trim().to_string()),
        classes: attr(node, "class")
            .unwrap_or_default()
            .split_whitespace()
            .map(str::to_string)
            .collect(),
        attrs: attrs(node),
    }
}

/// Every attribute of an element, names lower-cased so a selector written in
/// either case finds them.
fn attrs(node: &Handle) -> Vec<(String, String)> {
    let NodeData::Element { attrs, .. } = &node.data else {
        return Vec::new();
    };
    attrs
        .borrow()
        .iter()
        .map(|a| (a.name.local.to_lowercase(), a.value.to_string()))
        .collect()
}

/// The document's stylesheets, gathered in document order: every `<style>`
/// element's text and every `<link rel=stylesheet>`'s fetched body, wherever in
/// the document they sit.
///
/// Order is what makes this one pass rather than two. A sheet later in the
/// document wins a specificity tie against an earlier one, so a linked sheet
/// and an inline one have to arrive in the order the document wrote them.
struct Sheets<'a> {
    sheet: Stylesheet,
    base: &'a Url,
    fetch: &'a dyn Fn(&str) -> Option<Vec<u8>>,
    fetched: usize,
    viewport: Viewport,
}

impl Sheets<'_> {
    fn collect(&mut self, node: &Handle) {
        if let NodeData::Element { name, .. } = &node.data {
            match name.local {
                local_name!("style") => {
                    let mut source = String::new();
                    for child in node.children.borrow().iter() {
                        if let NodeData::Text { contents } = &child.data {
                            source.push_str(&contents.borrow());
                        }
                    }
                    self.sheet.add(&source);
                    return;
                }
                local_name!("link") => {
                    self.link(node);
                    return;
                }
                _ => {}
            }
        }
        for child in node.children.borrow().iter() {
            self.collect(child);
        }
    }

    /// Fetch a `<link>` when it is a stylesheet for this medium.
    fn link(&mut self, node: &Handle) {
        let rel = attr(node, "rel").unwrap_or_default();
        if !rel
            .split_whitespace()
            .any(|token| token.eq_ignore_ascii_case("stylesheet"))
        {
            return;
        }
        // A sheet for another medium is not fetched at all: a print sheet
        // applied to the screen is worse than no sheet, and fetching one this
        // will not use costs the page's load time.
        if let Some(media) = attr(node, "media") {
            self.sheet.media.record(&media);
            if !css::media_matches(&media, &self.viewport) {
                return;
            }
        }
        if self.fetched >= MAX_SHEETS {
            return;
        }
        let Some(href) = attr(node, "href") else {
            return;
        };
        let Ok(url) = self.base.join(href.trim()) else {
            return;
        };
        self.fetched += 1;
        if let Some(bytes) = (self.fetch)(&url.to_string())
            && let Ok(source) = String::from_utf8(bytes)
        {
            self.sheet.add(&source);
        }
    }
}

fn attr(node: &Handle, name: &str) -> Option<String> {
    let NodeData::Element { attrs, .. } = &node.data else {
        return None;
    };
    attrs
        .borrow()
        .iter()
        .find(|a| &*a.name.local == name)
        .map(|a| a.value.to_string())
}

fn start_attr(node: &Handle) -> Option<usize> {
    attr(node, "start")?.trim().parse().ok()
}

/// The text of the first `title` element under `head`.
fn find_title(head: &Handle) -> Option<String> {
    for child in head.children.borrow().iter() {
        let NodeData::Element { name, .. } = &child.data else {
            continue;
        };
        if name.local != local_name!("title") {
            continue;
        }
        let mut text = String::new();
        for grandchild in child.children.borrow().iter() {
            if let NodeData::Text { contents } = &grandchild.data {
                text.push_str(&contents.borrow());
            }
        }
        return Some(text.trim().to_string());
    }
    None
}
