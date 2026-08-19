//! HTML to a flat list of blocks.
//!
//! The block list is the whole document model: a browser stage with no CSS has
//! nothing to say about a box that is not either a run of inline text or a
//! break between two of them. Everything a renderer needs -- the text, its
//! emphasis, the link it belongs to -- is on a `Run`, so a text dump and a
//! window draw the same list.

use std::collections::BTreeMap;
use std::fmt;
use std::rc::Rc;
use std::sync::{Arc, Mutex};

use edos_http::url::Url;
use edos_render::graphics::Color;
use edos_render::image::{Image, Svg, decode_raster, looks_like_svg};
use html5ever::{LocalName, local_name, parse_document, tendril::TendrilSink};
use markup5ever_rcdom::{Handle, NodeData, RcDom};

use crate::css::{
    self, Computed, Decorations, Display, Element, ListStyle, MediaQueries, Siblings, Stylesheet,
    Tracks, Vars, Viewport, WhiteSpace,
};

/// The highlight `<mark>` is set in, as every UA sets it: yellow behind black.
const MARK_BACKGROUND: u32 = 0xffff_ff00;
const MARK_COLOR: u32 = 0xff00_0000;

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
    /// An `inline-block`'s contents, when this run is a box rather than text.
    ///
    /// The run then takes its place in the line as one atomic item, the way an
    /// image does: it does not break, and its size comes from laying its own
    /// subtree out rather than from measuring `text`, which is empty.
    ///
    /// `Arc` because a document is kept alive behind the next one in the
    /// history and a box may hold a whole subtree, and because a document is
    /// built on the loader thread and handed to the window.
    pub boxed: Option<Arc<Node>>,
}

/// A box in the layout tree.
///
/// The tree exists because a box engine needs one: a flat list of blocks cannot
/// say that three paragraphs share a container, which is the whole of what
/// `display: flex` and `display: grid` arrange. [`Document::blocks`] is this
/// tree flattened into document order, and is what the inline engine walks.
#[derive(Clone, Debug)]
pub enum Node {
    /// A box holding other boxes. Its own `css` carries the box properties --
    /// display, margins, padding, the flex and grid tracks -- that arrange
    /// them.
    Container {
        // Read by the box engine, which arranges the children from it. The
        // flattening path deliberately does not: a container's own box is
        // exactly what a flat list of leaves cannot express.
        #[allow(dead_code)]
        css: Computed,
        children: Vec<Node>,
    },
    /// A box holding inline content, which is what the line breaker lays out.
    Leaf(Block),
}

impl Node {
    /// The leaves in document order.
    ///
    /// Flattening is lossless for everything the old flat model could express,
    /// which is what lets the two live side by side while the box engine is
    /// wired up underneath.
    pub fn flatten_into(&self, out: &mut Vec<Block>) {
        match self {
            Node::Leaf(block) => {
                out.push(block.clone());
                // An `inline-block`'s contents hang off a run rather than
                // sitting in the tree, so a walk that only follows children
                // misses them and the text vanishes from anything reading this
                // list. Layout does not use it -- it arranges `root` -- so the
                // box appearing after the line it sits on costs nothing.
                for run in &block.runs {
                    if let Some(node) = &run.boxed {
                        node.flatten_into(out);
                    }
                }
            }
            Node::Container { children, .. } => {
                for child in children {
                    child.flatten_into(out);
                }
            }
        }
    }

    /// How deep this subtree nests, a container counting as one level.
    pub fn depth(&self) -> usize {
        match self {
            Node::Leaf(_) => 1,
            Node::Container { children, .. } => {
                1 + children.iter().map(Node::depth).max().unwrap_or(0)
            }
        }
    }

