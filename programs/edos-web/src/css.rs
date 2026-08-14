//! A CSS subset: the cascade, and the declarations that change how text is set.
//!
//! Stage 2 of `doc/design/browser.md`. What is here is chosen by what the block
//! list can already express -- colour, size, weight, face, decoration and the
//! vertical margins between blocks -- plus `display: none`, which is the one
//! declaration a document needs honoured before anything else, since a page
//! that hides its skip-links and its mobile navigation with CSS renders them as
//! stray text otherwise.
//!
//! Everything unrecognised is dropped rather than approximated. A declaration
//! this cannot represent is invisible, which is the same outcome the document
//! gets from a browser that never implemented it.

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
    pub underline: Option<bool>,
    pub margin_top: Option<u32>,
    pub margin_bottom: Option<u32>,
    pub margin_left: Option<u32>,
    /// `display: none`. Not inherited: a hidden element hides its subtree by
    /// not being walked at all, which is not the same thing as its children
    /// inheriting a value.
    pub hidden: bool,
}

impl Computed {
    /// The starting point for a child: the inherited properties survive, the
    /// rest resets.
    pub fn inherit(&self) -> Computed {
        Computed {
            hidden: false,
            margin_top: None,
            margin_bottom: None,
            margin_left: None,
            ..*self
        }
    }

    /// The font size this resolves relative lengths against.
    fn em(&self, root_px: u32) -> u32 {
        self.font_px.unwrap_or(root_px)
    }

    fn apply(&mut self, decl: &Declaration, root_px: u32, parent_px: u32) {
        match decl.name.as_str() {
            "color" => {
                if let Some(color) = parse_color(&decl.value) {
                    self.color = Some(color);
                }
            }
            "display" => self.hidden = decl.value.eq_ignore_ascii_case("none"),
            "visibility" => {
                if decl.value.eq_ignore_ascii_case("hidden") {
                    self.hidden = true;
                }
            }
            "font-size" => {
                if let Some(px) = parse_font_size(&decl.value, root_px, parent_px) {
                    self.font_px = Some(px);
                }
            }
            "font-weight" => self.bold = parse_weight(&decl.value).or(self.bold),
            "font-style" => match decl.value.as_str() {
                "italic" | "oblique" => self.italic = Some(true),
                "normal" => self.italic = Some(false),
                _ => {}
            },
            "font-family" => self.mono = Some(decl.value.contains("monospace")),
            "text-decoration" | "text-decoration-line" => {
                self.underline = Some(decl.value.contains("underline"));
            }
            "margin" => {
                let parts: Vec<Option<u32>> = decl
                    .value
                    .split_whitespace()
                    .map(|p| parse_length(p, root_px, self.em(parent_px)))
                    .collect();
                // One value sets all four, two set vertical then horizontal,
                // and three or four start at the top and go clockwise.
                let (top, bottom, left) = match parts.len() {
                    1 => (parts[0], parts[0], parts[0]),
                    2 => (parts[0], parts[0], parts[1]),
                    3 => (parts[0], parts[2], parts[1]),
                    4 => (parts[0], parts[2], parts[3]),
                    _ => (None, None, None),
                };
                self.margin_top = top.or(self.margin_top);
                self.margin_bottom = bottom.or(self.margin_bottom);
                self.margin_left = left.or(self.margin_left);
            }
            "margin-top" => self.margin_top = self.length(decl, root_px, parent_px),
            "margin-bottom" => self.margin_bottom = self.length(decl, root_px, parent_px),
            "margin-left" | "padding-left" => {
                self.margin_left = self.length(decl, root_px, parent_px)
            }
            _ => {}
        }
    }

    fn length(&self, decl: &Declaration, root_px: u32, parent_px: u32) -> Option<u32> {
        parse_length(&decl.value, root_px, self.em(parent_px))
    }
}

/// One `name: value` pair, with the value lowercased and whitespace collapsed.
#[derive(Clone, Debug)]
pub struct Declaration {
    pub name: String,
    pub value: String,
}

/// One simple selector: a tag, an id and any number of classes, all of which
/// must match the same element.
#[derive(Clone, Debug, Default)]
struct Compound {
    tag: Option<String>,
    id: Option<String>,
    classes: Vec<String>,
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
    }

    fn specificity(&self) -> (u32, u32, u32) {
        (
            self.id.is_some() as u32,
            self.classes.len() as u32,
            self.tag.is_some() as u32,
        )
    }
}

