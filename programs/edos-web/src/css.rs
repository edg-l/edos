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

use std::{borrow::Cow, collections::BTreeMap, rc::Rc};

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

    fn apply(&mut self, name: &str, value: &str, root_px: u32, parent_px: u32) {
        match name {
            "color" => {
                if let Some(color) = parse_color(value) {
                    self.color = Some(color);
                }
            }
            "display" => self.hidden = value.eq_ignore_ascii_case("none"),
            "visibility" => {
                if value.eq_ignore_ascii_case("hidden") {
                    self.hidden = true;
                }
            }
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
            "text-decoration" | "text-decoration-line" => {
                self.underline = Some(value.contains("underline"));
            }
            "margin" => {
                let parts: Vec<Option<u32>> = value
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
            "margin-top" => self.margin_top = self.length(value, root_px, parent_px),
            "margin-bottom" => self.margin_bottom = self.length(value, root_px, parent_px),
            "margin-left" | "padding-left" => {
                self.margin_left = self.length(value, root_px, parent_px)
            }
            _ => {}
        }
    }

    fn length(&self, value: &str, root_px: u32, parent_px: u32) -> Option<u32> {
        parse_length(value, root_px, self.em(parent_px))
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

/// Every rule from every stylesheet the document carries, in cascade order.
#[derive(Default)]
pub struct Stylesheet {
    rules: Vec<Rule>,
    viewport: Viewport,
}

impl Stylesheet {
    /// An empty sheet whose `@media` rules are answered for `viewport`.
    pub fn new(viewport: Viewport) -> Stylesheet {
        Stylesheet {
            rules: Vec::new(),
            viewport,
        }
    }

    /// Add the rules in `source`, which is one `<style>` element's text.
    pub fn add(&mut self, source: &str) {
        parse_rules(source, &mut self.rules, &self.viewport);
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
                computed.apply(&decl.name, &value, root_px, parent_px);
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
fn parse_rules(source: &str, out: &mut Vec<Rule>, viewport: &Viewport) {
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
                "media" => media_matches(&at_prelude(&bytes, i), viewport),
                // `@supports` and the rest are skipped whole, bodies included.
                _ => false,
            };
            if descend && let Some(body) = at_rule_body(&bytes, i) {
                parse_rules(&body, out, viewport);
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
    // `:root` is the one pseudo-class worth having: in an HTML document it is
    // the `html` element and nothing else, and it is where a stylesheet
    // declares the custom properties the rest of it reads.
    let text = text.replace(":root", "html");
    // A combinator other than descendant, an attribute selector, any other
    // pseudo, or the universal selector inside a compound: all unsupported, and
    // a selector matched without them would apply far too widely.
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
            id: None,
            classes: classes.iter().map(|c| c.to_string()).collect(),
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
    fn inline_beats_every_rule() {
        let sheet = sheet("#x { color: red }");
        let stack = vec![Element {
            tag: "p".into(),
            id: Some("x".into()),
            classes: Vec::new(),
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
    }
}