    /// The number of boxes in this subtree, counting itself.
    pub fn count(&self) -> usize {
        match self {
            Node::Leaf(_) => 1,
            Node::Container { children, .. } => 1 + children.iter().map(Node::count).sum::<usize>(),
        }
    }
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
    pub picture: Option<Arc<Picture>>,
    /// The `id` of the element that opened this block, which is what a link
    /// carrying a fragment names. Only a block-level id is recorded: a
    /// fragment points at a place to scroll to, and a place is a line.
    pub anchor: Option<String>,
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
        decode_raster(bytes).ok().map(Picture::Raster)
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
///
/// One of these is shared by every page a window loads, not made afresh per
/// document: the pages of a site link the same stylesheets, and following a
/// link fetched all of them again -- around 100 KB of CSS on `edos.edgl.dev`,
/// over a TLS handshake each.
pub type Cache = Arc<Mutex<BTreeMap<String, Option<Vec<u8>>>>>;

/// How much fetched content one of those may hold before it is emptied.
///
/// Emptied rather than evicted by age: nothing here records when an entry was
/// used, a miss costs a refetch rather than a wrong answer, and a browser
/// holding every image of every page visited is the failure this bounds.
const CACHE_BUDGET: usize = 8 * 1024 * 1024;

/// What a load needs besides the bytes.
#[derive(Clone)]
pub struct Context {
    /// The window the page is laid out for, which is what its media queries
    /// are answered against.
    pub viewport: Viewport,
    /// Whether to lay out the page's `<main>` alone. See [`Document::has_main`].
    pub reader: bool,
    /// The subresources this window has already fetched.
    pub cache: Cache,
}

impl Context {
    pub fn new(viewport: Viewport, reader: bool) -> Context {
        Context {
            viewport,
            reader,
            cache: Cache::default(),
        }
    }
}

/// What a document needs to be built again at a different viewport.
struct Source {
    html: Arc<Vec<u8>>,
    base: Url,
    cache: Cache,
    /// Whether to lay out the page's `<main>` alone. See [`Document::reader`].
    reader: bool,
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
    /// The tree flattened into document order. What the inline engine walks.
    pub blocks: Vec<Block>,
    /// The box tree, which is what a box engine arranges.
    pub root: Node,
    /// Whether the page carries a `<main>`, which is what reader mode lays out
    /// and what makes offering it worth anything.
    has_main: bool,
    source: Source,
}

impl Document {
    /// A page that was refused before it was parsed, standing in for the one
    /// that could not be built.
    ///
    /// It is a document rather than an error because every caller renders one:
    /// the window, the history and the `-d` dump all take a `Document`, and a
    /// page that says why it is blank is more use than a program that exits.
    fn refused(base: Url, viewport: Viewport, nesting: usize) -> Document {
        let text = format!(
            "This page nests elements {} deep, past the {} this browser will \
             parse, and was not loaded.",
            nesting, MAX_SOURCE_NESTING
        );
        let block = Block {
            kind: BlockKind::Paragraph,
            runs: vec![Run {
                text,
                link: None,
                bold: false,
                italic: false,
                code: false,
                script: Script::default(),
                css: Computed::default(),
                boxed: None,
            }],
            css: Computed::default(),
            picture: None,
            anchor: None,
        };
        Document {
            has_main: false,
            title: String::from("Refused"),
            blocks: vec![block.clone()],
            root: Node::Container {
                css: Computed::default(),
                children: vec![Node::Leaf(block)],
            },
            source: Source {
                html: Arc::new(Vec::new()),
                base,
                cache: Cache::default(),
                reader: true,
                media: MediaQueries::default(),
                viewport,
            },
        }
    }

    /// A page with nothing on it: what the window holds before its first load
    /// finishes, and what stands in the history's place while a page is moved
    /// out of it.
    pub fn blank() -> Document {
        Document {
            has_main: false,
            title: String::new(),
            blocks: Vec::new(),
            root: Node::Container {
                css: Computed::default(),
                children: Vec::new(),
            },
            source: Source {
                html: Arc::new(Vec::new()),
                // Every document has an origin, and a blank one's is the
                // machine it is running on, so a relative address typed into
                // the address bar still resolves.
                base: Url::parse("http://localhost/").expect("a literal URL parses"),
                cache: Cache::default(),
                reader: true,
                media: MediaQueries::default(),
                viewport: Viewport::default(),
            },
        }
    }