/// A descendant chain, the subject last: `nav ul li` is three compounds.
#[derive(Clone, Debug)]
struct Selector {
    parts: Vec<Compound>,
}

impl Selector {
    /// Match right to left against the open element stack, whose last entry is
    /// the element being matched. Ancestors are searched greedily from the
    /// nearest, which is what a descendant combinator means.
    fn matches(&self, stack: &[Element]) -> bool {
        let Some((subject, ancestors)) = self.parts.split_last() else {
            return false;
        };
        let Some((element, rest)) = stack.split_last() else {
            return false;
        };
        if !subject.matches(element) {
            return false;
        }
        let mut remaining = rest;
        for part in ancestors.iter().rev() {
            match remaining.iter().rposition(|e| part.matches(e)) {
                Some(index) => remaining = &remaining[..index],
                None => return false,
            }
        }
        true
    }

    fn specificity(&self) -> (u32, u32, u32) {
        self.parts.iter().fold((0, 0, 0), |acc, part| {
            let s = part.specificity();
            (acc.0 + s.0, acc.1 + s.1, acc.2 + s.2)
        })
    }
}

struct Rule {
    selector: Selector,
    declarations: Vec<Declaration>,
}

/// An element as the cascade sees it: everything a selector can ask about.
#[derive(Clone, Debug)]
pub struct Element {
    pub tag: String,
    pub id: Option<String>,
    pub classes: Vec<String>,
}

/// Every rule from every stylesheet the document carries, in cascade order.
#[derive(Default)]
pub struct Stylesheet {
    rules: Vec<Rule>,
}

impl Stylesheet {
    /// Add the rules in `source`, which is one `<style>` element's text.
    pub fn add(&mut self, source: &str) {
        parse_rules(source, &mut self.rules);
    }

    /// The style of the element on top of `stack`, cascading this sheet's
    /// matching rules over the inherited style and then the `style` attribute.
    ///
    /// Rules are applied in ascending specificity, ties going to the later
    /// rule, which is the cascade order for one origin with no `!important`.
    pub fn cascade(
        &self,
        stack: &[Element],
        inline: Option<&str>,
        parent: &Computed,
        root_px: u32,
    ) -> Computed {
        let parent_px = parent.font_px.unwrap_or(root_px);
        let mut computed = parent.inherit();

        let mut matched: Vec<(usize, &Rule)> = self
            .rules
            .iter()
            .enumerate()
            .filter(|(_, rule)| rule.selector.matches(stack))
            .collect();
        matched.sort_by_key(|(index, rule)| (rule.selector.specificity(), *index));
        for (_, rule) in matched {
            for decl in &rule.declarations {
                computed.apply(decl, root_px, parent_px);
            }
        }

        if let Some(inline) = inline {
            for decl in parse_declarations(inline) {
                computed.apply(&decl, root_px, parent_px);
            }
        }
        computed
    }
}

