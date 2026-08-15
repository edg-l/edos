//! A CSS subset: the cascade, and the declarations that change how text is set.
//!
//! Stage 2 of `doc/design/browser.md`. What is here is chosen by what the block
//! list can already express -- colour, size, weight, face, decoration,
//! alignment, case, the first-line indent, the leading, the tracking a page
//! puts between letters and words,
//! the vertical margins between blocks, the measure a box asks for
//! with `width` or `max-width`, and the box a block paints for itself with
//! `background-color`, `padding` and `border` -- plus `display: none`, the one
//! declaration a document needs honoured before anything else, since a page
//! that hides its skip-links and its mobile navigation with CSS renders them as
//! stray text otherwise.
//!
//! Everything unrecognised is dropped rather than approximated. A declaration
//! this cannot represent is invisible, which is the same outcome the document
//! gets from a browser that never implemented it.

use std::{borrow::Cow, collections::BTreeMap, rc::Rc};

/// What CSS calls `medium`, the width a border written with a style but no
/// width is painted at.
const MEDIUM_BORDER: u32 = 3;

/// The four edges of a box, in the order the 1-to-4 value shorthands write
/// them: top, right, bottom, left.
#[derive(Clone, Copy, PartialEq, Debug, Default)]
pub struct Sides<T> {
    pub top: T,
    pub right: T,
    pub bottom: T,
    pub left: T,
}

/// Where a line sits in the box it was laid out in.
#[derive(Clone, Copy, PartialEq, Debug, Default)]
pub enum Align {
    #[default]
    Left,
    Center,
    Right,
}

/// One edge's border. Nothing is painted without a style, whatever the width
/// says, which is why the style is tracked separately from the thickness.
#[derive(Clone, Copy, PartialEq, Debug, Default)]
pub struct Border {
    pub width: Option<u32>,
    /// A `border-style` other than `none` or `hidden` was written.
    pub on: bool,
    /// `None` is `currentColor`, resolved against the text colour by whoever
    /// paints it, since the colour a theme sets is not known here.
    pub color: Option<u32>,
}

impl Border {
    /// The thickness this is painted at, zero when it is not painted at all.
    pub fn px(&self) -> u32 {
        if self.on {
            self.width.unwrap_or(MEDIUM_BORDER)
        } else {
            0
        }
    }
}

/// `line-height`, the leading a box sets for the lines inside it.
#[derive(Clone, Copy, PartialEq, Debug, Default)]
pub enum LineHeight {
    /// The face decides, which is what the reader typography already gives.
    #[default]
    Normal,
    /// A unitless number, kept as the factor rather than resolved here: it
    /// inherits as the factor, so a heading inside a body set `1.6` is led in
    /// proportion to its own size and not to the body's.
    Scale(f32),
    Px(u32),
}

impl LineHeight {
    /// The leading for text set at `font_px`, or `None` when the face decides.
    pub fn px(self, font_px: u32) -> Option<u32> {
        match self {
            LineHeight::Normal => None,
            // Clamped to a pixel: `line-height: 0` is legal and stacks every
            // line on the one above it, which is not a rendering.
            LineHeight::Scale(factor) => Some(((font_px as f32 * factor).round() as u32).max(1)),
            LineHeight::Px(px) => Some(px.max(1)),
        }
    }
}

/// `white-space`, which decides two independent things: whether the source's
/// spaces and newlines survive into the rendering, and whether a line may be
/// broken to fit the column.
#[derive(Clone, Copy, PartialEq, Debug, Default)]
pub enum WhiteSpace {
    #[default]
    Normal,
    /// Collapsed like `normal`, but set on one line however wide it gets.
    NoWrap,
    /// Every space and newline kept, and no wrapping: the source is the layout.
    Pre,
    /// Spaces and newlines kept, and long lines still wrapped to the column.
    PreWrap,
    /// Newlines kept, runs of spaces collapsed.
    PreLine,
}

impl WhiteSpace {
    /// Whether a newline in the source starts a line in the rendering.
    pub fn keeps_newlines(self) -> bool {
        matches!(
            self,
            WhiteSpace::Pre | WhiteSpace::PreWrap | WhiteSpace::PreLine
        )
    }

    /// Whether a run of spaces is set at its own width rather than as one.
    pub fn keeps_spaces(self) -> bool {
        matches!(self, WhiteSpace::Pre | WhiteSpace::PreWrap)
    }

    /// Whether a line too wide for the column is broken.
    pub fn wraps(self) -> bool {
        !matches!(self, WhiteSpace::Pre | WhiteSpace::NoWrap)
    }
}

/// What a line breaker may do with a word too wide for the space left on the
/// line, which is `word-break` and `overflow-wrap` resolved into one answer.
#[derive(Clone, Copy, PartialEq, Debug, Default)]
pub enum Wrap {
    /// A word is never cut: one wider than the column overflows it, on the
    /// grounds that a URL broken across lines reads worse than a ragged edge.
    #[default]
    Word,
    /// `overflow-wrap: break-word`: a word is cut only as a last resort, when
    /// a line of its own would not hold it either.
    Overflow,
    /// `word-break: break-all`: the line is filled to the column edge and the
    /// word cut wherever that falls.
    Anywhere,
}

impl Wrap {
    /// Whether a word that does not fit the space left may be cut where it
    /// stands. `alone` says the word would not fit an empty line either, which
    /// is the only case `overflow-wrap: break-word` cuts in.
    pub fn breaks(self, alone: bool) -> bool {
        match self {
            Wrap::Word => false,
            Wrap::Overflow => alone,
            Wrap::Anywhere => true,
        }
    }
}

/// `list-style-type`, the marker a list item wears.
///
/// The counting styles are kept apart from the bullets because only they read
/// the item's position: a `ul` given `lower-roman` numbers its items, and an
/// `ol` given `square` does not, whatever the element says.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ListStyle {
    None,
    Disc,
    Circle,
    Square,
    Decimal,
    DecimalLeadingZero,
    LowerAlpha,
    UpperAlpha,
    LowerRoman,
    UpperRoman,
}

impl ListStyle {
    /// The marker for the `n`th item of a list, without its trailing space.
    /// Empty for `none`, which is what a page hiding its navigation bullets
    /// asks for.
    pub fn marker(self, n: usize) -> String {
        match self {
            ListStyle::None => String::new(),
            ListStyle::Disc => "\u{2022}".to_string(),
            ListStyle::Circle => "\u{25e6}".to_string(),
            ListStyle::Square => "\u{25aa}".to_string(),
            ListStyle::Decimal => format!("{n}."),
            ListStyle::DecimalLeadingZero => format!("{n:02}."),
            ListStyle::LowerAlpha => format!("{}.", alphabetic(n, 'a')),
            ListStyle::UpperAlpha => format!("{}.", alphabetic(n, 'A')),
            ListStyle::LowerRoman => format!("{}.", roman(n).to_lowercase()),
            ListStyle::UpperRoman => format!("{}.", roman(n)),
        }
    }

    /// The same marker for a plain-text rendering, where the bullet glyphs are
    /// spelled with the characters a terminal is certain to have.
    pub fn ascii_marker(self, n: usize) -> String {
        match self {
            ListStyle::Disc => "*".to_string(),
            ListStyle::Circle => "o".to_string(),
            ListStyle::Square => "-".to_string(),
            _ => self.marker(n),
        }
    }
}

/// `n` in the bijective base-26 CSS calls `lower-alpha`: a, b, ... z, aa, ab.
/// Zero has no representation, so it falls back to the decimal CSS Counter
/// Styles §5 asks for when a counter is outside its style's range.
fn alphabetic(n: usize, first: char) -> String {
    if n == 0 {
        return "0".to_string();
    }
    let mut n = n;
    let mut out = Vec::new();
    while n > 0 {
        let digit = (n - 1) % 26;
        out.push((first as u8 + digit as u8) as char);
        n = (n - 1) / 26;
    }
    out.iter().rev().collect()
}

/// `n` in upper-case Roman numerals. The style's range is 1 to 3999, and a
/// counter outside it is written in decimal instead, per CSS Counter Styles §5.
fn roman(n: usize) -> String {
    const DIGITS: [(usize, &str); 13] = [
        (1000, "M"),
        (900, "CM"),
        (500, "D"),
        (400, "CD"),
        (100, "C"),
        (90, "XC"),
        (50, "L"),
        (40, "XL"),
        (10, "X"),
        (9, "IX"),
        (5, "V"),
        (4, "IV"),
        (1, "I"),
    ];
    if !(1..=3999).contains(&n) {
        return n.to_string();
    }
    let mut n = n;
    let mut out = String::new();
    for (value, glyph) in DIGITS {
        while n >= value {
            out.push_str(glyph);
            n -= value;
        }
    }
    out
}

/// A `list-style-type` keyword, or `None` for one this cannot draw.
fn parse_list_style(value: &str) -> Option<ListStyle> {
    Some(match value {
        "none" => ListStyle::None,
        "disc" => ListStyle::Disc,
        "circle" => ListStyle::Circle,
        "square" => ListStyle::Square,
        "decimal" => ListStyle::Decimal,
        "decimal-leading-zero" => ListStyle::DecimalLeadingZero,
        "lower-alpha" | "lower-latin" => ListStyle::LowerAlpha,
        "upper-alpha" | "upper-latin" => ListStyle::UpperAlpha,
        "lower-roman" => ListStyle::LowerRoman,
        "upper-roman" => ListStyle::UpperRoman,
        _ => return None,
    })
}

/// The box `display` asks for, reduced to what a block-and-inline model can
/// answer. `display: none` is not here: it is `Computed::hidden`, which drops
/// the box rather than naming one.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Display {
    /// An inline-level box, which stays in the line its parent block is
    /// building. `inline-block` and friends land here too: the inline model is
    /// flat, so an inline-level box of any kind is a run of text.
    Inline,
    /// A block-level box, which breaks the line and starts one of its own.
    /// A layout mode the box engine does not implement — `table` and its
    /// parts — is a block, because that is the part of it a block can honour.
    Block,
    /// A block-level box that also draws a marker. `<li>` gets this from the
    /// UA stylesheet, which is why `li { display: block }` loses its bullet.
    ListItem,
    /// A block-level box that arranges its children along an axis.
    Flex,
    /// A block-level box that arranges its children on a track grid.
    Grid,
}

/// `flex-direction`: the axis a flex container lays its children along.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum FlexDirection {
    Row,
    RowReverse,
    Column,
    ColumnReverse,
}

/// `justify-content`: how leftover space along the main axis is distributed.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Justify {
    Start,
    End,
    Center,
    SpaceBetween,
    SpaceAround,
    SpaceEvenly,
}

/// `align-items`: where a child sits across the axis.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum AlignItems {
    Start,
    End,
    Center,
    Stretch,
}

/// The column tracks of a `grid-template-columns`, bounded so that [`Computed`]
/// stays `Copy` -- it is passed by value on every element, and a heap list here
/// would put an allocation on that path.
///
/// A template naming more than [`Tracks::MAX`] columns is refused rather than
/// truncated: a grid laid out on some of its tracks is not the page's grid, and
/// silently dropping the rest reads as a layout bug.
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct Tracks {
    items: [Track; Tracks::MAX],
    len: u8,
}

impl Tracks {
    pub const MAX: usize = 16;

    fn from_slice(list: &[Track]) -> Option<Tracks> {
        if list.is_empty() || list.len() > Tracks::MAX {
            return None;
        }
        let mut items = [Track::Auto; Tracks::MAX];
        items[..list.len()].copy_from_slice(list);
        Some(Tracks {
            items,
            len: list.len() as u8,
        })
    }

    pub fn as_slice(&self) -> &[Track] {
        &self.items[..self.len as usize]
    }
}

/// One column track of `grid-template-columns`.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Track {
    /// A fixed length in pixels.
    Px(u32),
    /// A share of the leftover space: the `fr` unit.
    Fr(f32),
    /// `auto`, which takes what its content asks for.
    Auto,
}

/// `display`, as a keyword the box model can act on. A two-keyword value
/// (`inline flow-root`) is answered by the first keyword that names something,
/// which is the outer display type in every ordering CSS allows.
/// css-display-3 §2.
fn parse_display(value: &str) -> Option<Option<Display>> {
    value.split_whitespace().find_map(|word| {
        Some(match word {
            "none" => None,
            // `contents` drops the box and keeps the children, which in a flat
            // inline model is what an inline box that opens nothing does.
            "inline" | "inline-block" | "inline-flex" | "inline-grid" | "inline-table"
            | "contents" | "table-cell" | "ruby" => Some(Display::Inline),
            "flex" => Some(Display::Flex),
            "grid" => Some(Display::Grid),
            "block" | "flow-root" | "table" | "table-row" | "table-row-group"
            | "table-header-group" | "table-footer-group" | "table-caption" => Some(Display::Block),
            "list-item" => Some(Display::ListItem),
            _ => return None,
        })
    })
}

/// `text-transform`, the case a box sets for the text inside it.
#[derive(Clone, Copy, PartialEq, Debug, Default)]
pub enum Transform {
    #[default]
    None,
    Upper,
    Lower,
    /// The first letter after every word boundary. A run glued mid-word to the
    /// one before it is left alone by whoever sets the text, since the letter
    /// this would raise is in the middle of the word the reader sees.
    Capitalize,
}

impl Transform {
    /// `word` recased, borrowed unchanged when nothing is to be done.
    pub fn apply(self, word: &str) -> Cow<'_, str> {
        match self {
            Transform::None => Cow::Borrowed(word),
            Transform::Upper => Cow::Owned(word.to_uppercase()),
            Transform::Lower => Cow::Owned(word.to_lowercase()),
            Transform::Capitalize => {
                let mut out = String::with_capacity(word.len());
                let mut boundary = true;
                for ch in word.chars() {
                    if boundary && ch.is_alphanumeric() {
                        out.extend(ch.to_uppercase());
                    } else {
                        out.push(ch);
                    }
                    // An apostrophe is inside a word, not between two: without
                    // this `it's` is set `It'S`.
                    boundary = !(ch.is_alphanumeric() || ch == '\'' || ch == '\u{2019}');
                }
                Cow::Owned(out)
            }
        }
    }
}

/// The lines `text-decoration` draws across a run (css-text-decor-3 §2.1).
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Decorations {
    pub underline: bool,
    pub line_through: bool,
    pub overline: bool,
}

impl Decorations {
    /// Both sets of lines at once, which is how a decoration an element wears
    /// by being a `<del>` joins one it inherited.
    pub fn merged(self, other: Decorations) -> Decorations {
        Decorations {
            underline: self.underline || other.underline,
            line_through: self.line_through || other.line_through,
            overline: self.overline || other.overline,
        }
    }
}

/// A property value that a `Computed` carries after the cascade.
#[derive(Clone, Copy, PartialEq, Debug, Default)]
pub struct Computed {
    pub color: Option<u32>,
    /// Resolved to absolute pixels, since `em` needs a parent to resolve
    /// against and no later stage has one.
    pub font_px: Option<u32>,
    pub bold: Option<bool>,
    pub italic: Option<bool>,
    pub mono: Option<bool>,
    /// `text-decoration`. `None` is "the page said nothing", which leaves the
    /// lines to the element: a link is underlined, a `<del>` struck through.
    pub decoration: Option<Decorations>,
    /// `line-height`, which inherits the way the property itself does.
    pub line: LineHeight,
    pub margin_top: Option<u32>,
    pub margin_bottom: Option<u32>,
    pub margin_left: Option<u32>,
    pub margin_right: Option<u32>,
    /// The measure `width` and `max-width` put on this box, in pixels, already
    /// narrowed by every ancestor's.
    ///
    /// It inherits, unlike the property it comes from, because the block list
    /// is flat: a wrapper that constrains its column is not a box any later
    /// stage sees, so its measure has no other way to reach the paragraphs
    /// inside it.
    pub measure: Option<u32>,
    /// `height`: the border box's own size, which content taller than it
    /// overflows rather than being cut, since `overflow` is visible.
    pub height: Option<u32>,
    /// How this box arranges its children, when it has any: the flex axis and
    /// the alignment along and across it, the grid's column tracks, and the
    /// gutter between items. Read by the box engine off a container and by
    /// nothing else, which is why none of it inherits.
    pub flex_direction: Option<FlexDirection>,
    pub justify: Option<Justify>,
    pub align_items: Option<AlignItems>,
    pub gap: Option<u32>,
    pub grid_columns: Option<Tracks>,
    /// `min-height` and `max-height`, the floor and ceiling that box size is
    /// clamped between. css-sizing-3 §5.
    pub min_height: Option<u32>,
    pub max_height: Option<u32>,
    /// `min-width`: a floor under the box's own width, which css-sizing-3 §5.1
    /// makes win over `max-width`. Unlike `measure` it does not inherit, since
    /// widening a descendant is not what a wrapper's floor asks for.
    pub min_width: Option<u32>,
    /// A horizontal margin written `auto`, which centres the box in its column.
    /// Inherited for the same reason `measure` is.
    pub center: bool,
    /// `text-align`, which inherits the way the property itself does.
    pub align: Align,
    /// `text-transform`, likewise inherited.
    pub transform: Transform,
    /// `list-style-type`, inherited so a rule on the list reaches its items.
    /// `None` is "the page said nothing", which leaves the marker to the kind
    /// of list the element opened and how deeply it is nested.
    pub list_style: Option<ListStyle>,
    /// `white-space`, inherited. `None` is "the page said nothing", which is
    /// what lets `<pre>` carry the UA default without the cascade knowing about
    /// element names: an author rule anywhere on the box overrides it.
    pub white_space: Option<WhiteSpace>,
    /// `word-break: break-all`, inherited. Kept apart from `break_word` because
    /// the two properties are independent: one asks for a cut wherever the line
    /// ends, the other only for one the column could not otherwise hold.
    pub break_all: bool,
    /// `overflow-wrap: break-word` (or `anywhere`), inherited.
    pub break_word: bool,
    /// `text-indent`: how far into the box the block's first line starts. It
    /// inherits, so a wrapper that sets it indents the paragraphs inside it.
    pub indent: u32,
    /// `letter-spacing`, inherited, and signed: a page tightening a display
    /// heading writes a negative value and means it.
    pub letter_spacing: i32,
    /// `word-spacing`, inherited, added to every space between two words.
    pub word_spacing: i32,
    /// `vertical-align` as a baseline shift in pixels, positive raising the
    /// run. `None` is "the page said nothing", which leaves the shift to the
    /// element: a `<sup>` rises, a `<sub>` drops.
    ///
    /// It inherits, unlike the property it comes from, because the inline
    /// model here is flat: a `<sup>` is not a box any later stage sees, so the
    /// `<b>` inside it has no other way to learn it is set as a superscript.
    pub shift: Option<i32>,
    /// `background-color`, painted behind the block's own box.
    pub background: Option<u32>,
    pub padding: Sides<Option<u32>>,
    pub borders: Sides<Border>,
    /// `display: none`. Not inherited: a hidden element hides its subtree by
    /// not being walked at all, which is not the same thing as its children
    /// inheriting a value.
    pub hidden: bool,
    /// `visibility: hidden`, which keeps the box in flow and only stops it
    /// being painted, per css-display-3 §3. It inherits, and a child setting
    /// `visible` comes back: that is the one way a subtree can be partly
    /// hidden, and it is why this cannot be answered as `display: none`.
    pub invisible: bool,
    /// `display`, when the page named a box other than the element's own.
    /// Not inherited. `None` is "the page said nothing", which leaves the box
    /// to the element: a `<div>` blocks, a `<span>` does not.
    pub display: Option<Display>,
}