    /// Whether this page marks its main content, and so whether reader mode
    /// changes anything about it.
    ///
    /// A page's navigation is laid out in the flow here, because `position:
    /// fixed` and `position: sticky` are not implemented: a sidebar a real
    /// browser pins beside the article lands above it instead, and a
    /// documentation page opens on a screen of links rather than on what it is
    /// about. Reader mode lays out the `<main>` the page itself marked and
    /// leaves the rest of the document out.
    pub fn has_main(&self) -> bool {
        self.has_main
    }

    /// The same page built in the other mode. Nothing is fetched again: the
    /// bytes and every subresource are the ones this document already holds.
    pub fn set_reader(&self, reader: bool, fetch: &dyn Fn(&str) -> Option<Vec<u8>>) -> Document {
        build(
            Source {
                html: Arc::clone(&self.source.html),
                base: self.source.base.clone(),
                cache: Arc::clone(&self.source.cache),
                reader,
                media: MediaQueries::default(),
                viewport: self.source.viewport,
            },
            fetch,
        )
    }

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
                html: Arc::clone(&self.source.html),
                base: self.source.base.clone(),
                cache: Arc::clone(&self.source.cache),
                reader: self.source.reader,
                media: MediaQueries::default(),
                viewport,
            },
            fetch,
        ))
    }
}