/// Strip comments, then read rule after rule until the source runs out.
fn parse_rules(source: &str, out: &mut Vec<Rule>) {
    let source = strip_comments(source);
    let bytes: Vec<char> = source.chars().collect();
    let mut i = 0;
    while i < bytes.len() {
        // An at-rule's body is skipped whole. `@media` and `@supports` are
        // conditional on a viewport and a feature set this has no answer for,
        // and applying their contents unconditionally is worse than ignoring
        // them: a page's mobile rules would win over its desktop ones.
        if bytes[i] == '@' {
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

/// A comma-separated selector list, dropping any selector using syntax this
/// does not implement rather than matching it too loosely.
fn parse_selectors(prelude: &str) -> Vec<Selector> {
    prelude
        .split(',')
        .filter_map(|text| parse_selector(text.trim()))
        .collect()
}

fn parse_selector(text: &str) -> Option<Selector> {
    if text.is_empty() {
        return None;
    }
    // A combinator other than descendant, an attribute selector, a pseudo
    // anything, or the universal selector inside a compound: all unsupported,
    // and a selector matched without them would apply far too widely.
    if text.contains([':', '[', '>', '+', '~', '(', '*']) {
        return None;
    }
    let mut parts = Vec::new();
    for word in text.split_whitespace() {
        parts.push(parse_compound(word)?);
    }
    (!parts.is_empty()).then_some(Selector { parts })
}

fn parse_compound(word: &str) -> Option<Compound> {
    let mut compound = Compound::default();
    let mut current = String::new();
    let mut kind = b' ';
    for ch in word.chars() {
        if ch == '#' || ch == '.' {
            push_part(kind, &mut current, &mut compound);
            kind = ch as u8;
        } else {
            current.push(ch);
        }
    }
    push_part(kind, &mut current, &mut compound);
    if compound.tag.is_none() && compound.id.is_none() && compound.classes.is_empty() {
        return None;
    }
    Some(compound)
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

/// A length in pixels. Relative units resolve against `em_px`; anything with a
/// unit that depends on a viewport or a container is refused, since guessing
/// one produces a size the document never asked for.
fn parse_length(value: &str, root_px: u32, em_px: u32) -> Option<u32> {
    let value = value.trim();
    if value == "0" || value == "auto" || value == "inherit" {
        return (value == "0").then_some(0);
    }
    let split = value.find(|c: char| !c.is_ascii_digit() && c != '.' && c != '-' && c != '+')?;
    let (number, unit) = value.split_at(split);
    let number: f32 = number.parse().ok()?;
    if number < 0.0 {
        return Some(0);
    }
    let px = match unit {
        "px" => number,
        "pt" => number * 4.0 / 3.0,
        "em" => number * em_px as f32,
        "rem" => number * root_px as f32,
        "%" => number * em_px as f32 / 100.0,
        _ => return None,
    };
    Some(px.round().max(0.0) as u32)
}

/// `#rgb`, `#rrggbb`, `rgb()`/`rgba()` and the handful of named colours a
/// hand-written page actually uses. Everything else, `var()` included, is
/// refused so the inherited colour stands.
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
            id: None,
            classes: classes.iter().map(|c| c.to_string()).collect(),
        }
    }

    fn sheet(source: &str) -> Stylesheet {
        let mut sheet = Stylesheet::default();
        sheet.add(source);
        sheet
    }

    #[test]
    fn specificity_orders_the_cascade() {
        let sheet = sheet("p { color: red } p.lead { color: blue } .lead { color: green }");
        let stack = vec![element("p", &["lead"])];
        let computed = sheet.cascade(&stack, None, &Computed::default(), 14);
        assert_eq!(computed.color, Some(rgb(0, 0, 255)));
    }

    #[test]
    fn inline_beats_every_rule() {
        let sheet = sheet("#x { color: red }");
        let stack = vec![Element {
            tag: "p".into(),
            id: Some("x".into()),
            classes: Vec::new(),
        }];
        let computed = sheet.cascade(&stack, Some("color: #00ff00"), &Computed::default(), 14);
        assert_eq!(computed.color, Some(rgb(0, 255, 0)));
    }

    #[test]
    fn descendant_selector_needs_the_ancestor() {
        let sheet = sheet("nav a { display: none }");
        let inside = vec![element("nav", &[]), element("ul", &[]), element("a", &[])];
        let outside = vec![element("p", &[]), element("a", &[])];
        assert!(
            sheet
                .cascade(&inside, None, &Computed::default(), 14)
                .hidden
        );
        assert!(
            !sheet
                .cascade(&outside, None, &Computed::default(), 14)
                .hidden
        );
    }

    #[test]
    fn media_queries_are_skipped_whole() {
        let sheet = sheet("@media (min-width: 50em) { p { display: none } } p { color: red }");
        let stack = vec![element("p", &[])];
        let computed = sheet.cascade(&stack, None, &Computed::default(), 14);
        assert!(!computed.hidden);
        assert_eq!(computed.color, Some(rgb(255, 0, 0)));
    }

    #[test]
    fn relative_sizes_resolve_against_the_parent() {
        let sheet = sheet("p { font-size: 2em }");
        let parent = Computed {
            font_px: Some(20),
            ..Computed::default()
        };
        let stack = vec![element("p", &[])];
        assert_eq!(sheet.cascade(&stack, None, &parent, 14).font_px, Some(40));
    }

    #[test]
    fn unsupported_selectors_do_not_match_too_widely() {
        let sheet = sheet("a:hover { display: none } p[hidden] { display: none }");
        let stack = vec![element("a", &[])];
        assert!(!sheet.cascade(&stack, None, &Computed::default(), 14).hidden);
    }

    #[test]
    fn margin_shorthand_reads_clockwise() {
        let sheet = sheet("p { margin: 1px 2px 3px 4px }");
        let stack = vec![element("p", &[])];
        let computed = sheet.cascade(&stack, None, &Computed::default(), 14);
        assert_eq!(computed.margin_top, Some(1));
        assert_eq!(computed.margin_bottom, Some(3));
        assert_eq!(computed.margin_left, Some(4));
    }
}