impl Computed {
    /// The starting point for a child: the inherited properties survive, the
    /// rest resets.
    pub fn inherit(&self) -> Computed {
        Computed {
            hidden: false,
            display: None,
            height: None,
            min_height: None,
            max_height: None,
            min_width: None,
            margin_top: None,
            margin_bottom: None,
            margin_left: None,
            margin_right: None,
            background: None,
            padding: Sides::default(),
            borders: Sides::default(),
            ..*self
        }
    }

    /// The two break properties as the one answer the line breaker needs.
    /// `word-break` wins where both were set, since it asks for the cut in
    /// strictly more cases than `overflow-wrap` does.
    pub fn wrap(&self) -> Wrap {
        match (self.break_all, self.break_word) {
            (true, _) => Wrap::Anywhere,
            (false, true) => Wrap::Overflow,
            (false, false) => Wrap::Word,
        }
    }

    /// The font size this resolves relative lengths against.
    fn em(&self, root_px: u32) -> u32 {
        self.font_px.unwrap_or(root_px)
    }

    /// `basis` is the containing block's measure, which is what a percentage
    /// width is a percentage of.
    fn apply(&mut self, name: &str, value: &str, root_px: u32, parent_px: u32, basis: u32) {
        match name {
            "color" => {
                if let Some(color) = parse_color(value) {
                    self.color = Some(color);
                }
            }
            "display" => {
                if let Some(display) = parse_display(&value.to_ascii_lowercase()) {
                    self.hidden = display.is_none();
                    self.display = display;
                }
            }
            // `collapse` is `hidden` everywhere outside a table, and there are
            // no tables here.
            "visibility" => match value.to_ascii_lowercase().as_str() {
                "hidden" | "collapse" => self.invisible = true,
                "visible" => self.invisible = false,
                _ => {}
            },
            "font-size" => {
                if let Some(px) = parse_font_size(value, root_px, parent_px) {
                    self.font_px = Some(px);
                }
            }
            "font-weight" => self.bold = parse_weight(value).or(self.bold),
            "font-style" => match value {
                "italic" | "oblique" => self.italic = Some(true),
                "normal" => self.italic = Some(false),
                _ => {}
            },
            "font-family" => self.mono = Some(value.contains("monospace")),
            // A relative `line-height` is of the element's own font size, not
            // its parent's, which is what `self.em` gives once `font-size` has
            // been applied.
            "line-height" => {
                if let Some(line) = parse_line_height(value, root_px, self.em(parent_px)) {
                    self.line = line;
                }
            }
            // The shorthand also carries a colour, a style and a thickness
            // (css-text-decor-3 §2.5); only the line keywords are read, and
            // every line the value names is drawn.
            "text-decoration" | "text-decoration-line" => {
                let mut lines = Decorations::default();
                for word in value.split_whitespace() {
                    match word {
                        "underline" => lines.underline = true,
                        "line-through" => lines.line_through = true,
                        "overline" => lines.overline = true,
                        _ => {}
                    }
                }
                self.decoration = Some(lines);
            }
            // `justify` is set flush left here: the last line of a justified
            // paragraph is left aligned anyway, and stretching the others needs
            // per-space positioning the blitter does not offer.
            "flex-direction" => {
                self.flex_direction = match value {
                    "row" => Some(FlexDirection::Row),
                    "row-reverse" => Some(FlexDirection::RowReverse),
                    "column" => Some(FlexDirection::Column),
                    "column-reverse" => Some(FlexDirection::ColumnReverse),
                    _ => return,
                }
            }
            "justify-content" => {
                self.justify = match value {
                    "flex-start" | "start" | "left" | "normal" => Some(Justify::Start),
                    "flex-end" | "end" | "right" => Some(Justify::End),
                    "center" => Some(Justify::Center),
                    "space-between" => Some(Justify::SpaceBetween),
                    "space-around" => Some(Justify::SpaceAround),
                    "space-evenly" => Some(Justify::SpaceEvenly),
                    _ => return,
                }
            }
            "align-items" => {
                self.align_items = match value {
                    "flex-start" | "start" | "self-start" => Some(AlignItems::Start),
                    "flex-end" | "end" | "self-end" => Some(AlignItems::End),
                    "center" => Some(AlignItems::Center),
                    "stretch" | "normal" => Some(AlignItems::Stretch),
                    _ => return,
                }
            }
            // `gap` takes a row and a column value; one gutter is what this
            // engine offers, so the first is taken and a differing second is
            // ignored rather than silently applied to both axes.
            "gap" | "grid-gap" | "row-gap" | "column-gap" => {
                if let Some(px) = value
                    .split_whitespace()
                    .next()
                    .and_then(|v| parse_length(v, root_px, self.em(parent_px)))
                {
                    self.gap = Some(px);
                }
            }
            "grid-template-columns" => {
                if let Some(tracks) = parse_tracks(value, root_px, self.em(parent_px)) {
                    self.grid_columns = Some(tracks);
                }
            }
            "text-align" => match value {
                "left" | "start" | "justify" => self.align = Align::Left,
                "center" => self.align = Align::Center,
                "right" | "end" => self.align = Align::Right,
                _ => {}
            },
            "white-space" => match value {
                "normal" => self.white_space = Some(WhiteSpace::Normal),
                "nowrap" => self.white_space = Some(WhiteSpace::NoWrap),
                "pre" => self.white_space = Some(WhiteSpace::Pre),
                "pre-wrap" | "break-spaces" => self.white_space = Some(WhiteSpace::PreWrap),
                "pre-line" => self.white_space = Some(WhiteSpace::PreLine),
                _ => {}
            },
            // `keep-all` forbids the breaks it has, which are the CJK ones the
            // breaker does not take anyway, so it lands on `normal`.
            "word-break" => match value {
                "break-all" => self.break_all = true,
                "normal" | "keep-all" => self.break_all = false,
                _ => {}
            },
            // `anywhere` differs from `break-word` only in what it does to a
            // box's intrinsic width, which nothing here measures.
            "overflow-wrap" | "word-wrap" => match value {
                "break-word" | "anywhere" => self.break_word = true,
                "normal" => self.break_word = false,
                _ => {}
            },
            "list-style-type" => {
                if let Some(style) = parse_list_style(value) {
                    self.list_style = Some(style);
                }
            }
            // The shorthand resets every component it leaves out, so one
            // written with only a position or an image still puts the type
            // back to `disc`.
            "list-style" => {
                let words: Vec<&str> = value.split_whitespace().collect();
                let named = words.iter().find_map(|word| parse_list_style(word));
                let other = words
                    .iter()
                    .any(|word| matches!(*word, "inside" | "outside") || word.starts_with("url("));
                if let Some(style) = named.or(other.then_some(ListStyle::Disc)) {
                    self.list_style = Some(style);
                }
            }
            "text-transform" => match value {
                "none" => self.transform = Transform::None,
                "uppercase" => self.transform = Transform::Upper,
                "lowercase" => self.transform = Transform::Lower,
                "capitalize" => self.transform = Transform::Capitalize,
                _ => {}
            },
            // A percentage is of the containing block, and a negative value --
            // a hanging indent -- resolves to zero rather than drawing into the
            // page margin, which is the one place the box has nothing to hang
            // over.
            "text-indent" => {
                if let Some(px) = parse_measure(value, root_px, self.em(parent_px), basis) {
                    self.indent = px;
                }
            }
            // Both take a signed length, and both are relative to the element's
            // own font size, so a heading that tightens by `-0.02em` tightens by
            // its own em rather than the parent's.
            "letter-spacing" => {
                if let Some(px) = parse_spacing(value, root_px, self.em(parent_px)) {
                    self.letter_spacing = px;
                }
            }
            "word-spacing" => {
                if let Some(px) = parse_spacing(value, root_px, self.em(parent_px)) {
                    self.word_spacing = px;
                }
            }
            // `super` and `sub` are font-relative and left to the face by
            // css-inline-3 §3.3; the fractions here are the usual ones. The
            // keywords that align against the line box rather than a baseline
            // -- `top`, `middle`, `bottom` and their `text-` forms -- have no
            // expression in a flat inline model, so they leave the run where
            // the baseline puts it rather than being silently misplaced.
            "vertical-align" => {
                let em = self.em(parent_px) as i32;
                let resolved = match value {
                    "super" => Some(em / 3),
                    "sub" => Some(-em / 5),
                    "baseline" | "top" | "middle" | "bottom" | "text-top" | "text-bottom" => {
                        Some(0)
                    }
                    _ => parse_signed_length(value, root_px, self.em(parent_px)),
                };
                if let Some(px) = resolved {
                    self.shift = Some(px);
                }
            }
            "margin" => {
                let written: Vec<&str> = value.split_whitespace().collect();
                let sides = quad(&self.lengths(&written, root_px, parent_px));
                let horizontal = match written.len() {
                    1 => Some(written[0]),
                    2 | 3 => Some(written[1]),
                    4 => Some(written[3]),
                    _ => None,
                };
                if horizontal.is_some_and(is_auto) {
                    self.center = true;
                }
                self.margin_top = sides.top.or(self.margin_top);
                self.margin_bottom = sides.bottom.or(self.margin_bottom);
                self.margin_left = sides.left.or(self.margin_left);
                self.margin_right = sides.right.or(self.margin_right);
            }
            "margin-top" => self.margin_top = self.length(value, root_px, parent_px),
            "margin-bottom" => self.margin_bottom = self.length(value, root_px, parent_px),
            // A box with only one auto horizontal margin is pushed to the other
            // side rather than centred, but a page that writes one means the
            // pair: the other half is in the shorthand or in a rule this cannot
            // see, and a centred box is what it was after either way.
            "margin-right" => {
                if is_auto(value) {
                    self.center = true;
                } else {
                    self.margin_right = self.length(value, root_px, parent_px);
                }
            }
            "margin-left" => {
                if is_auto(value) {
                    self.center = true;
                } else {
                    self.margin_left = self.length(value, root_px, parent_px);
                }
            }
            "padding" => {
                let written: Vec<&str> = value.split_whitespace().collect();
                let sides = quad(&self.lengths(&written, root_px, parent_px));
                self.padding = Sides {
                    top: sides.top.or(self.padding.top),
                    right: sides.right.or(self.padding.right),
                    bottom: sides.bottom.or(self.padding.bottom),
                    left: sides.left.or(self.padding.left),
                };
            }
            "padding-top" => self.padding.top = self.length(value, root_px, parent_px),
            "padding-right" => self.padding.right = self.length(value, root_px, parent_px),
            "padding-bottom" => self.padding.bottom = self.length(value, root_px, parent_px),
            "padding-left" => self.padding.left = self.length(value, root_px, parent_px),
            "background" | "background-color" => {
                // A `background` shorthand is mostly things this cannot paint
                // -- images, gradients, positions -- so its colour is taken
                // from whichever token is one and the rest is dropped.
                let color =
                    parse_color(value).or_else(|| value.split_whitespace().find_map(parse_color));
                if let Some(color) = color {
                    self.background = Some(color);
                }
            }
            "border" => {
                let border = self.border_shorthand(value, root_px, parent_px);
                self.borders = Sides {
                    top: border,
                    right: border,
                    bottom: border,
                    left: border,
                };
            }
            "border-top" => self.borders.top = self.border_shorthand(value, root_px, parent_px),
            "border-right" => self.borders.right = self.border_shorthand(value, root_px, parent_px),
            "border-bottom" => {
                self.borders.bottom = self.border_shorthand(value, root_px, parent_px)
            }
            "border-left" => self.borders.left = self.border_shorthand(value, root_px, parent_px),
            "border-width" => {
                let written: Vec<&str> = value.split_whitespace().collect();
                let parts: Vec<Option<u32>> = written
                    .iter()
                    .map(|p| border_width(p, root_px, self.em(parent_px)))
                    .collect();
                self.each_border(quad(&parts), |border, px| border.width = Some(px));
            }
            "border-color" => {
                let parts: Vec<Option<u32>> = value.split_whitespace().map(parse_color).collect();
                self.each_border(quad(&parts), |border, color| border.color = Some(color));
            }
            "border-style" => {
                let parts: Vec<Option<bool>> = value.split_whitespace().map(border_style).collect();
                self.each_border(quad(&parts), |border, on| border.on = on);
            }
            // Neither can widen the box: an ancestor's measure is the bound,
            // and `auto`, `none` and anything unparseable leave it alone.
            "width" | "max-width" => {
                if let Some(px) = parse_measure(value, root_px, self.em(parent_px), basis) {
                    self.measure = Some(self.measure.map_or(px, |narrower| narrower.min(px)));
                }
            }
            // A percentage `min-width` is of the containing block's width,
            // which the basis already carries.
            "min-width" => {
                self.min_width = parse_measure(value, root_px, self.em(parent_px), basis);
            }
            // A percentage height is of the containing block's height, which a
            // flowed column never has: css-sizing-3 §5.1 makes that case behave
            // as `auto`, so only an absolute length is taken here.
            "height" => self.height = self.absolute(value, root_px, parent_px),
            "min-height" => self.min_height = self.absolute(value, root_px, parent_px),
            "max-height" => self.max_height = self.absolute(value, root_px, parent_px),
            _ => {}
        }
    }

    fn length(&self, value: &str, root_px: u32, parent_px: u32) -> Option<u32> {
        parse_length(value, root_px, self.em(parent_px))
    }

    /// A length with no percentage form, for the properties whose percentage
    /// resolves against a size this layout never has.
    fn absolute(&self, value: &str, root_px: u32, parent_px: u32) -> Option<u32> {
        if value.trim().ends_with('%') {
            return None;
        }
        self.length(value, root_px, parent_px)
    }

    fn lengths(&self, written: &[&str], root_px: u32, parent_px: u32) -> Vec<Option<u32>> {
        written
            .iter()
            .map(|part| self.length(part, root_px, parent_px))
            .collect()
    }

    /// `border: <width> || <style> || <color>`, in any order and with any of
    /// the three left out.
    fn border_shorthand(&self, value: &str, root_px: u32, parent_px: u32) -> Border {
        let mut border = Border::default();
        for token in value.split_whitespace() {
            if let Some(on) = border_style(token) {
                border.on = on;
            } else if let Some(px) = border_width(token, root_px, self.em(parent_px)) {
                border.width = Some(px);
            } else if let Some(color) = parse_color(token) {
                border.color = Some(color);
            }
        }
        border
    }

    /// Apply the written sides of a per-edge longhand, leaving the edges the
    /// declaration said nothing about alone.
    fn each_border<T: Copy>(&mut self, values: Sides<Option<T>>, set: impl Fn(&mut Border, T)) {
        let edges = [
            (&mut self.borders.top, values.top),
            (&mut self.borders.right, values.right),
            (&mut self.borders.bottom, values.bottom),
            (&mut self.borders.left, values.left),
        ];
        for (border, value) in edges {
            if let Some(value) = value {
                set(border, value);
            }
        }
    }
}

/// The 1-to-4 value shorthand `margin`, `padding` and the `border-*` longhands
/// are all written in: one value sets all four edges, two set vertical then
/// horizontal, and three or four start at the top and go clockwise.
fn quad<T: Copy>(parts: &[Option<T>]) -> Sides<Option<T>> {
    let at = |index: usize| parts.get(index).copied().flatten();
    match parts.len() {
        1 => Sides {
            top: at(0),
            right: at(0),
            bottom: at(0),
            left: at(0),
        },
        2 => Sides {
            top: at(0),
            right: at(1),
            bottom: at(0),
            left: at(1),
        },
        3 => Sides {
            top: at(0),
            right: at(1),
            bottom: at(2),
            left: at(1),
        },
        4 => Sides {
            top: at(0),
            right: at(1),
            bottom: at(2),
            left: at(3),
        },
        _ => Sides::default(),
    }
}