impl Default for Document {
    fn default() -> Document {
        Document::blank()
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

/// How deep the element tree is walked before the rest is dropped.
///
/// The walk recurses once per level, and the tree comes off the network, so
/// without a bound a page of sufficiently nested markup overflows this
/// program's stack rather than rendering badly. Far past anything a real
/// document nests: the deepest page in the tree's own fixtures is under 20.
const MAX_DEPTH: usize = 512;

/// How deeply the source may nest before the document is refused unparsed.
///
/// [`MAX_DEPTH`] bounds the *walk*, and the walk runs over a tree that
/// `html5ever` has already built by recursing once per level. So a page nested
/// far enough overflows this program's stack inside the parser, before the walk
/// bound gets a chance to apply. Measured in the guest: 16384 levels parse and
/// render, 32768 die on SIGSEGV. This sits a factor of two under the deepest
/// depth known to survive, and three orders of magnitude above what a real
/// document nests.
const MAX_SOURCE_NESTING: usize = 8192;

/// Elements that never nest because they have no end tag.
const VOID_ELEMENTS: [&str; 14] = [
    "area", "base", "br", "col", "embed", "hr", "img", "input", "link", "meta", "param", "source",
    "track", "wbr",
];

/// The deepest run of unclosed element tags in `html`.
///
/// A scan of the bytes rather than a parse, because the parse is the thing
/// being protected — asking the parser how deep the document is would already
/// have overflowed. It is approximate by design: a `<` inside an attribute
/// value is not distinguished from a tag, and the count is therefore an
/// over-estimate. Against a limit in the thousands that costs nothing, and
/// erring high is the safe direction.
fn source_nesting(html: &[u8]) -> usize {
    let mut depth = 0usize;
    let mut max = 0usize;
    let mut i = 0;
    while let Some(off) = html[i..].iter().position(|&b| b == b'<') {
        i += off + 1;
        let Some(&first) = html.get(i) else { break };
        // Comments, doctypes and processing instructions open nothing.
        if first == b'!' || first == b'?' {
            continue;
        }
        let closing = first == b'/';
        let name_at = if closing { i + 1 } else { i };
        let name: String = html[name_at..]
            .iter()
            .take_while(|b| b.is_ascii_alphanumeric())
            .map(|b| b.to_ascii_lowercase() as char)
            .collect();
        if name.is_empty() {
            continue;
        }
        // Step over the tag so that a `>` inside it is not read as the end of
        // some later one, and so the scan is linear.
        let Some(end) = html[name_at..].iter().position(|&b| b == b'>') else {
            break;
        };
        let self_closing = html[name_at + end - 1] == b'/';
        i = name_at + end + 1;

        if closing {
            depth = depth.saturating_sub(1);
        } else if !self_closing && !VOID_ELEMENTS.contains(&name.as_str()) {
            depth += 1;
            max = max.max(depth);
        }
    }
    max
}

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
    context: &Context,
) -> Document {
    let nesting = source_nesting(html);
    if nesting > MAX_SOURCE_NESTING {
        return Document::refused(base, context.viewport, nesting);
    }
    build(
        Source {
            html: Arc::new(html.to_vec()),
            base,
            cache: Arc::clone(&context.cache),
            reader: context.reader,
            media: MediaQueries::default(),
            viewport: context.viewport,
        },
        fetch,
    )
}

/// Parse and style `source`'s bytes, fetching what it references through its
/// own cache so a rebuild costs no network.
fn build(mut source: Source, fetch: &dyn Fn(&str) -> Option<Vec<u8>>) -> Document {
    let cache = Arc::clone(&source.cache);
    // The lock is never held across the fetch: a document is built on the
    // loader thread while the window reads none of this, so the only thing
    // holding it there could do is make a slow server hold a lock as well.
    let cached = |url: &str| -> Option<Vec<u8>> {
        if let Some(hit) = cache.lock().ok().and_then(|cache| cache.get(url).cloned()) {
            return hit;
        }
        let bytes = fetch(url);
        if let Ok(mut cache) = cache.lock() {
            let held: usize = cache.values().flatten().map(Vec::len).sum();
            if held > CACHE_BUDGET {
                cache.clear();
            }
            cache.insert(url.to_string(), bytes.clone());
        }
        bytes
    };

    let dom = parse_document(RcDom::default(), Default::default())
        .from_utf8()
        .read_from(&mut &source.html[..])
        .expect("reading from a slice cannot fail");

    let main = main_path(&dom.document);
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
        frames: vec![Frame::default()],
        runs: Vec::new(),
        kind: BlockKind::Paragraph,
        style: Style::default(),
        lists: Vec::new(),
        sheet,
        stack: Vec::new(),
        computed: Computed::default(),
        vars: Vars::root(),
        block_css: Computed::default(),
        depth: 0,
        anchor: None,
        path: match &main {
            Some(path) if source.reader => path.clone(),
            _ => Vec::new(),
        },
        in_main: false,
    };
    builder.walk(&dom.document, Siblings::default(), &Rc::new(Vec::new()));
    builder.flush();
    while builder.frames.len() > 1 {
        builder.close_frame();
    }

    let root = Node::Container {
        css: Computed::default(),
        children: builder.frames.pop().expect("the document frame").children,
    };
    let mut blocks = Vec::new();
    root.flatten_into(&mut blocks);

