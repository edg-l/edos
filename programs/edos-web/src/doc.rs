//! HTML to a flat list of blocks.
//!
//! The block list is the whole document model: a browser stage with no CSS has
//! nothing to say about a box that is not either a run of inline text or a
//! break between two of them. Everything a renderer needs -- the text, its
//! emphasis, the link it belongs to -- is on a `Run`, so a text dump and a
//! window draw the same list.

use std::rc::Rc;

use edos_http::url::Url;
use html5ever::{local_name, parse_document, tendril::TendrilSink};
use markup5ever_rcdom::{Handle, NodeData, RcDom};

use crate::css::{Computed, Element, Stylesheet, Vars};

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
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Marker {
    Bullet,
    Number(usize),
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
}

impl Block {
    /// The block's text with its runs joined, for a plain-text rendering.
    pub fn text(&self) -> String {
        self.runs.iter().map(|r| r.text.as_str()).collect()
    }
}

/// A parsed document: its title and its blocks.
///
/// The document's own URL does not survive parsing because it does not need
/// to: every link on a `Run` was resolved against it already.
pub struct Document {
    pub title: String,
    pub blocks: Vec<Block>,
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
}

/// The font size relative lengths and the absolute keywords resolve against.
const ROOT_PX: u32 = edos_render::font::size::BODY;

/// How many external stylesheets one document may pull in. A page linking more
/// than this is linking print sheets and font sheets; a browser that fetched
/// all of them would spend the page's load time on styles nothing reads.
const MAX_SHEETS: usize = 6;

/// Parse `html` as a document fetched from `base`, using `fetch` for the
/// stylesheets it links.
///
/// `fetch` takes an absolute URL and returns the bytes, or `None` when it could
/// not be had: a stylesheet that fails to load leaves the page unstyled, never
/// unparsed.
pub fn parse(html: &[u8], base: Url, fetch: &dyn Fn(&str) -> Option<Vec<u8>>) -> Document {
    let dom = parse_document(RcDom::default(), Default::default())
        .from_utf8()
        .read_from(&mut &html[..])
        .expect("reading from a slice cannot fail");

    // The sheet has to be whole before the first element is styled, and a
    // `<style>` or a `<link>` may sit anywhere in the document, so collecting
    // it is its own pass rather than something the walk picks up as it goes.
    let mut sheets = Sheets {
        sheet: Stylesheet::default(),
        base: &base,
        fetch,
        fetched: 0,
    };
    sheets.collect(&dom.document);
    let sheet = sheets.sheet;

    let mut builder = Builder {
        base,
        title: String::new(),
        blocks: Vec::new(),
        runs: Vec::new(),
        kind: BlockKind::Paragraph,
        style: Style::default(),
        lists: Vec::new(),
        pre: false,
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
    }
}

#[derive(Clone, Default)]
struct Style {
    link: Option<String>,
    bold: bool,
    italic: bool,
    code: bool,
}

/// A list being built, so `li` knows whether it is bulleted or numbered.
struct List {
    ordered: bool,
    next: usize,
}

struct Builder {
    base: Url,
    title: String,
    blocks: Vec<Block>,
    runs: Vec<Run>,
    kind: BlockKind,
    style: Style,
    lists: Vec<List>,
    pre: bool,
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

impl Builder {
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

                let saved_style = self.style.clone();
                let saved_pre = self.pre;
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
                    local_name!("pre") => self.pre = true,
                    local_name!("a") => {
                        self.style.link = attr(node, "href").and_then(|h| self.resolve(&h))
                    }
                    local_name!("b") | local_name!("strong") => self.style.bold = true,
                    local_name!("i") | local_name!("em") => self.style.italic = true,
                    local_name!("code") | local_name!("kbd") | local_name!("samp") => {
                        self.style.code = true
                    }
                    local_name!("br") => self.push_text("\n"),
                    local_name!("img") => {
                        // No decode yet, so an image contributes what a reader
                        // with images off would see.
                        if let Some(alt) = attr(node, "alt").filter(|a| !a.trim().is_empty()) {
                            self.push_text(&format!("[{}]", alt.trim()));
                        }
                    }
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
                self.pre = saved_pre;
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

    /// The depth and marker for an `li`, consuming an ordered list's counter.
    fn marker(&mut self) -> (usize, Marker) {
        let depth = self.lists.len().saturating_sub(1);
        match self.lists.last_mut() {
            Some(list) if list.ordered => {
                let n = list.next;
                list.next += 1;
                (depth, Marker::Number(n))
            }
            _ => (depth, Marker::Bullet),
        }
    }

    fn resolve(&self, href: &str) -> Option<String> {
        let href = href.trim();
        if href.is_empty() || href.starts_with('#') {
            return None;
        }
        self.base.join(href).ok().map(|u| u.to_string())
    }

    /// Append text to the open block, collapsing whitespace the way
    /// `white-space: normal` does outside `pre`.
    fn push_text(&mut self, text: &str) {
        if self.pre {
            self.append(text);
            return;
        }
        let mut out = String::with_capacity(text.len());
        let mut space = self.ends_with_space();
        for ch in text.chars() {
            if ch.is_whitespace() {
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
            });
            return;
        }
        let mut runs = trim_edges(runs);
        if runs.is_empty() {
            return;
        }
        runs.retain(|r| !r.text.is_empty());
        self.blocks.push(Block { kind, runs, css });
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

/// What a selector can ask about an element: its tag, its id, its classes.
fn element_context(node: &Handle, tag: &html5ever::LocalName) -> Element {
    Element {
        tag: tag.to_string(),
        id: attr(node, "id").map(|id| id.trim().to_string()),
        classes: attr(node, "class")
            .unwrap_or_default()
            .split_whitespace()
            .map(str::to_string)
            .collect(),
    }
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
        // A `media` this cannot evaluate is refused for the reason `@media` is
        // skipped: a print sheet applied to the screen is worse than no sheet.
        if let Some(media) = attr(node, "media")
            && !matches!(media.trim().to_lowercase().as_str(), "" | "all" | "screen")
        {
            return;
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