/// Whether a `border-style` keyword paints anything. Every style that does is
/// painted solid: a dashed hairline and a solid one carry the same meaning at
/// this size, and the reader cannot tell a groove from a ridge either.
fn border_style(value: &str) -> Option<bool> {
    match value {
        "none" | "hidden" => Some(false),
        "solid" | "dashed" | "dotted" | "double" | "groove" | "ridge" | "inset" | "outset" => {
            Some(true)
        }
        _ => None,
    }
}

/// A `border-width`: a length, or one of the three keywords CSS gives instead.
fn border_width(value: &str, root_px: u32, em_px: u32) -> Option<u32> {
    match value {
        "thin" => Some(1),
        "medium" => Some(MEDIUM_BORDER),
        "thick" => Some(5),
        _ => parse_length(value, root_px, em_px),
    }
}

/// One `name: value` pair, with the value lowercased and whitespace collapsed.
#[derive(Clone, Debug)]
pub struct Declaration {
    pub name: String,
    pub value: String,
}

impl Declaration {
    /// A custom property, `--name`, which carries no meaning of its own and is
    /// only ever read back through `var()`.
    fn is_custom(&self) -> bool {
        self.name.starts_with("--")
    }
}

/// The custom properties in scope on one element.
///
/// They inherit, and they are resolved *after* the cascade rather than as each
/// declaration is read: a `--x` set by any rule matching this element is in
/// scope for every declaration on it, whichever rule wrote it.
#[derive(Clone, Default, Debug)]
pub struct Vars(BTreeMap<String, String>);

impl Vars {
    /// A scope shared until something declares a property, since a page that
    /// sets its palette once on `:root` would otherwise copy the whole map onto
    /// every element in the document.
    pub fn root() -> Rc<Vars> {
        Rc::new(Vars::default())
    }
}

/// How deep a `var()` may refer through another before it is called a cycle.
const VAR_DEPTH: u32 = 8;

/// How an attribute selector compares the attribute's value, per CSS Selectors
/// 4 §6.
#[derive(Clone, Copy, Debug, PartialEq)]
enum AttrOp {
    /// `[name]`: the attribute is present, whatever it holds.
    Present,
    /// `[name=value]`
    Equals,
    /// `[name~=value]`: one of the whitespace-separated words is `value`.
    Word,
    /// `[name|=value]`: `value`, or `value` followed by a hyphen.
    Dash,
    /// `[name^=value]`
    Prefix,
    /// `[name$=value]`
    Suffix,
    /// `[name*=value]`
    Substring,
}

/// One `[...]` test inside a compound.
#[derive(Clone, Debug)]
struct AttrTest {
    /// Lower-cased: HTML attribute names are matched case-insensitively.
    name: String,
    op: AttrOp,
    value: String,
    /// The `i` flag, which folds the *value* comparison to lower case.
    fold: bool,
}

impl AttrTest {
    fn matches(&self, element: &Element) -> bool {
        let Some(actual) = element.attr(&self.name) else {
            return false;
        };
        if self.op == AttrOp::Present {
            return true;
        }
        // An empty value matches nothing at all for the substring operators,
        // and `~=` additionally rejects a value carrying whitespace, since no
        // single word can contain any.
        let actual = if self.fold {
            actual.to_lowercase()
        } else {
            actual.to_string()
        };
        let value = &self.value;
        match self.op {
            AttrOp::Present => true,
            AttrOp::Equals => actual == *value,
            AttrOp::Word => {
                !value.is_empty()
                    && !value.contains(char::is_whitespace)
                    && actual.split_whitespace().any(|word| word == value)
            }
            AttrOp::Dash => actual == *value || actual.starts_with(&format!("{value}-")),
            AttrOp::Prefix => !value.is_empty() && actual.starts_with(value.as_str()),
            AttrOp::Suffix => !value.is_empty() && actual.ends_with(value.as_str()),
            AttrOp::Substring => !value.is_empty() && actual.contains(value.as_str()),
        }
    }
}

/// One pseudo-class. The structural family of CSS Selectors 4 §9 reduces to two
/// questions, since `:first-child` is `:nth-child(1)` and `:last-of-type` is
/// `:nth-last-of-type(1)`; the logical combinators of §4 are the third.
#[derive(Clone, Debug)]
enum Pseudo {
    /// `:nth-child(An+B)` and its three relatives.
    Nth {
        a: i32,
        b: i32,
        /// Count from the last sibling rather than the first.
        from_end: bool,
        /// Count only the siblings sharing the element's tag.
        of_type: bool,
    },
    /// `:only-child`, or `:only-of-type` when `of_type`.
    Only { of_type: bool },
    /// `:not()`, `:is()` and `:where()` over a selector list, per CSS Selectors
    /// 4 §4. The arguments are compounds: an argument carrying a combinator is
    /// not read, so the rule using it is dropped rather than applied wrongly.
    Logical {
        /// The test passes when any one of these matches.
        any: Vec<Compound>,
        /// `:not()` inverts that answer.
        negate: bool,
        /// `:where()` contributes no specificity at all; the other two
        /// contribute the largest among their arguments.
        weightless: bool,
    },
}

impl Pseudo {
    fn matches(&self, element: &Element) -> bool {
        let at = &element.position;
        match self {
            &Pseudo::Nth {
                a,
                b,
                from_end,
                of_type,
            } => {
                let (index, count) = if of_type {
                    (at.type_index, at.type_count)
                } else {
                    (at.index, at.count)
                };
                if index == 0 || index > count {
                    return false;
                }
                let index = if from_end { count + 1 - index } else { index };
                nth_matches(a, b, index as i32)
            }
            &Pseudo::Only { of_type } => {
                if of_type {
                    at.type_count == 1
                } else {
                    at.count == 1
                }
            }
            Pseudo::Logical { any, negate, .. } => {
                any.iter().any(|c| c.matches(element)) != *negate
            }
        }
    }

    /// What this pseudo-class adds to the compound holding it.
    fn specificity(&self) -> (u32, u32, u32) {
        match self {
            Pseudo::Logical {
                any, weightless, ..
            } => {
                if *weightless {
                    (0, 0, 0)
                } else {
                    any.iter()
                        .map(Compound::specificity)
                        .max()
                        .unwrap_or((0, 0, 0))
                }
            }
            // A structural pseudo-class counts alongside a class
            // (CSS Selectors 4 §17).
            _ => (0, 1, 0),
        }
    }
}

/// Whether `index` is `A*n + B` for some whole `n >= 0`.
fn nth_matches(a: i32, b: i32, index: i32) -> bool {
    let offset = index - b;
    if a == 0 {
        return offset == 0;
    }
    offset % a == 0 && offset / a >= 0
}

/// One simple selector: a tag, an id, any number of classes, attribute tests
/// and pseudo-classes, all of which must match the same element.
#[derive(Clone, Debug, Default)]
struct Compound {
    tag: Option<String>,
    id: Option<String>,
    classes: Vec<String>,
    attrs: Vec<AttrTest>,
    pseudos: Vec<Pseudo>,
}

impl Compound {
    fn matches(&self, element: &Element) -> bool {
        if let Some(tag) = &self.tag
            && tag != &element.tag
        {
            return false;
        }
        if self.id.is_some() && self.id != element.id {
            return false;
        }
        self.classes.iter().all(|c| element.classes.contains(c))
            && self.attrs.iter().all(|a| a.matches(element))
            && self.pseudos.iter().all(|p| p.matches(element))
    }

    fn specificity(&self) -> (u32, u32, u32) {
        let mut spec = (
            self.id.is_some() as u32,
            // An attribute selector counts alongside a class
            // (CSS Selectors 4 §17).
            (self.classes.len() + self.attrs.len()) as u32,
            self.tag.is_some() as u32,
        );
        for pseudo in &self.pseudos {
            let (id, class, tag) = pseudo.specificity();
            spec.0 += id;
            spec.1 += class;
            spec.2 += tag;
        }
        spec
    }
}

/// What joins a compound to the compound on its left, per CSS Selectors 4 §15.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Combinator {
    /// A space: the compound on the left matches any ancestor.
    Descendant,
    /// `>`: it must match the parent.
    Child,
    /// `+`: it must match the element immediately before this one.
    NextSibling,
    /// `~`: it must match any earlier sibling.
    LaterSibling,
}

/// One compound and how it is joined to the compound on its left.
#[derive(Clone, Debug)]
struct Step {
    compound: Compound,
    combinator: Combinator,
}

/// A chain, the subject last: `nav ul li` and `nav > ul li` are both three
/// steps, differing only in the first one's combinator.
#[derive(Clone, Debug)]
struct Selector {
    parts: Vec<Step>,
}

impl Selector {
    /// Match right to left against the open element stack, whose last entry is
    /// the element being matched. Two axes are walked at once: the ancestors
    /// still open above the match, and the siblings standing before it. A
    /// sibling combinator moves along the second and leaves the first alone,
    /// since siblings share every ancestor.
    fn matches(&self, stack: &[Element]) -> bool {
        let Some((subject, rest)) = self.parts.split_last() else {
            return false;
        };
        let Some((element, above)) = stack.split_last() else {
            return false;
        };
        if !subject.compound.matches(element) {
            return false;
        }
        let mut ancestors = above;
        let mut before = element.preceding();
        let mut combinator = subject.combinator;
        for step in rest.iter().rev() {
            match combinator {
                Combinator::Descendant => {
                    let Some(index) = ancestors.iter().rposition(|e| step.compound.matches(e))
                    else {
                        return false;
                    };
                    before = ancestors[index].preceding();
                    ancestors = &ancestors[..index];
                }
                Combinator::Child => {
                    let Some((parent, rest)) = ancestors.split_last() else {
                        return false;
                    };
                    if !step.compound.matches(parent) {
                        return false;
                    }
                    before = parent.preceding();
                    ancestors = rest;
                }
                Combinator::NextSibling => {
                    let Some((previous, rest)) = before.split_last() else {
                        return false;
                    };
                    if !step.compound.matches(previous) {
                        return false;
                    }
                    before = rest;
                }
                Combinator::LaterSibling => {
                    let Some(index) = before.iter().rposition(|e| step.compound.matches(e)) else {
                        return false;
                    };
                    before = &before[..index];
                }
            }
            combinator = step.combinator;
        }
        true
    }

    fn specificity(&self) -> (u32, u32, u32) {
        self.parts.iter().fold((0, 0, 0), |acc, part| {
            let s = part.compound.specificity();
            (acc.0 + s.0, acc.1 + s.1, acc.2 + s.2)
        })
    }
}

struct Rule {
    selector: Selector,
    declarations: Vec<Declaration>,
}

/// Where an element sits among its siblings, which is all the `:nth-child`
/// family asks about. Indices are 1-based and count elements only, text and
/// comments included in neither.
#[derive(Clone, Copy, Debug)]
pub struct Siblings {
    pub index: usize,
    pub count: usize,
    /// The same pair, counting only the siblings sharing this element's tag.
    pub type_index: usize,
    pub type_count: usize,
}

impl Default for Siblings {
    /// An only child, which is what the root element is.
    fn default() -> Siblings {
        Siblings {
            index: 1,
            count: 1,
            type_index: 1,
            type_count: 1,
        }
    }
}

/// An element as the cascade sees it: everything a selector can ask about.
#[derive(Clone, Debug, Default)]
pub struct Element {
    pub tag: String,
    pub id: Option<String>,
    pub classes: Vec<String>,
    /// Every attribute, names lower-cased, in document order.
    pub attrs: Vec<(String, String)>,
    pub position: Siblings,
    /// Every element sibling this one has, itself included, in document order,
    /// shared by the whole row so a wide list costs one copy rather than one
    /// per child. The entries carry an empty list of their own: a sibling
    /// combinator walks this slice and never asks a sibling for its siblings.
    pub siblings: Rc<Vec<Element>>,
}

impl Element {
    /// The siblings standing before this element, which is what `+` and `~`
    /// search. Empty for anything built without a sibling row.
    fn preceding(&self) -> &[Element] {
        self.siblings
            .get(..self.position.index.saturating_sub(1))
            .unwrap_or(&[])
    }

    /// The value of `name`, which the caller has already lower-cased.
    fn attr(&self, name: &str) -> Option<&str> {
        self.attrs
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.as_str())
    }
}

/// The window a media query is answered against.
#[derive(Clone, Copy, Debug)]
pub struct Viewport {
    pub width_px: u32,
    pub height_px: u32,
    /// The initial font size. `em` in a media query resolves against it, never
    /// against the root element's own `font-size` (CSS Media Queries 4 §1.3),
    /// which is why this is not the cascade's `root_px`.
    pub root_px: u32,
}

impl Viewport {
    pub fn new(width_px: u32, height_px: u32, root_px: u32) -> Viewport {
        Viewport {
            width_px,
            height_px,
            root_px,
        }
    }
}

impl Default for Viewport {
    fn default() -> Viewport {
        Viewport::new(760, 560, 16)
    }
}

/// Every media query list a document has been asked about, kept so a resized
/// window can find out whether any of the answers moved.
///
/// A query list is stored by its source text rather than by what it decided:
/// the same list evaluated at two viewports is the whole question, and text is
/// the only form that can be evaluated twice.
#[derive(Default, Clone)]
pub struct MediaQueries(Vec<String>);

impl MediaQueries {
    /// Note that `list` was answered, ignoring one already recorded and the
    /// empty list that every unqualified sheet carries.
    pub fn record(&mut self, list: &str) {
        let list = list.trim();
        if list.is_empty() || self.0.iter().any(|seen| seen == list) {
            return;
        }
        self.0.push(list.to_string());
    }

    /// Whether any recorded query answers differently at `other` than at `at`.
    /// False means a document built for `at` would come out identical at
    /// `other`, so nothing needs rebuilding.
    pub fn differ(&self, at: &Viewport, other: &Viewport) -> bool {
        self.0
            .iter()
            .any(|query| media_matches(query, at) != media_matches(query, other))
    }
}

/// Every rule from every stylesheet the document carries, in cascade order.
#[derive(Default)]
pub struct Stylesheet {
    rules: Vec<Rule>,
    viewport: Viewport,
    /// The `@media` preludes read while parsing, whether or not they matched.
    pub media: MediaQueries,
}

impl Stylesheet {
    /// An empty sheet whose `@media` rules are answered for `viewport`.
    pub fn new(viewport: Viewport) -> Stylesheet {
        Stylesheet {
            rules: Vec::new(),
            viewport,
            media: MediaQueries::default(),
        }
    }

    /// Add the rules in `source`, which is one `<style>` element's text.
    pub fn add(&mut self, source: &str) {
        parse_rules(source, &mut self.rules, &self.viewport, &mut self.media);
    }

    /// The style of the element on top of `stack`, cascading this sheet's
    /// matching rules over the inherited style and then the `style` attribute,
    /// and the custom properties its children inherit.
    ///
    /// Rules are applied in ascending specificity, ties going to the later
    /// rule, which is the cascade order for one origin with no `!important`.
    pub fn cascade(
        &self,
        stack: &[Element],
        inline: Option<&str>,
        parent: &Computed,
        parent_vars: &Rc<Vars>,
        root_px: u32,
    ) -> (Computed, Rc<Vars>) {
        let parent_px = parent.font_px.unwrap_or(root_px);
        // The window is the outermost containing block, so a page whose only
        // measure is a percentage still resolves it against something real.
        let basis = parent.measure.unwrap_or(self.viewport.width_px);
        let mut computed = parent.inherit();

        let mut matched: Vec<(usize, &Rule)> = self
            .rules
            .iter()
            .enumerate()
            .filter(|(_, rule)| rule.selector.matches(stack))
            .collect();
        matched.sort_by_key(|(index, rule)| (rule.selector.specificity(), *index));
        let inline = inline.map(parse_declarations).unwrap_or_default();

        let declarations = || {
            matched
                .iter()
                .flat_map(|(_, rule)| rule.declarations.iter())
                .chain(inline.iter())
        };

        let mut vars = Rc::clone(parent_vars);
        if declarations().any(Declaration::is_custom) {
            let mut scope = (**parent_vars).clone();
            for decl in declarations().filter(|d| d.is_custom()) {
                scope.0.insert(decl.name.clone(), decl.value.clone());
            }
            vars = Rc::new(scope);
        }

        for decl in declarations().filter(|d| !d.is_custom()) {
            // A `var()` naming nothing and carrying no fallback makes the
            // declaration invalid at computed-value time, which leaves the
            // inherited value standing rather than the property's initial one.
            if let Some(value) = substitute(&decl.value, &vars, 0) {
                computed.apply(&decl.name, &value, root_px, parent_px, basis);
            }
        }
        (computed, vars)
    }
}

/// Replace every `var(--name[, fallback])` in `value`, or return `None` when
/// one of them resolves to nothing.
fn substitute<'a>(value: &'a str, vars: &Vars, depth: u32) -> Option<Cow<'a, str>> {
    if !value.contains("var(") {
        return Some(Cow::Borrowed(value));
    }
    if depth >= VAR_DEPTH {
        return None;
    }
    let mut out = String::with_capacity(value.len());
    let mut rest = value;
    while let Some(start) = rest.find("var(") {
        out.push_str(&rest[..start]);
        let args_start = start + "var(".len();
        let end = close_paren(rest, args_start)?;
        let args = &rest[args_start..end];
        let (name, fallback) = match split_first_top_level(args, ',') {
            Some((name, fallback)) => (name, Some(fallback)),
            None => (args, None),
        };
        let replacement = match vars.0.get(name.trim()) {
            Some(value) => substitute(value, vars, depth + 1)?,
            None => substitute(fallback?.trim(), vars, depth + 1)?,
        };
        out.push_str(&replacement);
        rest = &rest[end + 1..];
    }
    out.push_str(rest);
    Some(Cow::Owned(out))
}