    Document {
        title: builder.title,
        blocks,
        root,
        has_main: main.is_some(),
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

/// One open container while the tree is being built.
#[derive(Default)]
struct Frame {
    css: Computed,
    children: Vec<Node>,
}

struct Builder<'a> {
    base: Url,
    fetch: &'a dyn Fn(&str) -> Option<Vec<u8>>,
    /// How many images have been fetched, against [`MAX_IMAGES`].
    images: usize,
    title: String,
    /// The open containers, outermost first. The last is where a flushed block
    /// lands, and the first is the document itself, which never closes.
    frames: Vec<Frame>,
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
    /// How many levels of element the walk is currently inside, against
    /// [`MAX_DEPTH`].
    depth: usize,
    /// The elements from the document root down to its `<main>`, when the page
    /// has one and reader mode is on. Above `<main>` the walk enters only
    /// these, so the page's own navigation is not laid out; inside it,
    /// everything.
    path: Vec<Handle>,
    /// Whether the walk has reached `<main>`.
    in_main: bool,
    /// The `id` of the innermost element seen since the last block was
    /// emitted, waiting for the block it names.
    anchor: Option<String>,
}

impl Builder<'_> {
    fn walk(&mut self, node: &Handle, position: Siblings, row: &Rc<Vec<Element>>) {
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

                let mut context = element_context(node, &tag, position);
                // The outermost id wins, since a fragment naming a section
                // means the top of it rather than the first thing inside that
                // happens to be identified as well.
                if self.anchor.is_none() {
                    self.anchor = context.id.clone();
                }
                context.siblings = Rc::clone(row);
                self.stack.push(context);
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

                // A table is laid out as a grid of one column per cell in its
                // widest row. There is no table layout algorithm here and no
                // `display: table` to reach for, and the alternative is what
                // this did before: `<tr>` opened a block and its cells were
                // inline inside it, so a row came out as its cells' text run
                // together on one line. `colspan` and `rowspan` are not
                // honoured, so a table using them misaligns from that row on.
                if tag == local_name!("table") && self.computed.display.is_none()
                    && let Some(columns) = table_columns(node) {
                        self.computed.display = Some(Display::Grid);
                        self.computed.grid_columns = Some(columns);
                    }

                let entered_main = !self.in_main && self.is_main(node);
                if entered_main {
                    self.in_main = true;
                }
                let saved_style = self.style.clone();
                let saved_block_css = self.block_css;
                // `display` overrides the box the element would open on its
                // own, in both directions: a `<span>` asking for `block` gets
                // a block of its own, and a `<li>` asking for `inline` stays
                // in the line its parent is building. A box the page said
                // nothing about is the element's own.
                let block = match self.computed.display {
                    Some(Display::Inline) => None,
                    Some(Display::InlineBlock) => {
                        Some(block_kind(&tag).unwrap_or(BlockKind::Paragraph))
                    }
                    Some(Display::Block | Display::ListItem | Display::Flex | Display::Grid) => {
                        Some(block_kind(&tag).unwrap_or(BlockKind::Paragraph))
                    }
                    None => block_kind(&tag),
                };

                // An inline-block is an inline-*level* box: the line it sits in
                // carries on around it, so the parent's runs are set aside
                // rather than flushed into a block of their own.
                let inline_box = self.computed.display == Some(Display::InlineBlock);
                let mut outer_runs = Vec::new();
                let mut outer_kind = BlockKind::Paragraph;
                if let Some(kind) = block {
                    if inline_box {
                        outer_runs = core::mem::take(&mut self.runs);
                        outer_kind = self.kind;
                    } else {
                        // Close whatever inline content the parent had
                        // accumulated before this box interrupted it, an
                        // anonymous block box in CSS terms.
                        self.flush();
                    }
                    self.open_frame(self.computed);
                    self.kind = kind;
                    self.block_css = self.computed;
                }
                // A marker belongs to `display: list-item` and to nothing
                // else, which is why `li { display: block }` loses its bullet.
                if self.computed.display == Some(Display::ListItem)
                    || (tag == local_name!("li") && self.computed.display.is_none())
                {
                    let (depth, marker) = self.marker();
                    self.kind = BlockKind::ListItem { depth, marker };
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
                    local_name!("mark") => self.ua_background(&saved_computed),
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

                self.walk_children(node);

                if matches!(tag, local_name!("ul") | local_name!("ol")) {
                    self.lists.pop();
                }
                if block.is_some() {
                    self.flush();
                    if inline_box {
                        let boxed = self.take_frame();
                        self.runs = outer_runs;
                        self.kind = outer_kind;
                        if let Some(node) = boxed {
                            self.runs.push(Run {
                                text: String::new(),
                                link: self.style.link.clone(),
                                bold: false,
                                italic: false,
                                code: false,
                                script: Script::default(),
                                css: self.computed,
                                boxed: Some(Arc::new(node)),
                            });
                        }
                    } else {
                        self.close_frame();
                    }
                }
                if entered_main {
                    self.in_main = false;
                }
                self.style = saved_style;
                self.block_css = saved_block_css;
                self.computed = saved_computed;
                self.vars = saved_vars;
                self.stack.pop();
            }
            _ => self.walk_children(node),
        }
    }

    /// Walk an element's children, each with its position among its element
    /// siblings and the row they all share, which is what `+` and `~` search.
    /// The row holds every element child, including the ones the walk itself
    /// skips: a sibling combinator asks about the document, not about what was
    /// rendered.
    fn walk_children(&mut self, node: &Handle) {
        if self.depth >= MAX_DEPTH {
            return;
        }
        self.depth += 1;
        self.walk_children_inner(node);
        self.depth -= 1;
    }

    /// Whether `node` is one of the elements the reader-mode walk may enter.
    ///
    /// The chain above `<main>` is entered rather than skipped to, so the
    /// cascade keeps the ancestors a selector matches against and the custom
    /// properties `:root` declares -- starting the walk at `<main>` instead
    /// would drop a modern page's whole palette.
    fn on_path(&self, node: &Handle) -> bool {
        self.path.iter().any(|step| Rc::ptr_eq(step, node))
    }

    /// Whether `node` is the `<main>` the reader-mode walk is heading for.
    fn is_main(&self, node: &Handle) -> bool {
        self.path.last().is_some_and(|last| Rc::ptr_eq(last, node))
    }

    fn walk_children_inner(&mut self, node: &Handle) {
        let children = node.children.borrow();
        let positions = sibling_positions(&children);
        let row = Rc::new(
            children
                .iter()
                .zip(&positions)
                .filter_map(|(child, position)| match &child.data {
                    NodeData::Element { name, .. } => {
                        Some(element_context(child, &name.local, *position))
                    }
                    _ => None,
                })
                .collect(),
        );
        for (child, position) in children.iter().zip(positions) {
            // The row above was built from every element child, matched or
            // not, so a sibling combinator still sees the document.
            if !self.path.is_empty() && !self.in_main && !self.on_path(child) && !is_head(child) {
                continue;
            }
            self.walk(child, position, &row);
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

    /// The highlight a `<mark>` wears, painted only where the page has not
    /// said what the box looks like itself.
    ///
    /// The text is darkened with it, and only when it was inherited: a
    /// highlight sitting under whatever colour the surrounding page set is
    /// unreadable exactly where it is meant to stand out, and a page that
    /// coloured this element chose the pair itself.
    fn ua_background(&mut self, inherited: &Computed) {
        if self.computed.background.is_some() {
            return;
        }
        self.computed.background = Some(MARK_BACKGROUND);
        if self.computed.color == inherited.color {
            self.computed.color = Some(MARK_COLOR);
        }
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
        let runs = if !alt.is_empty() { {
                vec![Run {
                    text: format!("[{}]", alt),
                    link: style.link,
                    bold: style.bold,
                    italic: style.italic,
                    code: style.code,
                    script: style.script,
                    css: self.computed,
                    boxed: None,
                }]
            } } else { Default::default() };
        let anchor = self.anchor.take();
        self.push_leaf(Block {
            kind: BlockKind::Image,
            runs,
            css: self.computed,
            picture: Some(Arc::new(picture)),
            anchor,
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

    /// The absolute URL an `href` names, or `None` for one that names nothing
    /// this browser can go to.
    ///
    /// A fragment-only reference resolves to this document plus that fragment,
    /// per RFC 3986 §5.2, and is kept: it is what a table of contents is made
    /// of, and the window answers it by scrolling rather than by fetching.
    fn resolve(&self, href: &str) -> Option<String> {
        let href = href.trim();
        if href.is_empty() {
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
                boxed: None,
            }),
        }
    }

    /// Close the open block, dropping it when it holds nothing but whitespace.
    fn flush(&mut self) {
        let runs = std::mem::take(&mut self.runs);
        let kind = std::mem::replace(&mut self.kind, BlockKind::Paragraph);
        let css = self.block_css;
        if kind == BlockKind::Rule {
            let anchor = self.anchor.take();
            self.push_leaf(Block {
                kind,
                runs: Vec::new(),
                css,
                picture: None,
                anchor,
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
        // A box carries no text and would otherwise be dropped here, along with
        // the whole `inline-block` it stands for.
        runs.retain(|r| !r.text.is_empty() || r.boxed.is_some());
        // Taken only where a block is actually emitted, so the id of a wrapper
        // whose own block holds nothing lands on the first block inside it
        // rather than being dropped with the wrapper.
        let anchor = self.anchor.take();
        self.push_leaf(Block {
            kind,
            runs,
            css,
            picture: None,
            anchor,
        });
    }

    /// Put a finished block into the innermost open container.
    fn push_leaf(&mut self, block: Block) {
        self.frames
            .last_mut()
            .expect("the document frame is never popped")
            .children
            .push(Node::Leaf(block));
    }

    /// Open a container for an element that makes a box holding other boxes.
    fn open_frame(&mut self, css: Computed) {
        self.frames.push(Frame {
            css,
            children: Vec::new(),
        });
    }

    /// Close the innermost container and attach it to its parent.
    ///
    /// A container that holds exactly one leaf is replaced by that leaf: the
    /// wrapper would arrange a single child the same way its absence does, and
    /// keeping it would put a box between every paragraph and its parent. A
    /// container that holds nothing is dropped for the same reason.
    fn close_frame(&mut self) {
        if let Some(node) = self.take_frame() {
            self.frames
                .last_mut()
                .expect("the document frame is never popped")
                .children
                .push(node);
        }
    }

    /// Pop the innermost container and hand back what it held, if anything.
    ///
    /// Separate from `close_frame` because an `inline-block` wants the subtree
    /// rather than a sibling: it goes back into the parent's line as one run.
    fn take_frame(&mut self) -> Option<Node> {
        let frame = self.frames.pop()?;
        let mut children = frame.children;
        match children.len() {
            // An element holding nothing still draws whatever box it was given
            // one for: a `<span>` sized 12 by 12 with a background and a radius
            // of half of it is how a page draws a status dot, and dropping it
            // for having no text loses the only thing it was there for.
            0 => frame.css.paints().then(|| {
                Node::Leaf(Block {
                    kind: BlockKind::Paragraph,
                    runs: Vec::new(),
                    css: frame.css,
                    picture: None,
                    anchor: None,
                })
            }),
            1 if matches!(children[0], Node::Leaf(_)) => children.pop(),
            _ => Some(Node::Container {
                css: frame.css,
                children,
            }),
        }
    }
}

/// Drop leading and trailing whitespace across the run list, since only the
/// ends of a block are trimmed and a run boundary may fall anywhere.
fn trim_edges(mut runs: Vec<Run>) -> Vec<Run> {
    while let Some(first) = runs.first_mut() {
        if first.boxed.is_some() {
            break;
        }
        let trimmed = first.text.trim_start().to_string();
        if trimmed.is_empty() {
            runs.remove(0);
        } else {
            first.text = trimmed;
            break;
        }
    }
    while let Some(last) = runs.last_mut() {
        if last.boxed.is_some() {
            break;
        }
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
        | local_name!("td")
        | local_name!("th")
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

/// The column tracks a table needs: one per cell in its widest row, or `None`
/// for a table with no cells or with more columns than [`Tracks`] holds.
///
/// `<tr>`, `<thead>` and the rest open no box of their own -- they are not in
/// `block_kind` -- so the cells land in the table's own container and become
/// its grid items.
fn table_columns(node: &Handle) -> Option<Tracks> {
    let mut widest = 0;
    widest_row(node, &mut widest);
    Tracks::auto(widest)
}

fn widest_row(node: &Handle, widest: &mut usize) {
    for child in node.children.borrow().iter() {
        let NodeData::Element { name, .. } = &child.data else {
            continue;
        };
        match name.local {
            // A nested table counts its own columns, not this one's.
            local_name!("table") => continue,
            local_name!("tr") => {
                let cells = child
                    .children
                    .borrow()
                    .iter()
                    .filter(|cell| {
                        matches!(&cell.data, NodeData::Element { name, .. }
                            if name.local == local_name!("td") || name.local == local_name!("th"))
                    })
                    .count();
                *widest = (*widest).max(cells);
            }
            _ => widest_row(child, widest),
        }
    }
}

/// Whether `node` is the `<head>`, which reader mode walks whatever else it
/// skips: the document's title is in there and belongs to the page either way.
fn is_head(node: &Handle) -> bool {
    matches!(&node.data, NodeData::Element { name, .. } if name.local == local_name!("head"))
}

/// The chain from the document root down to its first `<main>`, or `None` for
/// a page that marks no main content.
///
/// `<main>` is the element the HTML Standard defines as "the dominant contents
/// of the document", which is exactly the question reader mode asks.
fn main_path(node: &Handle) -> Option<Vec<Handle>> {
    if let NodeData::Element { name, .. } = &node.data
        && name.local == local_name!("main") {
            return Some(vec![node.clone()]);
        }
    for child in node.children.borrow().iter() {
        if let Some(mut path) = main_path(child) {
            path.insert(0, node.clone());
            return Some(path);
        }
    }
    None
}

/// What a selector can ask about an element: its tag, its id, its classes, its
/// attributes and where it sits among its siblings.
fn element_context(node: &Handle, tag: &html5ever::LocalName, position: Siblings) -> Element {
    Element {
        tag: tag.to_string(),
        id: attr(node, "id").map(|id| id.trim().to_string()),
        classes: attr(node, "class")
            .unwrap_or_default()
            .split_whitespace()
            .map(str::to_string)
            .collect(),
        attrs: attrs(node),
        position,
        siblings: Rc::new(Vec::new()),
    }
}

/// Where each child sits among its element siblings, one entry per child so a
/// caller can walk children and positions together. Text and comments hold a
/// place in the list but are counted by nothing, and an element the walk
/// itself skips — a `<style>`, say — still counts, since `:nth-child` is a
/// question about the document rather than about what was rendered.
fn sibling_positions(children: &[Handle]) -> Vec<Siblings> {
    let tags: Vec<Option<LocalName>> = children
        .iter()
        .map(|child| match &child.data {
            NodeData::Element { name, .. } => Some(name.local.clone()),
            _ => None,
        })
        .collect();
    let count = tags.iter().flatten().count();
    let mut totals: Vec<(LocalName, usize)> = Vec::new();
    for tag in tags.iter().flatten() {
        match totals.iter_mut().find(|(name, _)| name == tag) {
            Some(total) => total.1 += 1,
            None => totals.push((tag.clone(), 1)),
        }
    }

    let mut seen: Vec<(LocalName, usize)> = Vec::new();
    let mut index = 0;
    let mut out = Vec::with_capacity(children.len());
    for tag in &tags {
        let Some(tag) = tag else {
            out.push(Siblings::default());
            continue;
        };
        index += 1;
        let type_index = match seen.iter_mut().find(|(name, _)| name == tag) {
            Some(slot) => {
                slot.1 += 1;
                slot.1
            }
            None => {
                seen.push((tag.clone(), 1));
                1
            }
        };
        let type_count = totals
            .iter()
            .find(|(name, _)| name == tag)
            .map_or(1, |(_, total)| *total);
        out.push(Siblings {
            index,
            count,
            type_index,
            type_count,
        });
    }
    out
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