/// The byte index of the `)` closing a group that starts at `from`.
fn close_paren(text: &str, from: usize) -> Option<usize> {
    let mut depth = 0usize;
    for (i, ch) in text.char_indices().skip_while(|(i, _)| *i < from) {
        match ch {
            '(' => depth += 1,
            ')' if depth == 0 => return Some(i),
            ')' => depth -= 1,
            _ => {}
        }
    }
    None
}

/// Split at the first `sep` outside parentheses, so a `var()` fallback that is
/// itself a function survives whole.
fn split_first_top_level(text: &str, sep: char) -> Option<(&str, &str)> {
    let mut depth = 0usize;
    for (i, ch) in text.char_indices() {
        match ch {
            '(' => depth += 1,
            ')' => depth = depth.saturating_sub(1),
            _ if ch == sep && depth == 0 => return Some((&text[..i], &text[i + ch.len_utf8()..])),
            _ => {}
        }
    }
    None
}

/// Strip comments, then read rule after rule until the source runs out.
fn parse_rules(source: &str, out: &mut Vec<Rule>, viewport: &Viewport, media: &mut MediaQueries) {
    let source = strip_comments(source);
    let bytes: Vec<char> = source.chars().collect();
    let mut i = 0;
    while i < bytes.len() {
        // Whitespace between rules is skipped rather than left in the next
        // prelude, since an at-rule is recognised by starting with its `@`.
        if bytes[i].is_whitespace() {
            i += 1;
            continue;
        }
        if bytes[i] == '@' {
            let descend = match at_keyword(&bytes, i).as_str() {
                // A cascade layer's body is ordinary rules, and a modern
                // stylesheet puts nearly all of itself inside one, so skipping
                // it would drop the sheet. Layer *order* is not honoured: rules
                // keep their document order, which differs from a real cascade
                // only where two layers set the same property on the same
                // element.
                "layer" => true,
                // A media query is answered against the window the page is
                // being laid out for. One this cannot read -- an unknown
                // feature, a unit with no fixed length -- drops its body rather
                // than applying it, since a page's print or mobile rules
                // beating its desktop ones is worse than losing them.
                "media" => {
                    let prelude = at_prelude(&bytes, i);
                    media.record(&prelude);
                    media_matches(&prelude, viewport)
                }
                // `@supports` and the rest are skipped whole, bodies included.
                _ => false,
            };
            if descend && let Some(body) = at_rule_body(&bytes, i) {
                parse_rules(&body, out, viewport, media);
            }
            i = skip_at_rule(&bytes, i);
            continue;
        }
        let Some(open) = (i..bytes.len()).find(|&j| bytes[j] == '{') else {
            break;
        };
        let close = match_brace(&bytes, open);
        let prelude: String = bytes[i..open].iter().collect();
        let body: String = bytes[open + 1..close.min(bytes.len())].iter().collect();
        let declarations = parse_declarations(&body);
        if !declarations.is_empty() {
            for selector in parse_selectors(&prelude) {
                out.push(Rule {
                    selector,
                    declarations: declarations.clone(),
                });
            }
        }
        i = close + 1;
    }
}

/// The at-rule's name, lowercased: the word after the `@` at `start`.
fn at_keyword(chars: &[char], start: usize) -> String {
    chars[start + 1..]
        .iter()
        .take_while(|c| c.is_ascii_alphabetic())
        .flat_map(|c| c.to_lowercase())
        .collect()
}

/// The at-rule's prelude: everything between its keyword and its block, which
/// for `@media` is the query list.
fn at_prelude(chars: &[char], start: usize) -> String {
    let after_keyword = start + 1 + at_keyword(chars, start).chars().count();
    chars[after_keyword..]
        .iter()
        .take_while(|c| !matches!(c, '{' | ';'))
        .collect()
}

/// The text between the braces of the at-rule at `start`, or `None` when it is
/// a statement rather than a block -- `@layer base, utils;` names layers and
/// carries no rules.
fn at_rule_body(chars: &[char], start: usize) -> Option<String> {
    let open = (start..chars.len()).find(|&i| matches!(chars[i], '{' | ';'))?;
    if chars[open] == ';' {
        return None;
    }
    let close = match_brace(chars, open);
    Some(chars[open + 1..close.min(chars.len())].iter().collect())
}

/// The index just past an at-rule: past its block, or past its `;`.
fn skip_at_rule(chars: &[char], start: usize) -> usize {
    for (i, ch) in chars.iter().enumerate().skip(start) {
        match ch {
            '{' => return match_brace(chars, i) + 1,
            ';' => return i + 1,
            _ => {}
        }
    }
    chars.len()
}

/// The index of the `}` closing the `{` at `open`, or the end of the input.
fn match_brace(chars: &[char], open: usize) -> usize {
    let mut depth = 0usize;
    for (i, ch) in chars.iter().enumerate().skip(open) {
        match ch {
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    return i;
                }
            }
            _ => {}
        }
    }
    chars.len()
}

fn strip_comments(source: &str) -> String {
    let mut out = String::with_capacity(source.len());
    let mut rest = source;
    while let Some(start) = rest.find("/*") {
        out.push_str(&rest[..start]);
        rest = match rest[start + 2..].find("*/") {
            Some(end) => &rest[start + 2 + end + 2..],
            None => "",
        };
    }
    out.push_str(rest);
    out
}

/// Split a comma-separated selector list, with `(...)`, `[...]` and quoted
/// strings opaque, so a comma inside an attribute value or a `:not()` argument
/// does not end a selector.
fn split_selector_list(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut current = String::new();
    let mut depth = 0usize;
    let mut quote: Option<char> = None;
    for ch in text.chars() {
        if let Some(q) = quote {
            if ch == q {
                quote = None;
            }
            current.push(ch);
            continue;
        }
        match ch {
            '"' | '\'' => quote = Some(ch),
            '(' | '[' => depth += 1,
            ')' | ']' => depth = depth.saturating_sub(1),
            ',' if depth == 0 => {
                out.push(std::mem::take(&mut current));
                continue;
            }
            _ => {}
        }
        current.push(ch);
    }
    out.push(current);
    out
}

/// A comma-separated selector list, dropping any selector using syntax this
/// does not implement rather than matching it too loosely.
fn parse_selectors(prelude: &str) -> Vec<Selector> {
    split_selector_list(prelude)
        .iter()
        .filter_map(|text| parse_selector(text.trim()))
        .collect()
}

fn parse_selector(text: &str) -> Option<Selector> {
    if text.is_empty() {
        return None;
    }
    // `:root` is the one pseudo-class worth having: in an HTML document it is
    // the `html` element and nothing else, and it is where a stylesheet
    // declares the custom properties the rest of it reads.
    let text = text.replace(":root", "html");
    let mut parts: Vec<Step> = Vec::new();
    let mut pending: Option<Combinator> = None;
    for word in tokenize_selector(&text)? {
        let combinator = match word.as_str() {
            ">" => Some(Combinator::Child),
            "+" => Some(Combinator::NextSibling),
            "~" => Some(Combinator::LaterSibling),
            _ => None,
        };
        if let Some(combinator) = combinator {
            // A chain cannot start with a combinator, and two in a row is not
            // a selector either.
            if parts.is_empty() || pending.is_some() {
                return None;
            }
            pending = Some(combinator);
            continue;
        }
        parts.push(Step {
            compound: parse_compound(&word, 0)?,
            combinator: pending.take().unwrap_or(Combinator::Descendant),
        });
    }
    (!parts.is_empty() && pending.is_none()).then_some(Selector { parts })
}

/// Split a selector into compounds and combinator tokens, with `[...]` and
/// `(...)` opaque so a quoted attribute value may hold a space, a `>` or
/// anything else, and `:nth-child(2n + 1)` stays one compound. `None` for a
/// bracket, paren or quote left open, since a selector read past its end would
/// apply somewhere the page never asked for.
fn tokenize_selector(text: &str) -> Option<Vec<String>> {
    let mut tokens: Vec<String> = Vec::new();
    let mut current = String::new();
    let mut depth = 0usize;
    let mut quote: Option<char> = None;
    for ch in text.chars() {
        if let Some(q) = quote {
            if ch == q {
                quote = None;
            }
            current.push(ch);
            continue;
        }
        match ch {
            '"' | '\'' if depth > 0 => {
                quote = Some(ch);
                current.push(ch);
            }
            '[' | '(' => {
                depth += 1;
                current.push(ch);
            }
            ']' | ')' => {
                depth = depth.checked_sub(1)?;
                current.push(ch);
            }
            _ if depth > 0 => current.push(ch),
            '>' | '+' | '~' => {
                if !current.is_empty() {
                    tokens.push(std::mem::take(&mut current));
                }
                tokens.push(ch.to_string());
            }
            _ if ch.is_whitespace() => {
                if !current.is_empty() {
                    tokens.push(std::mem::take(&mut current));
                }
            }
            _ => current.push(ch),
        }
    }
    if depth != 0 || quote.is_some() {
        return None;
    }
    if !current.is_empty() {
        tokens.push(current);
    }
    (!tokens.is_empty()).then_some(tokens)
}

/// One compound selector. `depth` counts the enclosing logical pseudo-classes,
/// since `:not()` takes selectors that may themselves hold one; a page nesting
/// them beyond a handful is not describing anything, and reading it would
/// recurse as deep as the source is long.
fn parse_compound(word: &str, depth: u32) -> Option<Compound> {
    if depth > MAX_SELECTOR_NESTING {
        return None;
    }
    let mut compound = Compound::default();
    let mut current = String::new();
    let mut kind = b' ';
    // `*` matches every element, so it constrains nothing: an empty compound
    // already says that, and `*.card` is exactly `.card`.
    let mut universal = false;
    let mut chars = word.chars().peekable();
    while let Some(ch) = chars.next() {
        match ch {
            '*' => universal = true,
            '#' | '.' => {
                push_part(kind, &mut current, &mut compound);
                kind = ch as u8;
            }
            '[' => {
                push_part(kind, &mut current, &mut compound);
                kind = b' ';
                let mut body = String::new();
                let mut quote: Option<char> = None;
                let mut closed = false;
                for ch in chars.by_ref() {
                    match quote {
                        Some(q) => {
                            if ch == q {
                                quote = None;
                            }
                            body.push(ch);
                        }
                        None if ch == '"' || ch == '\'' => {
                            quote = Some(ch);
                            body.push(ch);
                        }
                        None if ch == ']' => {
                            closed = true;
                            break;
                        }
                        None => body.push(ch),
                    }
                }
                if !closed {
                    return None;
                }
                compound.attrs.push(parse_attr(&body)?);
            }
            ':' => {
                push_part(kind, &mut current, &mut compound);
                kind = b' ';
                // A `::` selects a pseudo-element, which is not a test on this
                // element at all.
                if chars.peek() == Some(&':') {
                    return None;
                }
                let mut name = String::new();
                while let Some(&ch) = chars.peek() {
                    // Anything that opens another part of the compound ends
                    // the name and belongs to the outer loop.
                    if matches!(ch, '.' | '#' | '[' | ':' | '(') {
                        break;
                    }
                    name.push(ch);
                    chars.next();
                }
                let mut arg: Option<String> = None;
                if chars.peek() == Some(&'(') {
                    chars.next();
                    let mut body = String::new();
                    let mut nesting = 1usize;
                    let mut quote: Option<char> = None;
                    let mut closed = false;
                    for ch in chars.by_ref() {
                        match quote {
                            Some(q) => {
                                if ch == q {
                                    quote = None;
                                }
                            }
                            None => match ch {
                                '"' | '\'' => quote = Some(ch),
                                '(' => nesting += 1,
                                ')' => {
                                    nesting -= 1;
                                    if nesting == 0 {
                                        closed = true;
                                        break;
                                    }
                                }
                                _ => {}
                            },
                        }
                        body.push(ch);
                    }
                    if !closed {
                        return None;
                    }
                    arg = Some(body);
                }
                compound
                    .pseudos
                    .push(parse_pseudo(&name, arg.as_deref(), depth)?);
            }
            // A combinator is the tokenizer's business and never reaches a
            // compound; anything here is a stray, an unopened functional
            // notation, or a combinator inside a logical pseudo-class, whose
            // arguments are compounds. A compound matched without it would
            // apply far too widely.
            '+' | '~' | '>' | '(' | ')' | ']' => return None,
            _ if ch.is_whitespace() => return None,
            _ => current.push(ch),
        }
    }
    push_part(kind, &mut current, &mut compound);
    if !universal
        && compound.tag.is_none()
        && compound.id.is_none()
        && compound.classes.is_empty()
        && compound.attrs.is_empty()
        && compound.pseudos.is_empty()
    {
        return None;
    }
    Some(compound)
}

/// How deep `:not(:is(...))` may nest before a selector is refused.
const MAX_SELECTOR_NESTING: u32 = 4;

/// One `:name` or `:name(...)`, per CSS Selectors 4 §4 and §9. `None` for
/// anything not implemented, which drops the whole selector.
fn parse_pseudo(name: &str, arg: Option<&str>, depth: u32) -> Option<Pseudo> {
    let name = name.to_lowercase();
    let nth = |a: i32, b: i32, from_end: bool, of_type: bool| {
        Some(Pseudo::Nth {
            a,
            b,
            from_end,
            of_type,
        })
    };
    let logical = |arg: &str, negate: bool, weightless: bool| {
        let mut any = Vec::new();
        for text in split_selector_list(arg) {
            any.push(parse_compound(text.trim(), depth + 1)?);
        }
        Some(Pseudo::Logical {
            any,
            negate,
            weightless,
        })
    };
    match (name.as_str(), arg) {
        ("first-child", None) => nth(0, 1, false, false),
        ("last-child", None) => nth(0, 1, true, false),
        ("first-of-type", None) => nth(0, 1, false, true),
        ("last-of-type", None) => nth(0, 1, true, true),
        ("only-child", None) => Some(Pseudo::Only { of_type: false }),
        ("only-of-type", None) => Some(Pseudo::Only { of_type: true }),
        ("nth-child", Some(arg)) => parse_nth(arg).and_then(|(a, b)| nth(a, b, false, false)),
        ("nth-last-child", Some(arg)) => parse_nth(arg).and_then(|(a, b)| nth(a, b, true, false)),
        ("nth-of-type", Some(arg)) => parse_nth(arg).and_then(|(a, b)| nth(a, b, false, true)),
        ("nth-last-of-type", Some(arg)) => parse_nth(arg).and_then(|(a, b)| nth(a, b, true, true)),
        ("not", Some(arg)) => logical(arg, true, false),
        ("is", Some(arg)) => logical(arg, false, false),
        ("where", Some(arg)) => logical(arg, false, true),
        _ => None,
    }
}

/// The `An+B` microsyntax of CSS Syntax 3 §5.4: `odd`, `even`, a bare integer,
/// or a coefficient on `n` with an optional offset, whitespace anywhere.
fn parse_nth(arg: &str) -> Option<(i32, i32)> {
    let text: String = arg
        .chars()
        .filter(|c| !c.is_whitespace())
        .collect::<String>()
        .to_lowercase();
    match text.as_str() {
        "odd" => return Some((2, 1)),
        "even" => return Some((2, 0)),
        "" => return None,
        _ => {}
    }
    let Some((coefficient, offset)) = text.split_once('n') else {
        return Some((0, text.parse().ok()?));
    };
    let a = match coefficient {
        "" | "+" => 1,
        "-" => -1,
        _ => coefficient.parse().ok()?,
    };
    let b = if offset.is_empty() {
        0
    } else {
        // A bare `2n5` is not the syntax: the offset carries its own sign.
        if !offset.starts_with(['+', '-']) {
            return None;
        }
        offset.parse().ok()?
    };
    Some((a, b))
}

/// The inside of one `[...]`, per CSS Selectors 4 §6: a name, optionally an
/// operator and a value, optionally the `i` (or `s`) case-sensitivity flag.
fn parse_attr(body: &str) -> Option<AttrTest> {
    let body = body.trim();
    let (name, op, rest) = match body.find('=') {
        None => (body, AttrOp::Present, ""),
        Some(eq) => {
            let (head, tail) = body.split_at(eq);
            let (name, op) = match head.chars().next_back() {
                Some('~') => (&head[..head.len() - 1], AttrOp::Word),
                Some('|') => (&head[..head.len() - 1], AttrOp::Dash),
                Some('^') => (&head[..head.len() - 1], AttrOp::Prefix),
                Some('$') => (&head[..head.len() - 1], AttrOp::Suffix),
                Some('*') => (&head[..head.len() - 1], AttrOp::Substring),
                _ => (head, AttrOp::Equals),
            };
            (name, op, &tail[1..])
        }
    };
    let name = name.trim().to_lowercase();
    if name.is_empty()
        || !name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        return None;
    }
    let mut value = rest.trim();
    let mut fold = false;
    // The flag sits outside the value, so a quoted value ending in `"` cannot
    // have eaten one.
    if !value.ends_with(['"', '\'']) {
        if let Some((head, flag)) = value.rsplit_once(char::is_whitespace)
            && matches!(flag, "i" | "I" | "s" | "S")
        {
            fold = flag.eq_ignore_ascii_case("i");
            value = head.trim_end();
        }
    }
    let mut value = value.to_string();
    if value.len() >= 2
        && let Some(quote) = value.chars().next()
        && (quote == '"' || quote == '\'')
        && value.ends_with(quote)
    {
        value = value[1..value.len() - 1].to_string();
    }
    if fold {
        value = value.to_lowercase();
    }
    Some(AttrTest {
        name,
        op,
        value,
        fold,
    })
}

/// Add the piece just read to the compound it belongs to. The tag is folded to
/// lower case because HTML tag names are matched case-insensitively; a class
/// and an id are not.
fn push_part(kind: u8, name: &mut String, compound: &mut Compound) {
    if name.is_empty() {
        return;
    }
    let name = std::mem::take(name);
    match kind {
        b'#' => compound.id = Some(name),
        b'.' => compound.classes.push(name),
        _ => compound.tag = Some(name.to_lowercase()),
    }
}

/// A declaration block's `name: value` pairs. Values keep their internal
/// spacing but are lowercased, since every value this understands is
/// case-insensitive and none of them is a string.
/// Whether a media query list applies to `viewport`.
///
/// An empty list applies: that is what a `<style>` or a `<link>` with no
/// `media` attribute means. A list is a set of alternatives, so one query that
/// cannot be read does not sink the ones beside it.
pub fn media_matches(list: &str, viewport: &Viewport) -> bool {
    list.trim().is_empty() || list.split(',').any(|query| media_query(query, viewport))
}

/// One query of a list: an optional `not`, then `and`-joined terms.
fn media_query(query: &str, viewport: &Viewport) -> bool {
    let lowered = query.trim().to_ascii_lowercase();
    let mut rest = lowered.as_str();
    let mut negated = false;
    if let Some(tail) = rest.strip_prefix("not ") {
        negated = true;
        rest = tail;
    } else if let Some(tail) = rest.strip_prefix("only ") {
        // `only` exists to hide a sheet from browsers written before media
        // queries were; to one that has them it says nothing.
        rest = tail;
    }
    match media_terms(rest.trim(), viewport) {
        Some(matched) => matched != negated,
        // Syntax this cannot read never applies, negated or not. `not` flipping
        // an unparsed query into a match is how a sheet meant for print ends up
        // on the screen.
        None => false,
    }
}

/// Every `and`-joined term of one query, or `None` when the syntax is not read.
fn media_terms(query: &str, viewport: &Viewport) -> Option<bool> {
    let mut matched = true;
    for term in split_and(query) {
        matched &= media_term(term.trim(), viewport)?;
    }
    Some(matched)
}

/// Split a query on the `and` combinators outside its parentheses.
fn split_and(query: &str) -> Vec<&str> {
    const AND: &str = " and ";
    let (mut parts, mut depth, mut start, mut skip_to) = (Vec::new(), 0usize, 0usize, 0usize);
    for (i, ch) in query.char_indices() {
        if i < skip_to {
            continue;
        }
        match ch {
            '(' => depth += 1,
            ')' => depth = depth.saturating_sub(1),
            _ => {}
        }
        if depth == 0 && query[i..].starts_with(AND) {
            parts.push(&query[start..i]);
            skip_to = i + AND.len();
            start = skip_to;
        }
    }
    parts.push(&query[start..]);
    parts
}

/// One term: a parenthesised feature test, or a media type.
fn media_term(term: &str, viewport: &Viewport) -> Option<bool> {
    if let Some(inner) = term.strip_prefix('(').and_then(|t| t.strip_suffix(')')) {
        return media_feature(inner.trim(), viewport);
    }
    match term {
        // A window is a screen, and `all` is every medium there is.
        "screen" | "all" => Some(true),
        // Any other media type -- `print`, `speech` -- is one this is not, and
        // an unknown type is simply a type this is not either.
        _ if !term.is_empty() && term.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') => {
            Some(false)
        }
        _ => None,
    }
}

/// A feature test, in either the `min-width: 40rem` form or the range form.
fn media_feature(feature: &str, viewport: &Viewport) -> Option<bool> {
    if let Some((operands, ops)) = range_parts(feature) {
        // `20em <= width <= 40em` is two comparisons sharing their middle
        // operand, and reads left to right like the arithmetic it looks like.
        let mut matched = true;
        for (index, op) in ops.iter().enumerate() {
            matched &= compare(operands[index], *op, operands[index + 1], viewport)?;
        }
        return Some(matched);
    }
    let Some((name, value)) = feature.split_once(':') else {
        // The boolean form, `(color)`, asks whether a feature exists and is
        // non-zero. None of the ones answered here can be written that way.
        return Some(false);
    };
    let (name, value) = (name.trim(), value.trim());
    let (axis, op) = match name {
        "min-width" => (Axis::Width, Cmp::Ge),
        "max-width" => (Axis::Width, Cmp::Le),
        "width" => (Axis::Width, Cmp::Eq),
        "min-height" => (Axis::Height, Cmp::Ge),
        "max-height" => (Axis::Height, Cmp::Le),
        "height" => (Axis::Height, Cmp::Eq),
        "orientation" => {
            return Some(match value {
                "landscape" => viewport.width_px >= viewport.height_px,
                "portrait" => viewport.width_px < viewport.height_px,
                _ => false,
            });
        }
        // A feature this cannot answer -- `prefers-color-scheme`, `hover`,
        // `resolution` -- never matches (CSS Media Queries 4 §3).
        _ => return Some(false),
    };
    Some(op.holds(axis.of(viewport), media_length(value, viewport)?))
}

/// One comparison of the range form, with the feature named on either side.
fn compare(lhs: &str, op: Cmp, rhs: &str, viewport: &Viewport) -> Option<bool> {
    if let Some(axis) = Axis::named(lhs) {
        return Some(op.holds(axis.of(viewport), media_length(rhs, viewport)?));
    }
    if let Some(axis) = Axis::named(rhs) {
        // `40rem <= width` is `width >= 40rem`.
        return Some(
            op.flip()
                .holds(axis.of(viewport), media_length(lhs, viewport)?),
        );
    }
    Some(false)
}

/// Split the range form into its operands and the comparators between them.
fn range_parts(feature: &str) -> Option<(Vec<&str>, Vec<Cmp>)> {
    let (mut operands, mut ops, mut start, mut i) = (Vec::new(), Vec::new(), 0usize, 0usize);
    let bytes = feature.as_bytes();
    while i < bytes.len() {
        let (op, len) = match (bytes[i], bytes.get(i + 1)) {
            (b'>', Some(b'=')) => (Cmp::Ge, 2),
            (b'<', Some(b'=')) => (Cmp::Le, 2),
            (b'>', _) => (Cmp::Gt, 1),
            (b'<', _) => (Cmp::Lt, 1),
            (b'=', _) => (Cmp::Eq, 1),
            _ => {
                i += 1;
                continue;
            }
        };
        operands.push(feature[start..i].trim());
        ops.push(op);
        i += len;
        start = i;
    }
    if ops.is_empty() {
        return None;
    }
    operands.push(feature[start..].trim());
    Some((operands, ops))
}

/// A media query's length. `em` resolves against the initial font size, and a
/// unit whose length depends on the viewport is refused rather than resolved
/// against the very thing being asked about.
fn media_length(value: &str, viewport: &Viewport) -> Option<u32> {
    parse_length(value, viewport.root_px, viewport.root_px)
}

#[derive(Clone, Copy)]
enum Axis {
    Width,
    Height,
}

impl Axis {
    fn named(name: &str) -> Option<Axis> {
        match name {
            "width" => Some(Axis::Width),
            "height" => Some(Axis::Height),
            _ => None,
        }
    }

    fn of(self, viewport: &Viewport) -> u32 {
        match self {
            Axis::Width => viewport.width_px,
            Axis::Height => viewport.height_px,
        }
    }
}

#[derive(Clone, Copy)]
enum Cmp {
    Lt,
    Le,
    Eq,
    Ge,
    Gt,
}

impl Cmp {
    fn holds(self, left: u32, right: u32) -> bool {
        match self {
            Cmp::Lt => left < right,
            Cmp::Le => left <= right,
            Cmp::Eq => left == right,
            Cmp::Ge => left >= right,
            Cmp::Gt => left > right,
        }
    }

    /// The comparator that says the same thing with its operands swapped.
    fn flip(self) -> Cmp {
        match self {
            Cmp::Lt => Cmp::Gt,
            Cmp::Le => Cmp::Ge,
            Cmp::Eq => Cmp::Eq,
            Cmp::Ge => Cmp::Le,
            Cmp::Gt => Cmp::Lt,
        }
    }
}

pub fn parse_declarations(body: &str) -> Vec<Declaration> {
    let mut out = Vec::new();
    for piece in split_top_level(body, ';') {
        let Some((name, value)) = piece.split_once(':') else {
            continue;
        };
        let name = name.trim().to_lowercase();
        let value = value.trim().to_lowercase();
        if name.is_empty() || value.is_empty() {
            continue;
        }
        out.push(Declaration { name, value });
    }
    out
}

/// Split on `sep`, ignoring separators inside parentheses so a `rgb(1, 2, 3)`
/// survives.
fn split_top_level(text: &str, sep: char) -> Vec<String> {
    let mut out = Vec::new();
    let mut depth = 0usize;
    let mut current = String::new();
    for ch in text.chars() {
        match ch {
            '(' => depth += 1,
            ')' => depth = depth.saturating_sub(1),
            _ => {}
        }
        if ch == sep && depth == 0 {
            out.push(std::mem::take(&mut current));
        } else {
            current.push(ch);
        }
    }
    out.push(current);
    out
}

fn parse_weight(value: &str) -> Option<bool> {
    match value {
        "bold" | "bolder" => Some(true),
        "normal" | "lighter" => Some(false),
        _ => match value.parse::<u32>() {
            Ok(n) => Some(n >= 600),
            Err(_) => None,
        },
    }
}

/// `font-size`, including the absolute keywords, which are the CSS scale
/// against the document's own body size rather than a fixed pixel table.
fn parse_font_size(value: &str, root_px: u32, parent_px: u32) -> Option<u32> {
    let scaled = |num: u32, den: u32| Some((root_px * num / den).max(1));
    match value {
        "xx-small" => scaled(3, 5),
        "x-small" => scaled(3, 4),
        "small" => scaled(8, 9),
        "medium" => scaled(1, 1),
        "large" => scaled(6, 5),
        "x-large" => scaled(3, 2),
        "xx-large" => scaled(2, 1),
        "smaller" => Some((parent_px * 5 / 6).max(1)),
        "larger" => Some((parent_px * 6 / 5).max(1)),
        _ => parse_length(value, root_px, parent_px),
    }
}

/// `normal`, a unitless number, or a length. The number is the common case on
/// a real page and the only one that survives a font-size change downtree, so
/// it is kept unresolved.
fn parse_line_height(value: &str, root_px: u32, em_px: u32) -> Option<LineHeight> {
    let value = value.trim();
    if value.eq_ignore_ascii_case("normal") {
        return Some(LineHeight::Normal);
    }
    if let Ok(factor) = value.parse::<f32>() {
        return (factor > 0.0).then_some(LineHeight::Scale(factor));
    }
    parse_length(value, root_px, em_px).map(LineHeight::Px)
}

/// A length in pixels. Relative units resolve against `em_px`; anything with a
/// unit that depends on a viewport or a container is refused, since guessing
/// one produces a size the document never asked for.
fn is_auto(value: &str) -> bool {
    value.trim().eq_ignore_ascii_case("auto")
}

/// A `width`, whose percentage is of the containing block rather than of the
/// font size a `parse_length` percentage resolves against.
fn parse_measure(value: &str, root_px: u32, em_px: u32, basis: u32) -> Option<u32> {
    let value = value.trim();
    if let Some(number) = value.strip_suffix('%') {
        let percent: f32 = number.trim().parse().ok()?;
        return Some((basis as f32 * percent.max(0.0) / 100.0).round() as u32);
    }
    parse_length(value, root_px, em_px)
}

/// A length that keeps its sign, which only the spacing properties want: a
/// negative margin or padding has nowhere to go in a flat block list, so
/// [`parse_length`] floors those at zero instead.
fn parse_signed_length(value: &str, root_px: u32, em_px: u32) -> Option<i32> {
    let value = value.trim();
    if value == "0" || value == "auto" || value == "inherit" {
        return (value == "0").then_some(0);
    }
    let split = value.find(|c: char| !c.is_ascii_digit() && c != '.' && c != '-' && c != '+')?;
    let (number, unit) = value.split_at(split);
    let number: f32 = number.parse().ok()?;
    let px = match unit {
        "px" => number,
        "pt" => number * 4.0 / 3.0,
        "em" => number * em_px as f32,
        "rem" => number * root_px as f32,
        "%" => number * em_px as f32 / 100.0,
        _ => return None,
    };
    Some(px.round() as i32)
}

/// `grid-template-columns`, as the track list the box engine takes.
///
/// `repeat(N, <track>)` is expanded here rather than carried, since the engine
/// wants the tracks themselves. An unreadable track makes the whole
/// declaration invalid, the way a bad component value does in CSS.
fn parse_tracks(value: &str, root_px: u32, em_px: u32) -> Option<Tracks> {
    let mut out = Vec::new();
    let mut rest = value.trim();
    while !rest.is_empty() {
        if let Some(open) = rest.strip_prefix("repeat(") {
            let close = open.find(')')?;
            let (args, tail) = open.split_at(close);
            let (count, track) = args.split_once(',')?;
            let count: usize = count.trim().parse().ok()?;
            if count > Tracks::MAX {
                return None;
            }
            let track = parse_track(track.trim(), root_px, em_px)?;
            out.extend(core::iter::repeat_n(track, count));
            rest = tail[1..].trim_start();
            continue;
        }
        let end = rest.find(char::is_whitespace).unwrap_or(rest.len());
        let (word, tail) = rest.split_at(end);
        out.push(parse_track(word, root_px, em_px)?);
        rest = tail.trim_start();
    }
    Tracks::from_slice(&out)
}

/// One track of a template: a fraction, a length, or `auto`.
fn parse_track(word: &str, root_px: u32, em_px: u32) -> Option<Track> {
    if let Some(fr) = word.strip_suffix("fr") {
        let fr: f32 = fr.trim().parse().ok()?;
        return (fr.is_finite() && fr >= 0.0).then_some(Track::Fr(fr));
    }
    if word == "auto" || word == "min-content" || word == "max-content" {
        return Some(Track::Auto);
    }
    parse_length(word, root_px, em_px).map(Track::Px)
}

fn parse_length(value: &str, root_px: u32, em_px: u32) -> Option<u32> {
    Some(parse_signed_length(value, root_px, em_px)?.max(0) as u32)
}

/// `letter-spacing`/`word-spacing`: a signed length, or `normal`, which is the
/// face's own advances and so no adjustment at all.
fn parse_spacing(value: &str, root_px: u32, em_px: u32) -> Option<i32> {
    if value.trim().eq_ignore_ascii_case("normal") {
        return Some(0);
    }
    parse_signed_length(value, root_px, em_px)
}

/// `#rgb`, `#rrggbb`, `rgb()`/`rgba()` and the handful of named colours a
/// hand-written page actually uses. Everything else -- `hsl()`, `oklch()`, a
/// colour named outside this list -- is refused so the inherited colour stands.
/// A `var()` never reaches here: it is substituted before the declaration is
/// applied.
fn parse_color(value: &str) -> Option<u32> {
    let value = value.trim();
    if let Some(hex) = value.strip_prefix('#') {
        let (r, g, b) = match hex.len() {
            3 | 4 => {
                let d = |i: usize| u8::from_str_radix(&hex[i..i + 1], 16).ok();
                let (r, g, b) = (d(0)?, d(1)?, d(2)?);
                (r * 17, g * 17, b * 17)
            }
            6 | 8 => {
                let d = |i: usize| u8::from_str_radix(&hex[i..i + 2], 16).ok();
                (d(0)?, d(2)?, d(4)?)
            }
            _ => return None,
        };
        return Some(rgb(r, g, b));
    }
    if let Some(args) = value
        .strip_prefix("rgb(")
        .or_else(|| value.strip_prefix("rgba("))
        .and_then(|rest| rest.strip_suffix(')'))
    {
        let parts: Vec<u8> = args
            .split([',', ' ', '/'])
            .filter(|p| !p.trim().is_empty())
            .take(3)
            .filter_map(|p| p.trim().parse::<f32>().ok())
            .map(|n| n.clamp(0.0, 255.0) as u8)
            .collect();
        if parts.len() == 3 {
            return Some(rgb(parts[0], parts[1], parts[2]));
        }
        return None;
    }
    let named: (u8, u8, u8) = match value {
        "black" => (0, 0, 0),
        "white" => (255, 255, 255),
        "red" => (255, 0, 0),
        "green" => (0, 128, 0),
        "lime" => (0, 255, 0),
        "blue" => (0, 0, 255),
        "navy" => (0, 0, 128),
        "yellow" => (255, 255, 0),
        "orange" => (255, 165, 0),
        "purple" => (128, 0, 128),
        "teal" => (0, 128, 128),
        "olive" => (128, 128, 0),
        "maroon" => (128, 0, 0),
        "silver" => (192, 192, 192),
        "gray" | "grey" => (128, 128, 128),
        _ => return None,
    };
    Some(rgb(named.0, named.1, named.2))
}

const fn rgb(r: u8, g: u8, b: u8) -> u32 {
    0xFF00_0000 | ((r as u32) << 16) | ((g as u32) << 8) | b as u32
}

#[cfg(test)]
mod tests {
    use super::*;

    fn element(tag: &str, classes: &[&str]) -> Element {
        Element {
            tag: tag.to_string(),
            classes: classes.iter().map(|c| c.to_string()).collect(),
            ..Element::default()
        }
    }

    fn element_with(tag: &str, attrs: &[(&str, &str)]) -> Element {
        Element {
            tag: tag.to_string(),
            attrs: attrs
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
            ..Element::default()
        }
    }

    fn sheet(source: &str) -> Stylesheet {
        sheet_in(source, Viewport::default())
    }

    fn sheet_in(source: &str, viewport: Viewport) -> Stylesheet {
        let mut sheet = Stylesheet::new(viewport);
        sheet.add(source);
        sheet
    }

    /// The computed style alone, for the cases that do not care what the
    /// element left in scope for its children.
    fn cascade(sheet: &Stylesheet, stack: &[Element], inline: Option<&str>) -> Computed {
        sheet
            .cascade(stack, inline, &Computed::default(), &Vars::root(), 14)
            .0
    }

    #[test]
    fn specificity_orders_the_cascade() {
        let sheet = sheet("p { color: red } p.lead { color: blue } .lead { color: green }");
        let stack = vec![element("p", &["lead"])];
        let computed = cascade(&sheet, &stack, None);
        assert_eq!(computed.color, Some(rgb(0, 0, 255)));
    }

    #[test]
    fn display_names_the_box_the_element_opens() {
        let sheet = sheet(
            "span { display: BLOCK } li { display: inline } em { display: list-item } \
             i { display: inline flow-root } b { display: contents } u { display: marquee }",
        );
        let of = |tag: &str| cascade(&sheet, &[element(tag, &[])], None);
        assert_eq!(of("span").display, Some(Display::Block));
        assert_eq!(of("li").display, Some(Display::Inline));
        assert_eq!(of("em").display, Some(Display::ListItem));
        assert_eq!(of("i").display, Some(Display::Inline));
        assert_eq!(of("b").display, Some(Display::Inline));
        // An unknown keyword leaves the declaration invalid, so the element
        // keeps the box it would have opened on its own.
        assert_eq!(of("u").display, None);
        assert!(!of("span").hidden);
    }

    #[test]
    fn display_none_hides_and_names_no_box() {
        let sheet = sheet("p { display: block } p { display: none }");
        let computed = cascade(&sheet, &[element("p", &[])], None);
        assert!(computed.hidden);
        assert_eq!(computed.display, None);
    }

    #[test]
    fn visibility_hides_without_dropping_the_box() {
        let computed = cascade(
            &sheet("p { visibility: hidden }"),
            &[element("p", &[])],
            None,
        );
        assert!(computed.invisible);
        assert!(!computed.hidden);
        assert!(
            cascade(
                &sheet("p { visibility: collapse }"),
                &[element("p", &[])],
                None
            )
            .invisible
        );
    }

    #[test]
    fn visibility_inherits_and_a_child_can_come_back() {
        let sheet = sheet("div { visibility: hidden } span { visibility: visible }");
        let parent = sheet
            .cascade(
                &[element("div", &[])],
                None,
                &Computed::default(),
                &Vars::root(),
                14,
            )
            .0;
        assert!(parent.invisible);
        let hidden_child = sheet
            .cascade(
                &[element("div", &[]), element("em", &[])],
                None,
                &parent,
                &Vars::root(),
                14,
            )
            .0;
        assert!(hidden_child.invisible);
        let visible_child = sheet
            .cascade(
                &[element("div", &[]), element("span", &[])],
                None,
                &parent,
                &Vars::root(),
                14,
            )
            .0;
        assert!(!visible_child.invisible);
    }

    #[test]
    fn display_does_not_inherit() {
        let sheet = sheet("div { display: block }");
        let parent = sheet
            .cascade(
                &[element("div", &[])],
                None,
                &Computed::default(),
                &Vars::root(),
                14,
            )
            .0;
        let child = sheet
            .cascade(
                &[element("div", &[]), element("span", &[])],
                None,
                &parent,
                &Vars::root(),
                14,
            )
            .0;
        assert_eq!(parent.display, Some(Display::Block));
        assert_eq!(child.display, None);
    }

    #[test]
    fn inline_beats_every_rule() {
        let sheet = sheet("#x { color: red }");
        let stack = vec![Element {
            tag: "p".into(),
            id: Some("x".into()),
            ..Element::default()
        }];
        let computed = cascade(&sheet, &stack, Some("color: #00ff00"));
        assert_eq!(computed.color, Some(rgb(0, 255, 0)));
    }

    #[test]
    fn descendant_selector_needs_the_ancestor() {
        let sheet = sheet("nav a { display: none }");
        let inside = vec![element("nav", &[]), element("ul", &[]), element("a", &[])];
        let outside = vec![element("p", &[]), element("a", &[])];
        assert!(cascade(&sheet, &inside, None).hidden);
        assert!(!cascade(&sheet, &outside, None).hidden);
    }

    /// A viewport wide enough for `min-width: 50em`, and one that is not.
    fn wide() -> Viewport {
        Viewport::new(1000, 700, 16)
    }

    fn narrow() -> Viewport {
        Viewport::new(600, 700, 16)
    }

    #[test]
    fn a_media_query_applies_only_at_its_width() {
        let source = "p { color: red } @media (min-width: 50em) { p { color: blue } }";
        let stack = vec![element("p", &[])];
        let at_wide = cascade(&sheet_in(source, wide()), &stack, None);
        let at_narrow = cascade(&sheet_in(source, narrow()), &stack, None);
        assert_eq!(at_wide.color, Some(rgb(0, 0, 255)));
        assert_eq!(at_narrow.color, Some(rgb(255, 0, 0)));
    }

    #[test]
    fn a_sheet_records_the_queries_it_answered() {
        let sheet = sheet_in(
            "@media (min-width: 50em) { p { color: blue } } @media print { p { color: red } }",
            wide(),
        );
        // Both are recorded, the one that did not match included, since it is
        // the query that would start matching at another size.
        assert!(sheet.media.differ(&wide(), &narrow()));
        assert!(!sheet.media.differ(&wide(), &Viewport::new(1200, 700, 16)));
        // `print` matches at neither, so it is not what made those differ.
        assert!(
            !sheet_in("@media print { p { color: red } }", wide())
                .media
                .differ(&wide(), &narrow())
        );
    }

    #[test]
    fn a_sheet_with_no_query_never_asks_to_be_rebuilt() {
        let sheet = sheet_in("p { color: red } @layer base { p { color: blue } }", wide());
        assert!(!sheet.media.differ(&wide(), &narrow()));
    }

    #[test]
    fn media_queries_read_types_lists_and_negation() {
        let vp = wide();
        assert!(media_matches("", &vp));
        assert!(media_matches("screen", &vp));
        assert!(media_matches("only screen and (min-width: 40em)", &vp));
        assert!(media_matches("print, screen", &vp));
        assert!(media_matches("not print", &vp));
        assert!(media_matches("(orientation: landscape)", &vp));
        assert!(!media_matches("print", &vp));
        assert!(!media_matches("not screen", &vp));
        assert!(!media_matches("screen and (max-width: 40em)", &vp));
        assert!(!media_matches("(orientation: portrait)", &vp));
    }

    #[test]
    fn the_range_form_reads_either_way_round() {
        let vp = wide();
        assert!(media_matches("(width >= 50em)", &vp));
        assert!(media_matches("(50em <= width)", &vp));
        assert!(media_matches("(40em < width < 80em)", &vp));
        assert!(media_matches("(height > 40em)", &vp));
        assert!(!media_matches("(width < 50em)", &vp));
        assert!(!media_matches("(80em <= width)", &vp));
    }

    #[test]
    fn a_query_this_cannot_answer_never_matches() {
        let vp = wide();
        // An unknown feature, an unreadable unit, and a bare boolean test are
        // each false, and `not` does not turn any of them into a match.
        assert!(!media_matches("(prefers-color-scheme: dark)", &vp));
        assert!(!media_matches("(min-width: 40vw)", &vp));
        assert!(!media_matches("(color)", &vp));
        assert!(!media_matches("not (min-width: 40vw)", &vp));
        assert!(!media_matches("screen and (hover: hover)", &vp));
    }

    #[test]
    fn relative_sizes_resolve_against_the_parent() {
        let sheet = sheet("p { font-size: 2em }");
        let parent = Computed {
            font_px: Some(20),
            ..Computed::default()
        };
        let stack = vec![element("p", &[])];
        assert_eq!(
            sheet
                .cascade(&stack, None, &parent, &Vars::root(), 14)
                .0
                .font_px,
            Some(40)
        );
    }

    #[test]
    fn unsupported_selectors_do_not_match_too_widely() {
        let sheet = sheet("a:hover { display: none } p[hidden] { display: none }");
        let stack = vec![element("a", &[])];
        assert!(!cascade(&sheet, &stack, None).hidden);
    }

    #[test]
    fn a_child_combinator_wants_the_parent_and_not_an_ancestor() {
        let sheet = sheet("div > p { color: red }");
        let child = vec![element("div", &[]), element("p", &[])];
        let grandchild = vec![element("div", &[]), element("ul", &[]), element("p", &[])];
        assert_eq!(cascade(&sheet, &child, None).color, Some(rgb(255, 0, 0)));
        assert_eq!(cascade(&sheet, &grandchild, None).color, None);
    }

    #[test]
    fn a_child_combinator_needs_no_spaces_and_chains() {
        let sheet = sheet("nav>ul li { color: red }");
        let matching = vec![
            element("nav", &[]),
            element("ul", &[]),
            element("div", &[]),
            element("li", &[]),
        ];
        // `ul` is not `nav`'s child here, so the chain fails at its left end
        // even though every tag in it appears.
        let missing = vec![
            element("nav", &[]),
            element("div", &[]),
            element("ul", &[]),
            element("li", &[]),
        ];
        assert_eq!(cascade(&sheet, &matching, None).color, Some(rgb(255, 0, 0)));
        assert_eq!(cascade(&sheet, &missing, None).color, None);
    }

    #[test]
    fn the_universal_selector_matches_anything_and_adds_no_specificity() {
        let sheet = sheet("* { color: red } p { color: blue } .card * { color: lime }");
        assert_eq!(
            cascade(&sheet, &[element("div", &[])], None).color,
            Some(rgb(255, 0, 0))
        );
        // A tag beats `*` on specificity however the sheet is ordered.
        assert_eq!(
            cascade(&sheet, &[element("p", &[])], None).color,
            Some(rgb(0, 0, 255))
        );
        let inside = vec![element("div", &["card"]), element("p", &[])];
        assert_eq!(cascade(&sheet, &inside, None).color, Some(rgb(0, 255, 0)));
    }

    #[test]
    fn a_dangling_combinator_is_not_a_selector() {
        let sheet = sheet("> p { color: red } div > { color: red } p > > b { color: red }");
        let stack = vec![element("div", &[]), element("p", &[]), element("b", &[])];
        assert_eq!(cascade(&sheet, &stack, None).color, None);
    }

    /// A row of siblings, each carrying the whole row and its own place in it,
    /// which is what a sibling combinator walks. `"p.lead"` names a class.
    fn siblings(spec: &[&str]) -> Vec<Element> {
        let mut row: Vec<Element> = spec
            .iter()
            .enumerate()
            .map(|(at, item)| {
                let (tag, classes) = match item.split_once('.') {
                    Some((tag, class)) => (tag, vec![class]),
                    None => (*item, Vec::new()),
                };
                let same = |other: &&&str| other.split('.').next() == Some(tag);
                let mut element = element(tag, &classes);
                element.position = Siblings {
                    index: at + 1,
                    count: spec.len(),
                    type_index: spec[..=at].iter().filter(same).count(),
                    type_count: spec.iter().filter(same).count(),
                };
                element
            })
            .collect();
        let shared = Rc::new(row.clone());
        for element in &mut row {
            element.siblings = Rc::clone(&shared);
        }
        row
    }

    #[test]
    fn the_next_sibling_combinator_takes_only_the_element_before() {
        let sheet = sheet("h2 + p { color: red } h2 ~ p { font-weight: bold }");
        let row = siblings(&["h2", "p", "p"]);
        let at = |n: usize| vec![element("div", &[]), row[n].clone()];
        // `+` reaches the paragraph after the heading and no further; `~`
        // reaches both, and neither reaches something that is not a `p`.
        assert_eq!(cascade(&sheet, &at(1), None).color, Some(rgb(255, 0, 0)));
        assert_eq!(cascade(&sheet, &at(2), None).color, None);
        assert_eq!(cascade(&sheet, &at(1), None).bold, Some(true));
        assert_eq!(cascade(&sheet, &at(2), None).bold, Some(true));
        assert_eq!(cascade(&sheet, &at(0), None).color, None);
        assert_eq!(cascade(&sheet, &at(0), None).bold, None);
    }

    #[test]
    fn sibling_combinators_chain_and_mix_with_the_other_axes() {
        let sheet = sheet(
            "li + li + li { color: red }
             div > h2 ~ p.lead { font-weight: bold }
             h2 + div b { font-style: italic }",
        );
        let items = siblings(&["li", "li", "li"]);
        let at = |n: usize| vec![element("ul", &[]), items[n].clone()];
        assert_eq!(cascade(&sheet, &at(2), None).color, Some(rgb(255, 0, 0)));
        assert_eq!(cascade(&sheet, &at(1), None).color, None);

        // The `>` step lands on the parent and the `~` then searches the
        // parent's own row, not the subject's.
        let row = siblings(&["h2", "p.lead"]);
        let stack = vec![element("div", &[]), row[1].clone()];
        assert_eq!(cascade(&sheet, &stack, None).bold, Some(true));

        // A sibling combinator above a descendant one: `b` is inside the
        // `div`, and the `div` follows the heading.
        let above = siblings(&["h2", "div"]);
        let stack = vec![element("body", &[]), above[1].clone(), element("b", &[])];
        assert_eq!(cascade(&sheet, &stack, None).italic, Some(true));
        let alone = siblings(&["div"]);
        let stack = vec![element("body", &[]), alone[0].clone(), element("b", &[])];
        assert_eq!(cascade(&sheet, &stack, None).italic, None);
    }

    #[test]
    fn a_dangling_sibling_combinator_is_not_a_selector() {
        let sheet = sheet(
            "+ p { color: red } p + { color: red } h2 + ~ p { color: red }
             p[title~=note] + p { font-weight: bold }",
        );
        let row = siblings(&["h2", "p"]);
        let stack = vec![element("div", &[]), row[1].clone()];
        assert_eq!(cascade(&sheet, &stack, None).color, None);
        // A `~` inside an attribute test is not a combinator, so that rule
        // survives the ones around it.
        let mut before = element_with("p", &[("title", "a note")]);
        before.position = Siblings {
            index: 1,
            count: 2,
            type_index: 1,
            type_count: 2,
        };
        let mut subject = element("p", &[]);
        subject.position = Siblings {
            index: 2,
            count: 2,
            type_index: 2,
            type_count: 2,
        };
        subject.siblings = Rc::new(vec![before, subject.clone()]);
        let stack = vec![element("div", &[]), subject];
        assert_eq!(cascade(&sheet, &stack, None).bold, Some(true));
    }

    #[test]
    fn logical_pseudo_classes_negate_and_gather() {
        let sheet = sheet(
            "p:not(.lead) { color: red }
             p:is(.lead, .note) { font-weight: bold }
             p:where(.lead) { font-style: italic }
             p:not(.lead):not(.note) { text-indent: 3px }",
        );
        let plain = cascade(&sheet, &[element("p", &[])], None);
        assert_eq!(plain.color, Some(rgb(255, 0, 0)));
        assert_eq!(plain.bold, None);
        assert_eq!(plain.indent, 3);
        let lead = cascade(&sheet, &[element("p", &["lead"])], None);
        assert_eq!(lead.color, None);
        assert_eq!(lead.bold, Some(true));
        assert_eq!(lead.italic, Some(true));
        assert_eq!(lead.indent, 0);
        // Either arm of the `:is()` list answers it; neither arm of the pair of
        // `:not()`s may.
        let note = cascade(&sheet, &[element("p", &["note"])], None);
        assert_eq!(note.bold, Some(true));
        assert_eq!(note.indent, 0);
    }

    #[test]
    fn a_logical_pseudo_class_takes_its_arguments_specificity() {
        // `:not(.x)` weighs a class, so it beats the bare tag written after it;
        // `:where(.x)` weighs nothing, so the same tag beats it.
        let negated = sheet("p:not(.lead) { color: red } p { color: blue }");
        assert_eq!(
            cascade(&negated, &[element("p", &[])], None).color,
            Some(rgb(255, 0, 0))
        );
        let weightless = sheet("p:where(.lead) { color: red } p { color: blue }");
        assert_eq!(
            cascade(&weightless, &[element("p", &["lead"])], None).color,
            Some(rgb(0, 0, 255))
        );
        // A list weighs its heaviest argument, not its last one.
        let listed = sheet("p:is(#head, .lead) { color: red } p.lead { color: blue }");
        let mut head = element("p", &["lead"]);
        head.id = Some("head".to_string());
        assert_eq!(
            cascade(&listed, &[head], None).color,
            Some(rgb(255, 0, 0)),
            "the id arm weighs the list, so it outranks a rule weighing a class"
        );
    }

    #[test]
    fn a_logical_pseudo_class_this_cannot_read_drops_its_rule() {
        // A combinator inside the argument, an empty list, and a nesting depth
        // past the limit are all unread rather than approximated — matching
        // `:not()` too widely would style the whole document.
        for source in [
            "p:not(.a .b) { color: red }",
            "p:not(.a > .b) { color: red }",
            "p:not() { color: red }",
            "p:not(.a, ) { color: red }",
            "p:not(:is(:not(:is(:not(.a))))) { color: red }",
        ] {
            assert_eq!(
                cascade(&sheet(source), &[element("p", &[])], None).color,
                None,
                "{source} should be dropped"
            );
        }
        // Nesting up to the limit still reads, double negation and all.
        let nested = sheet("p:not(:is(:not(.a))) { color: red }");
        assert_eq!(
            cascade(&nested, &[element("p", &["a"])], None).color,
            Some(rgb(255, 0, 0))
        );
        assert_eq!(cascade(&nested, &[element("p", &[])], None).color, None);
    }

    #[test]
    fn a_selector_list_splits_outside_brackets_and_parentheses() {
        // The comma in the attribute value and the one in the `:is()` list are
        // both inside the selector they belong to.
        let sheet = sheet("p[title=\"a,b\"], p:is(.x, .y) { color: red }");
        assert_eq!(
            cascade(&sheet, &[element_with("p", &[("title", "a,b")])], None).color,
            Some(rgb(255, 0, 0))
        );
        assert_eq!(
            cascade(&sheet, &[element("p", &["y"])], None).color,
            Some(rgb(255, 0, 0))
        );
        assert_eq!(cascade(&sheet, &[element("p", &[])], None).color, None);
    }

    #[test]
    fn custom_properties_inherit_and_resolve() {
        let sheet = sheet(":root { --ink: #123456 } p { color: var(--ink) }");
        let root = vec![element("html", &[])];
        let (computed, vars) = sheet.cascade(&root, None, &Computed::default(), &Vars::root(), 14);
        let stack = vec![element("html", &[]), element("p", &[])];
        let computed = sheet.cascade(&stack, None, &computed, &vars, 14).0;
        assert_eq!(computed.color, Some(rgb(0x12, 0x34, 0x56)));
    }

    #[test]
    fn a_var_declared_by_any_matching_rule_is_in_scope() {
        // The declaration reading it is written before the one setting it, and
        // by a rule of lower specificity: neither may matter.
        let sheet = sheet("p { color: var(--ink) } p.lead { --ink: red }");
        let stack = vec![element("p", &["lead"])];
        assert_eq!(cascade(&sheet, &stack, None).color, Some(rgb(255, 0, 0)));
    }

    #[test]
    fn an_unresolvable_var_takes_its_fallback_or_drops_the_declaration() {
        let sheet = sheet("p { color: var(--missing, blue); font-size: var(--gone) }");
        let stack = vec![element("p", &[])];
        let computed = cascade(&sheet, &stack, None);
        assert_eq!(computed.color, Some(rgb(0, 0, 255)));
        assert_eq!(computed.font_px, None);
    }

    #[test]
    fn a_var_cycle_does_not_hang() {
        let sheet = sheet(":root { --a: var(--b); --b: var(--a) } html { color: var(--a) }");
        let stack = vec![element("html", &[])];
        assert_eq!(cascade(&sheet, &stack, None).color, None);
    }

    #[test]
    fn cascade_layers_carry_their_rules() {
        let sheet = sheet("@layer base, utils; @layer base { p { color: red } }");
        let stack = vec![element("p", &[])];
        assert_eq!(cascade(&sheet, &stack, None).color, Some(rgb(255, 0, 0)));
    }

    #[test]
    fn margin_shorthand_reads_clockwise() {
        let sheet = sheet("p { margin: 1px 2px 3px 4px }");
        let stack = vec![element("p", &[])];
        let computed = cascade(&sheet, &stack, None);
        assert_eq!(computed.margin_top, Some(1));
        assert_eq!(computed.margin_bottom, Some(3));
        assert_eq!(computed.margin_left, Some(4));
        assert_eq!(computed.margin_right, Some(2));
    }

    #[test]
    fn a_margin_right_length_is_kept_and_auto_still_centres() {
        let sheet = sheet("p { margin-right: 3em } div { margin-right: auto }");
        let computed = cascade(&sheet, &[element("p", &[])], None);
        assert_eq!(computed.margin_right, Some(3 * 14));
        assert!(!computed.center);

        let centred = cascade(&sheet, &[element("div", &[])], None);
        assert_eq!(centred.margin_right, None);
        assert!(centred.center);
    }

    #[test]
    fn a_horizontal_margin_does_not_inherit() {
        let sheet = sheet("div { margin: 0 5px }");
        let outer = cascade(&sheet, &[element("div", &[])], None);
        assert_eq!(outer.margin_right, Some(5));
        assert_eq!(outer.inherit().margin_right, None);
        assert_eq!(outer.inherit().margin_left, None);
    }

    #[test]
    fn max_width_sets_the_measure() {
        let sheet = sheet("div { max-width: 40rem }");
        let computed = cascade(&sheet, &[element("div", &[])], None);
        assert_eq!(computed.measure, Some(40 * 14));
    }

    #[test]
    fn a_percentage_width_is_of_the_containing_block() {
        // Nothing constrains the outer box, so its half is half the window.
        let sheet = sheet("div { width: 50% } p { width: 50% }");
        let outer = cascade(&sheet, &[element("div", &[])], None);
        assert_eq!(outer.measure, Some(Viewport::default().width_px / 2));

        let stack = vec![element("div", &[]), element("p", &[])];
        let inner = sheet
            .cascade(&stack, None, &outer, &Vars::root(), 14)
            .0
            .measure;
        assert_eq!(inner, Some(Viewport::default().width_px / 4));
    }

    #[test]
    fn a_child_cannot_widen_past_its_container() {
        let sheet = sheet("div { max-width: 300px } p { width: 900px }");
        let outer = cascade(&sheet, &[element("div", &[])], None);
        let stack = vec![element("div", &[]), element("p", &[])];
        let inner = sheet.cascade(&stack, None, &outer, &Vars::root(), 14).0;
        assert_eq!(inner.measure, Some(300));
    }

    #[test]
    fn heights_are_read_and_do_not_inherit() {
        let sheet = sheet("div { height: 5em; min-height: 40px; max-height: 10rem }");
        let computed = cascade(&sheet, &[element("div", &[])], None);
        assert_eq!(computed.height, Some(5 * 14));
        assert_eq!(computed.min_height, Some(40));
        assert_eq!(computed.max_height, Some(10 * 14));
        assert_eq!(computed.inherit().height, None);
        assert_eq!(computed.inherit().min_height, None);
        assert_eq!(computed.inherit().max_height, None);
    }

    #[test]
    fn a_min_width_is_read_and_does_not_inherit() {
        let sheet = sheet("div { min-width: 20em }");
        let computed = cascade(&sheet, &[element("div", &[])], None);
        assert_eq!(computed.min_width, Some(20 * 14));
        assert_eq!(computed.inherit().min_width, None);
    }

    #[test]
    fn a_percentage_height_behaves_as_auto() {
        // The containing block's height is indefinite in a flowed column, so
        // css-sizing-3 §5.1 leaves the box content-sized.
        let sheet = sheet("div { height: 50%; min-height: 25% }");
        let computed = cascade(&sheet, &[element("div", &[])], None);
        assert_eq!(computed.height, None);
        assert_eq!(computed.min_height, None);
    }

    #[test]
    fn auto_horizontal_margins_centre_the_box() {
        let sheet = sheet("div { margin: 0 auto } section { margin-left: auto } p { margin: 1px }");
        assert!(cascade(&sheet, &[element("div", &[])], None).center);
        assert!(cascade(&sheet, &[element("section", &[])], None).center);
        assert!(!cascade(&sheet, &[element("p", &[])], None).center);
    }

    #[test]
    fn width_auto_leaves_the_inherited_measure_standing() {
        let sheet = sheet("div { max-width: 300px } p { width: auto }");
        let outer = cascade(&sheet, &[element("div", &[])], None);
        let stack = vec![element("div", &[]), element("p", &[])];
        let inner = sheet.cascade(&stack, None, &outer, &Vars::root(), 14).0;
        assert_eq!(inner.measure, Some(300));
    }

    #[test]
    fn text_align_is_read_and_inherited() {
        let sheet =
            sheet("div { text-align: center } p { text-align: right } li { text-align: justify }");
        let outer = cascade(&sheet, &[element("div", &[])], None);
        assert_eq!(outer.align, Align::Center);
        assert_eq!(
            cascade(&sheet, &[element("p", &[])], None).align,
            Align::Right
        );
        // Justification is not implemented, so it sets flush left rather than
        // being dropped and leaving an inherited centre standing.
        assert_eq!(
            cascade(&sheet, &[element("li", &[])], None).align,
            Align::Left
        );

        let stack = vec![element("div", &[]), element("span", &[])];
        let inner = sheet.cascade(&stack, None, &outer, &Vars::root(), 14).0;
        assert_eq!(inner.align, Align::Center);
    }

    #[test]
    fn text_decoration_reads_every_line_it_names() {
        let sheet = sheet(
            "p { text-decoration: underline } \
             del { text-decoration: line-through overline } \
             a { text-decoration: none } \
             h1 { text-decoration: underline wavy #f00 2px }",
        );
        let underlined = Decorations {
            underline: true,
            ..Decorations::default()
        };
        assert_eq!(
            cascade(&sheet, &[element("p", &[])], None).decoration,
            Some(underlined)
        );
        assert_eq!(
            cascade(&sheet, &[element("del", &[])], None).decoration,
            Some(Decorations {
                line_through: true,
                overline: true,
                ..Decorations::default()
            })
        );
        // `none` is an answer, not a silence: it has to reach the renderer so a
        // link's default underline is suppressed.
        assert_eq!(
            cascade(&sheet, &[element("a", &[])], None).decoration,
            Some(Decorations::default())
        );
        // A colour, a style and a thickness in the shorthand are ignored
        // without taking the line with them.
        assert_eq!(
            cascade(&sheet, &[element("h1", &[])], None).decoration,
            Some(underlined)
        );
    }

    #[test]
    fn text_decoration_inherits() {
        let sheet = sheet("div { text-decoration: line-through }");
        let outer = cascade(&sheet, &[element("div", &[])], None);
        let stack = vec![element("div", &[]), element("span", &[])];
        let inner = sheet.cascade(&stack, None, &outer, &Vars::root(), 14).0;
        assert_eq!(inner.decoration, outer.decoration);
    }

    #[test]
    fn vertical_align_resolves_against_the_font() {
        let sheet = sheet(
            "p { font-size: 30px } \
             .up { vertical-align: super } \
             .down { vertical-align: sub } \
             .flat { vertical-align: baseline } \
             .box { vertical-align: middle } \
             .len { vertical-align: -4px }",
        );
        let shift = |class: &str| cascade(&sheet, &[element("p", &[class])], None).shift;
        assert_eq!(shift("up"), Some(10));
        assert_eq!(shift("down"), Some(-6));
        assert_eq!(shift("flat"), Some(0));
        // No baseline to align against in a flat inline model, so the run
        // stays put rather than being put somewhere arbitrary.
        assert_eq!(shift("box"), Some(0));
        assert_eq!(shift("len"), Some(-4));
        assert_eq!(shift(""), None);
    }

    #[test]
    fn a_background_shorthand_keeps_its_colour() {
        let sheet =
            sheet("p { background: #112233 } div { background: url(x.png) no-repeat #fff }");
        assert_eq!(
            cascade(&sheet, &[element("p", &[])], None).background,
            Some(0xff112233)
        );
        assert_eq!(
            cascade(&sheet, &[element("div", &[])], None).background,
            Some(0xffffffff)
        );
    }

    #[test]
    fn a_background_does_not_inherit() {
        let sheet = sheet("div { background: red }");
        let outer = cascade(&sheet, &[element("div", &[])], None);
        assert_eq!(outer.background, Some(0xffff0000));
        let stack = vec![element("div", &[]), element("p", &[])];
        let inner = sheet.cascade(&stack, None, &outer, &Vars::root(), 14).0;
        assert_eq!(inner.background, None);
    }

    #[test]
    fn a_border_shorthand_reads_in_any_order() {
        let sheet = sheet("p { border-left: 4px solid #00ff00 } div { border: solid thin }");
        let quote = cascade(&sheet, &[element("p", &[])], None);
        assert_eq!(quote.borders.left.px(), 4);
        assert_eq!(quote.borders.left.color, Some(0xff00ff00));
        assert_eq!(quote.borders.top.px(), 0);

        // No colour is `currentColor`, which only the painter can resolve.
        let all = cascade(&sheet, &[element("div", &[])], None);
        assert_eq!(all.borders.bottom.px(), 1);
        assert_eq!(all.borders.bottom.color, None);
    }

    #[test]
    fn a_border_width_alone_paints_nothing() {
        let sheet = sheet("p { border-width: 6px } div { border-width: 6px; border-style: solid }");
        assert_eq!(
            cascade(&sheet, &[element("p", &[])], None).borders.top.px(),
            0
        );
        assert_eq!(
            cascade(&sheet, &[element("div", &[])], None)
                .borders
                .top
                .px(),
            6
        );
    }

    #[test]
    fn a_style_written_without_a_width_is_medium() {
        let sheet = sheet("p { border: solid }");
        assert_eq!(
            cascade(&sheet, &[element("p", &[])], None)
                .borders
                .right
                .px(),
            MEDIUM_BORDER
        );
    }

    #[test]
    fn padding_follows_the_four_value_shorthand() {
        let sheet = sheet("p { padding: 1px 2px 3px 4px } div { padding: 5px 10px }");
        let p = cascade(&sheet, &[element("p", &[])], None).padding;
        assert_eq!(
            (p.top, p.right, p.bottom, p.left),
            (Some(1), Some(2), Some(3), Some(4))
        );
        let d = cascade(&sheet, &[element("div", &[])], None).padding;
        assert_eq!(
            (d.top, d.right, d.bottom, d.left),
            (Some(5), Some(10), Some(5), Some(10))
        );
    }

    #[test]
    fn line_height_takes_a_number_a_length_or_normal() {
        let sheet = sheet(
            "p { line-height: 1.5 } pre { line-height: 24px } \
             blockquote { line-height: 150% } h1 { line-height: normal }",
        );
        let line = |tag| cascade(&sheet, &[element(tag, &[])], None).line;
        assert_eq!(line("p"), LineHeight::Scale(1.5));
        assert_eq!(line("pre"), LineHeight::Px(24));
        // A percentage is of the element's own font size, resolved once here.
        assert_eq!(line("blockquote"), LineHeight::Px(21));
        assert_eq!(line("h1"), LineHeight::Normal);
    }

    #[test]
    fn a_number_line_height_inherits_as_the_factor() {
        let sheet = sheet("body { line-height: 1.5; font-size: 10px } h1 { font-size: 40px }");
        let body = sheet.cascade(
            &[element("body", &[])],
            None,
            &Computed::default(),
            &Vars::root(),
            14,
        );
        let heading = sheet
            .cascade(
                &[element("body", &[]), element("h1", &[])],
                None,
                &body.0,
                &body.1,
                14,
            )
            .0;
        // The parent resolves to 15px, but the heading is led against its own
        // 40px rather than inheriting the length the parent landed on.
        assert_eq!(body.0.line.px(body.0.font_px.unwrap()), Some(15));
        assert_eq!(heading.line.px(heading.font_px.unwrap()), Some(60));
    }

    #[test]
    fn a_length_line_height_inherits_as_the_length() {
        let sheet = sheet("body { line-height: 20px } h1 { font-size: 40px }");
        let body = sheet.cascade(
            &[element("body", &[])],
            None,
            &Computed::default(),
            &Vars::root(),
            14,
        );
        let heading = sheet
            .cascade(
                &[element("body", &[]), element("h1", &[])],
                None,
                &body.0,
                &body.1,
                14,
            )
            .0;
        assert_eq!(heading.line.px(40), Some(20));
    }

    #[test]
    fn line_height_refuses_what_it_cannot_represent() {
        let sheet = sheet(
            "p { line-height: 0px } div { line-height: calc(1em + 2px) } \
             pre { line-height: -1 }",
        );
        // A zero length stacks every line on the one above it, so it is
        // clamped to a pixel; a negative factor and a `calc()` are refused
        // outright, leaving the inherited value standing.
        let line = |tag| cascade(&sheet, &[element(tag, &[])], None).line;
        assert_eq!(line("p").px(16), Some(1));
        assert_eq!(line("div"), LineHeight::Normal);
        assert_eq!(line("pre"), LineHeight::Normal);
    }

    #[test]
    fn white_space_is_read_and_inherited() {
        let sheet = sheet(
            "div { white-space: pre-wrap } p { white-space: nowrap } \
             li { white-space: pre-line } pre { white-space: balance }",
        );
        let ws = |tag| cascade(&sheet, &[element(tag, &[])], None).white_space;
        assert_eq!(ws("div"), Some(WhiteSpace::PreWrap));
        assert_eq!(ws("p"), Some(WhiteSpace::NoWrap));
        assert_eq!(ws("li"), Some(WhiteSpace::PreLine));
        // A keyword this cannot set leaves the property unset, which is what
        // lets `<pre>` keep the UA default.
        assert_eq!(ws("pre"), None);

        let stack = vec![element("div", &[]), element("span", &[])];
        let inner = sheet
            .cascade(
                &stack,
                None,
                &cascade(&sheet, &stack[..1], None),
                &Vars::root(),
                14,
            )
            .0;
        assert_eq!(inner.white_space, Some(WhiteSpace::PreWrap));
    }

    #[test]
    fn white_space_separates_keeping_from_wrapping() {
        // The two questions are independent, which is the whole point of the
        // property: `pre-wrap` keeps the source's spacing and still wraps,
        // `nowrap` collapses it and does not.
        assert!(WhiteSpace::PreWrap.keeps_spaces() && WhiteSpace::PreWrap.wraps());
        assert!(WhiteSpace::PreLine.keeps_newlines() && !WhiteSpace::PreLine.keeps_spaces());
        assert!(!WhiteSpace::NoWrap.keeps_newlines() && !WhiteSpace::NoWrap.wraps());
        assert!(WhiteSpace::Pre.keeps_spaces() && !WhiteSpace::Pre.wraps());
        assert!(WhiteSpace::Normal.wraps() && !WhiteSpace::Normal.keeps_newlines());
    }

    #[test]
    fn break_properties_are_read_and_inherited() {
        let sheet = sheet(
            "div { overflow-wrap: break-word } p { word-break: break-all } \
             li { word-wrap: anywhere } pre { word-break: keep-all } \
             blockquote { overflow-wrap: elsewhere }",
        );
        let wrap = |tag| cascade(&sheet, &[element(tag, &[])], None).wrap();
        assert_eq!(wrap("div"), Wrap::Overflow);
        assert_eq!(wrap("p"), Wrap::Anywhere);
        // The legacy alias is the same property, and `anywhere` differs only in
        // intrinsic sizing, which nothing here measures.
        assert_eq!(wrap("li"), Wrap::Overflow);
        // `keep-all` forbids breaks the breaker never took, and a keyword it
        // cannot read leaves the property alone.
        assert_eq!(wrap("pre"), Wrap::Word);
        assert_eq!(wrap("blockquote"), Wrap::Word);

        let stack = vec![element("div", &[]), element("span", &[])];
        let inner = sheet
            .cascade(
                &stack,
                None,
                &cascade(&sheet, &stack[..1], None),
                &Vars::root(),
                14,
            )
            .0;
        assert_eq!(inner.wrap(), Wrap::Overflow);
    }

    #[test]
    fn word_break_wins_over_overflow_wrap() {
        // The properties are independent, and `word-break: break-all` asks for
        // the cut in strictly more cases, so setting both is not a conflict.
        let sheet = sheet("p { overflow-wrap: break-word; word-break: break-all }");
        let computed = cascade(&sheet, &[element("p", &[])], None);
        assert_eq!(computed.wrap(), Wrap::Anywhere);
        // A last-resort cut happens only where a line of its own would not
        // hold the word; an anywhere cut happens wherever the line ends.
        assert!(!Wrap::Overflow.breaks(false) && Wrap::Overflow.breaks(true));
        assert!(Wrap::Anywhere.breaks(false));
        assert!(!Wrap::Word.breaks(true));
    }

    #[test]
    fn text_transform_is_read_and_inherited() {
        let sheet = sheet(
            "div { text-transform: uppercase } p { text-transform: capitalize } \
             li { text-transform: lowercase } pre { text-transform: full-width }",
        );
        let transform = |tag| cascade(&sheet, &[element(tag, &[])], None).transform;
        assert_eq!(transform("div"), Transform::Upper);
        assert_eq!(transform("p"), Transform::Capitalize);
        assert_eq!(transform("li"), Transform::Lower);
        // A keyword this cannot set is dropped, leaving what was inherited.
        assert_eq!(transform("pre"), Transform::None);

        let stack = vec![element("div", &[]), element("span", &[])];
        let inner = sheet
            .cascade(
                &stack,
                None,
                &cascade(&sheet, &stack[..1], None),
                &Vars::root(),
                14,
            )
            .0;
        assert_eq!(inner.transform, Transform::Upper);
    }

    #[test]
    fn transform_recases_at_word_boundaries() {
        assert_eq!(Transform::Upper.apply("edos v2"), "EDOS V2");
        assert_eq!(Transform::Lower.apply("EDOS v2"), "edos v2");
        assert_eq!(Transform::Capitalize.apply("edos"), "Edos");
        // A hyphen starts a word and an apostrophe does not.
        assert_eq!(Transform::Capitalize.apply("read-only"), "Read-Only");
        assert_eq!(Transform::Capitalize.apply("it's"), "It's");
        assert_eq!(Transform::None.apply("EdOS"), "EdOS");
    }

    #[test]
    fn text_indent_resolves_and_inherits() {
        let sheet = sheet(
            "div { text-indent: 2em } p { text-indent: 10% } \
             li { text-indent: -2em } pre { text-indent: 1em hanging }",
        );
        let indent = |tag| cascade(&sheet, &[element(tag, &[])], None).indent;
        assert_eq!(indent("div"), 28);
        assert_eq!(indent("p"), Viewport::default().width_px / 10);
        // A hanging indent has nothing to hang over at the page edge, and a
        // keyword this cannot honour leaves the property alone.
        assert_eq!(indent("li"), 0);
        assert_eq!(indent("pre"), 0);

        let stack = vec![element("div", &[]), element("p2", &[])];
        let inner = sheet
            .cascade(
                &stack,
                None,
                &cascade(&sheet, &stack[..1], None),
                &Vars::root(),
                14,
            )
            .0;
        assert_eq!(inner.indent, 28);
    }

    #[test]
    fn spacing_takes_a_sign_and_inherits() {
        let sheet = sheet(
            "div { letter-spacing: 2px; word-spacing: 0.5em } \
             p { letter-spacing: -1px } h1 { letter-spacing: normal } \
             pre { letter-spacing: 3 } code { word-spacing: -0.25em }",
        );
        let computed = |tag| cascade(&sheet, &[element(tag, &[])], None);
        assert_eq!(computed("div").letter_spacing, 2);
        assert_eq!(computed("div").word_spacing, 7);
        // A page tightening its display type writes a negative value and means
        // it, unlike a negative margin, which has nowhere to go.
        assert_eq!(computed("p").letter_spacing, -1);
        assert_eq!(computed("h1").letter_spacing, 0);
        // A bare number is not a length, so the declaration is dropped.
        assert_eq!(computed("pre").letter_spacing, 0);
        assert_eq!(computed("code").word_spacing, -4);

        // Both inherit, so a rule on a wrapper reaches the text inside it.
        let stack = vec![element("div", &[]), element("span", &[])];
        let inner = sheet
            .cascade(
                &stack,
                None,
                &cascade(&sheet, &stack[..1], None),
                &Vars::root(),
                14,
            )
            .0;
        assert_eq!(inner.letter_spacing, 2);
        assert_eq!(inner.word_spacing, 7);
    }

    #[test]
    fn list_style_type_parses_and_inherits() {
        let sheet = sheet(
            "ul { list-style-type: square } ol { list-style: lower-roman }              nav { list-style: none } menu { list-style: inside }              dir { list-style-type: cjk-earthly-branch }",
        );
        let style = |tag| cascade(&sheet, &[element(tag, &[])], None).list_style;
        assert_eq!(style("ul"), Some(ListStyle::Square));
        assert_eq!(style("ol"), Some(ListStyle::LowerRoman));
        assert_eq!(style("nav"), Some(ListStyle::None));
        // A shorthand carrying only a position still resets the type, and a
        // counter style this cannot draw leaves the property alone.
        assert_eq!(style("menu"), Some(ListStyle::Disc));
        assert_eq!(style("dir"), None);

        // The property inherits, which is what carries a rule on the list down
        // to the items that wear the marker.
        let stack = vec![element("ul", &[]), element("li", &[])];
        let inner = sheet
            .cascade(
                &stack,
                None,
                &cascade(&sheet, &stack[..1], None),
                &Vars::root(),
                14,
            )
            .0;
        assert_eq!(inner.list_style, Some(ListStyle::Square));
    }

    #[test]
    fn list_markers_count_in_their_own_style() {
        assert_eq!(ListStyle::Disc.marker(3), "\u{2022}");
        assert_eq!(ListStyle::None.marker(3), "");
        assert_eq!(ListStyle::Decimal.marker(12), "12.");
        assert_eq!(ListStyle::DecimalLeadingZero.marker(3), "03.");
        assert_eq!(ListStyle::DecimalLeadingZero.marker(12), "12.");
        assert_eq!(ListStyle::LowerAlpha.marker(1), "a.");
        assert_eq!(ListStyle::LowerAlpha.marker(26), "z.");
        assert_eq!(ListStyle::LowerAlpha.marker(27), "aa.");
        assert_eq!(ListStyle::UpperAlpha.marker(28), "AB.");
        assert_eq!(ListStyle::LowerRoman.marker(4), "iv.");
        assert_eq!(ListStyle::UpperRoman.marker(1994), "MCMXCIV.");
        // Outside the style's range a counter is written in decimal.
        assert_eq!(ListStyle::UpperRoman.marker(4000), "4000.");
        assert_eq!(ListStyle::LowerAlpha.marker(0), "0.");
        // The plain-text rendering spells the bullets in ASCII and leaves the
        // counting styles as they are.
        assert_eq!(ListStyle::Circle.ascii_marker(1), "o");
        assert_eq!(ListStyle::LowerRoman.ascii_marker(9), "ix.");
    }

    #[test]
    fn attribute_selector_tests_presence_and_value() {
        let sheet = sheet(
            "[hidden] { display: none }              a[href] { color: red }              a[href=\"/x\"] { color: blue }",
        );
        let plain = vec![element_with("a", &[])];
        let linked = vec![element_with("a", &[("href", "/y")])];
        let exact = vec![element_with("a", &[("href", "/x")])];
        assert_eq!(cascade(&sheet, &plain, None).color, None);
        assert_eq!(cascade(&sheet, &linked, None).color, Some(rgb(255, 0, 0)));
        assert_eq!(cascade(&sheet, &exact, None).color, Some(rgb(0, 0, 255)));
        assert!(cascade(&sheet, &vec![element_with("p", &[("hidden", "")])], None).hidden);
        assert!(!cascade(&sheet, &vec![element_with("p", &[])], None).hidden);
    }

    #[test]
    fn attribute_operators_follow_the_spec() {
        let sheet = sheet(
            "[class~=\"lead\"] { color: red }              [lang|=en] { color: blue }              [href^=https] { color: #00ff00 }              [href$=\".png\"] { color: #00ffff }              [title*=\"the middle\"] { color: #ff00ff }",
        );
        let red = rgb(255, 0, 0);
        assert_eq!(
            cascade(&sheet, &[element_with("p", &[("class", "a lead b")])], None).color,
            Some(red)
        );
        // `~=` compares whole words, never a substring of one.
        assert_eq!(
            cascade(&sheet, &[element_with("p", &[("class", "leader")])], None).color,
            None
        );
        assert_eq!(
            cascade(&sheet, &[element_with("p", &[("lang", "en-GB")])], None).color,
            Some(rgb(0, 0, 255))
        );
        assert_eq!(
            cascade(&sheet, &[element_with("p", &[("lang", "english")])], None).color,
            None
        );
        assert_eq!(
            cascade(
                &sheet,
                &[element_with("a", &[("href", "https://x/")])],
                None
            )
            .color,
            Some(rgb(0, 255, 0))
        );
        assert_eq!(
            cascade(&sheet, &[element_with("a", &[("href", "/a.png")])], None).color,
            Some(rgb(0, 255, 255))
        );
        // A quoted value keeps its space, which is what the tokenizer has to
        // hold on to.
        assert_eq!(
            cascade(
                &sheet,
                &[element_with("p", &[("title", "in the middle of")])],
                None
            )
            .color,
            Some(rgb(255, 0, 255))
        );
    }

    #[test]
    fn attribute_flag_folds_only_the_value() {
        let sheet = sheet("[lang=EN i] { color: red } [type=TEXT] { color: blue }");
        assert_eq!(
            cascade(&sheet, &[element_with("p", &[("lang", "en")])], None).color,
            Some(rgb(255, 0, 0))
        );
        assert_eq!(
            cascade(&sheet, &[element_with("input", &[("type", "text")])], None).color,
            None
        );
    }

    #[test]
    fn attribute_selector_counts_as_a_class() {
        let ranked = sheet("a[href] { color: red } a { color: blue }");
        assert_eq!(
            cascade(&ranked, &[element_with("a", &[("href", "/")])], None).color,
            Some(rgb(255, 0, 0))
        );
        // An unclosed bracket is not a selector, so the rule is dropped rather
        // than applied to every element.
        let unclosed = sheet("p[title { color: red }");
        assert_eq!(cascade(&unclosed, &[element("p", &[])], None).color, None);
    }

    /// The `n`th of `count` `li` children, all of the same tag.
    fn nth_child(n: usize, count: usize) -> Element {
        Element {
            tag: "li".to_string(),
            position: Siblings {
                index: n,
                count,
                type_index: n,
                type_count: count,
            },
            ..Element::default()
        }
    }

    #[test]
    fn parses_the_an_plus_b_microsyntax() {
        assert_eq!(parse_nth("odd"), Some((2, 1)));
        assert_eq!(parse_nth("EVEN"), Some((2, 0)));
        assert_eq!(parse_nth("3"), Some((0, 3)));
        assert_eq!(parse_nth("-2"), Some((0, -2)));
        assert_eq!(parse_nth("n"), Some((1, 0)));
        assert_eq!(parse_nth("2n"), Some((2, 0)));
        assert_eq!(parse_nth(" 2n + 1 "), Some((2, 1)));
        assert_eq!(parse_nth("-n+3"), Some((-1, 3)));
        assert_eq!(parse_nth("+3n-2"), Some((3, -2)));
        // An offset carries its own sign, and a selector this cannot read is
        // dropped rather than guessed at.
        assert_eq!(parse_nth("2n5"), None);
        assert_eq!(parse_nth(""), None);
        assert_eq!(parse_nth("two"), None);
    }

    #[test]
    fn nth_child_counts_from_either_end() {
        let sheet = sheet(
            "li:nth-child(2n+1) { color: red }
             li:nth-last-child(1) { font-weight: bold }
             li:nth-child(-n+2) { font-style: italic }",
        );
        let color = |n| cascade(&sheet, &[nth_child(n, 5)], None).color;
        assert_eq!(color(1), Some(rgb(255, 0, 0)));
        assert_eq!(color(2), None);
        assert_eq!(color(3), Some(rgb(255, 0, 0)));
        assert!(cascade(&sheet, &[nth_child(5, 5)], None).bold == Some(true));
        assert!(cascade(&sheet, &[nth_child(4, 5)], None).bold.is_none());
        // `-n+2` is the first two and nothing after them.
        assert_eq!(cascade(&sheet, &[nth_child(2, 5)], None).italic, Some(true));
        assert_eq!(cascade(&sheet, &[nth_child(3, 5)], None).italic, None);
    }

    #[test]
    fn structural_pseudo_classes_read_position() {
        let sheet = sheet(
            "li:first-child { color: red }
             li:last-child { color: blue }
             li:only-child { color: lime }",
        );
        assert_eq!(
            cascade(&sheet, &[nth_child(1, 3)], None).color,
            Some(rgb(255, 0, 0))
        );
        assert_eq!(
            cascade(&sheet, &[nth_child(3, 3)], None).color,
            Some(rgb(0, 0, 255))
        );
        assert_eq!(
            cascade(&sheet, &[nth_child(1, 1)], None).color,
            Some(rgb(0, 255, 0))
        );
    }

    #[test]
    fn of_type_counts_only_the_matching_tag() {
        let sheet = sheet("p:first-of-type { color: red } p:only-of-type { color: blue }");
        // The second element child, but the first `p` among them.
        let first_p = Element {
            tag: "p".to_string(),
            position: Siblings {
                index: 2,
                count: 4,
                type_index: 1,
                type_count: 2,
            },
            ..Element::default()
        };
        assert_eq!(
            cascade(&sheet, &[first_p], None).color,
            Some(rgb(255, 0, 0))
        );
        let only_p = Element {
            tag: "p".to_string(),
            position: Siblings {
                index: 3,
                count: 4,
                type_index: 1,
                type_count: 1,
            },
            ..Element::default()
        };
        assert_eq!(cascade(&sheet, &[only_p], None).color, Some(rgb(0, 0, 255)));
    }

    #[test]
    fn a_pseudo_class_counts_as_a_class_and_an_unknown_one_drops_the_rule() {
        let ranked = sheet("li:first-child { color: red } li { color: blue }");
        assert_eq!(
            cascade(&ranked, &[nth_child(1, 3)], None).color,
            Some(rgb(255, 0, 0))
        );
        // `:hover` has no answer here, and a rule matched without it would
        // paint every item; a pseudo-element is not a test on the element at
        // all.
        for source in ["li:hover { color: red }", "li::before { color: red }"] {
            assert_eq!(
                cascade(&sheet(source), &[nth_child(1, 3)], None).color,
                None
            );
        }
    }

    #[test]
    fn a_pseudo_class_composes_with_the_rest_of_its_compound() {
        let dropped = sheet("ul > li.item:nth-child(2):not-a-thing { color: red }");
        assert_eq!(cascade(&dropped, &[nth_child(2, 3)], None).color, None);
        let sheet = sheet("ul > li.item:nth-child( 2 ).lead { color: red }");
        let mut item = nth_child(2, 3);
        item.classes = vec!["item".to_string(), "lead".to_string()];
        assert_eq!(
            cascade(&sheet, &[element("ul", &[]), item.clone()], None).color,
            Some(rgb(255, 0, 0))
        );
        // The same element in the third position is not the second child.
        let mut third = nth_child(3, 3);
        third.classes = item.classes.clone();
        assert_eq!(
            cascade(&sheet, &[element("ul", &[]), third], None).color,
            None
        );
    }
}
