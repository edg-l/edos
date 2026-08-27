//! A backtracking matcher for POSIX basic and extended regular expressions,
//! with the GNU extensions sed scripts rely on: `\+`, `\?`, `\|`, `\{m,n\}`,
//! `\w`, `\s` and their negations.
//!
//! Text is matched as `&[char]`, so capture offsets index characters and can be
//! spliced back into a `String` without re-scanning UTF-8.

/// One element of a bracket expression.
#[derive(Clone)]
enum ClassItem {
    Ch(char),
    Range(char, char),
    Named(&'static str),
}

#[derive(Clone)]
enum Node {
    Char(char),
    Any,
    Class {
        neg: bool,
        items: Vec<ClassItem>,
    },
    Bol,
    Eol,
    Group(usize, Alt),
    Repeat {
        node: Box<Node>,
        min: u32,
        max: Option<u32>,
    },
}

#[derive(Clone)]
struct Seq(Vec<Node>);

#[derive(Clone)]
struct Alt(Vec<Seq>);

/// Capture slots. Slot 0 is the whole match, slot N the Nth `\(...\)` group.
pub type Caps = Vec<Option<(usize, usize)>>;

pub struct Regex {
    alt: Alt,
    ngroups: usize,
    icase: bool,
}

impl Regex {
    pub fn new(pattern: &str, ere: bool, icase: bool) -> Result<Regex, String> {
        let mut p = Parser {
            src: pattern.chars().collect(),
            pos: 0,
            ere,
            ngroups: 0,
        };
        let alt = p.parse_alt()?;
        if p.pos < p.src.len() {
            return Err(format!("unmatched ) at offset {}", p.pos));
        }
        Ok(Regex {
            alt,
            ngroups: p.ngroups,
            icase,
        })
    }

    /// Leftmost match at or after `start`.
    pub fn find_at(&self, text: &[char], start: usize) -> Option<Caps> {
        for s in start..=text.len() {
            let mut caps: Caps = vec![None; self.ngroups + 1];
            let mut end = None;
            let hit = m_alt(&self.alt, text, s, &mut caps, self.icase, &mut |p, _| {
                end = Some(p);
                true
            });
            if hit {
                caps[0] = Some((s, end.unwrap()));
                return Some(caps);
            }
        }
        None
    }

    pub fn is_match(&self, text: &[char]) -> bool {
        self.find_at(text, 0).is_some()
    }
}

struct Parser {
    src: Vec<char>,
    pos: usize,
    ere: bool,
    ngroups: usize,
}

impl Parser {
    fn peek(&self) -> Option<char> {
        self.src.get(self.pos).copied()
    }

    fn peek2(&self) -> Option<char> {
        self.src.get(self.pos + 1).copied()
    }

    /// True at `|` (ERE) or `\|` (BRE).
    fn at_alt(&self) -> bool {
        if self.ere {
            self.peek() == Some('|')
        } else {
            self.peek() == Some('\\') && self.peek2() == Some('|')
        }
    }

    /// True at `)` (ERE) or `\)` (BRE).
    fn at_group_close(&self) -> bool {
        if self.ere {
            self.peek() == Some(')')
        } else {
            self.peek() == Some('\\') && self.peek2() == Some(')')
        }
    }

    fn parse_alt(&mut self) -> Result<Alt, String> {
        let mut branches = vec![self.parse_seq()?];
        while self.at_alt() {
            self.pos += if self.ere { 1 } else { 2 };
            branches.push(self.parse_seq()?);
        }
        Ok(Alt(branches))
    }

    fn parse_seq(&mut self) -> Result<Seq, String> {
        let mut out: Vec<Node> = Vec::new();
        while self.pos < self.src.len() && !self.at_alt() && !self.at_group_close() {
            let c = self.src[self.pos];
            if c == '^' && (self.ere || out.is_empty()) {
                self.pos += 1;
                out.push(Node::Bol);
                continue;
            }
            if c == '$' && (self.ere || self.bre_dollar_is_anchor()) {
                self.pos += 1;
                out.push(Node::Eol);
                continue;
            }
            // In a BRE a leading `*` is an ordinary character.
            let mut atom = if c == '*' && !self.ere && out.is_empty() {
                self.pos += 1;
                Node::Char('*')
            } else {
                self.parse_atom()?
            };
            while let Some((min, max)) = self.parse_postfix()? {
                atom = Node::Repeat {
                    node: Box::new(atom),
                    min,
                    max,
                };
            }
            out.push(atom);
        }
        Ok(Seq(out))
    }

    /// In a BRE, `$` anchors only at the end of the pattern or of a branch.
    fn bre_dollar_is_anchor(&self) -> bool {
        match self.src.get(self.pos + 1) {
            None => true,
            Some('\\') => matches!(self.src.get(self.pos + 2), Some(')') | Some('|')),
            _ => false,
        }
    }

    fn parse_atom(&mut self) -> Result<Node, String> {
        let c = self.src[self.pos];
        self.pos += 1;
        match c {
            '.' => Ok(Node::Any),
            '[' => self.parse_class(),
            '(' if self.ere => self.parse_group(1),
            '\\' => {
                let n = self.peek().ok_or("trailing backslash")?;
                self.pos += 1;
                match n {
                    '(' if !self.ere => self.parse_group(2),
                    'n' => Ok(Node::Char('\n')),
                    't' => Ok(Node::Char('\t')),
                    'r' => Ok(Node::Char('\r')),
                    'w' | 'W' => Ok(Node::Class {
                        neg: n == 'W',
                        items: vec![ClassItem::Named("alnum"), ClassItem::Ch('_')],
                    }),
                    's' | 'S' => Ok(Node::Class {
                        neg: n == 'S',
                        items: vec![ClassItem::Named("space")],
                    }),
                    other => Ok(Node::Char(other)),
                }
            }
            other => Ok(Node::Char(other)),
        }
    }

    /// Parse a group whose opening delimiter was `open_len` characters long.
    fn parse_group(&mut self, open_len: usize) -> Result<Node, String> {
        self.ngroups += 1;
        let idx = self.ngroups;
        let inner = self.parse_alt()?;
        if !self.at_group_close() {
            return Err("unmatched (".to_string());
        }
        self.pos += open_len;
        Ok(Node::Group(idx, inner))
    }

    fn parse_class(&mut self) -> Result<Node, String> {
        let mut neg = false;
        if self.peek() == Some('^') {
            neg = true;
            self.pos += 1;
        }
        let mut items = Vec::new();
        let mut first = true;
        loop {
            let c = self.peek().ok_or("unterminated [")?;
            if c == ']' && !first {
                self.pos += 1;
                break;
            }
            first = false;
            // [:name:]
            if c == '[' && self.peek2() == Some(':') {
                let rest: String = self.src[self.pos + 2..].iter().collect();
                if let Some(end) = rest.find(":]") {
                    let name = &rest[..end];
                    let known = [
                        "alpha", "digit", "alnum", "space", "upper", "lower", "punct", "xdigit",
                        "blank", "print", "graph", "cntrl",
                    ];
                    if let Some(k) = known.iter().find(|k| **k == name) {
                        items.push(ClassItem::Named(k));
                        self.pos += 2 + end + 2;
                        continue;
                    }
                }
            }
            self.pos += 1;
            let lo = if c == '\\' {
                let n = self.peek().ok_or("trailing backslash in []")?;
                self.pos += 1;
                match n {
                    'n' => '\n',
                    't' => '\t',
                    'r' => '\r',
                    other => other,
                }
            } else {
                c
            };
            if self.peek() == Some('-') && self.peek2().is_some_and(|n| n != ']') {
                self.pos += 1;
                let hi = self.src[self.pos];
                self.pos += 1;
                items.push(ClassItem::Range(lo, hi));
            } else {
                items.push(ClassItem::Ch(lo));
            }
        }
        Ok(Node::Class { neg, items })
    }

    /// `*`, `\+`/`+`, `\?`/`?`, `\{m,n\}`/`{m,n}`.
    fn parse_postfix(&mut self) -> Result<Option<(u32, Option<u32>)>, String> {
        match self.peek() {
            Some('*') => {
                self.pos += 1;
                Ok(Some((0, None)))
            }
            Some('+') if self.ere => {
                self.pos += 1;
                Ok(Some((1, None)))
            }
            Some('?') if self.ere => {
                self.pos += 1;
                Ok(Some((0, Some(1))))
            }
            Some('{') if self.ere => {
                self.pos += 1;
                self.parse_interval(1)
            }
            Some('\\') if !self.ere => match self.peek2() {
                Some('+') => {
                    self.pos += 2;
                    Ok(Some((1, None)))
                }
                Some('?') => {
                    self.pos += 2;
                    Ok(Some((0, Some(1))))
                }
                Some('{') => {
                    self.pos += 2;
                    self.parse_interval(2)
                }
                _ => Ok(None),
            },
            _ => Ok(None),
        }
    }

    /// Body of `{m}` / `{m,}` / `{m,n}`; `close_len` is the width of the closer.
    fn parse_interval(&mut self, close_len: usize) -> Result<Option<(u32, Option<u32>)>, String> {
        let mut min = String::new();
        while self.peek().is_some_and(|c| c.is_ascii_digit()) {
            min.push(self.src[self.pos]);
            self.pos += 1;
        }
        let min: u32 = min.parse().map_err(|_| "bad {} interval".to_string())?;
        let max = if self.peek() == Some(',') {
            self.pos += 1;
            let mut hi = String::new();
            while self.peek().is_some_and(|c| c.is_ascii_digit()) {
                hi.push(self.src[self.pos]);
                self.pos += 1;
            }
            if hi.is_empty() {
                None
            } else {
                Some(hi.parse().map_err(|_| "bad {} interval".to_string())?)
            }
        } else {
            Some(min)
        };
        let closer_ok = if close_len == 1 {
            self.peek() == Some('}')
        } else {
            self.peek() == Some('\\') && self.peek2() == Some('}')
        };
        if !closer_ok {
            return Err("unterminated {} interval".to_string());
        }
        self.pos += close_len;
        Ok(Some((min, max)))
    }
}

fn eqc(a: char, b: char, icase: bool) -> bool {
    a == b || (icase && a.to_lowercase().eq(b.to_lowercase()))
}

fn named_match(name: &str, c: char) -> bool {
    match name {
        "alpha" => c.is_alphabetic(),
        "digit" => c.is_ascii_digit(),
        "alnum" => c.is_alphanumeric(),
        "space" => c.is_whitespace(),
        "upper" => c.is_uppercase(),
        "lower" => c.is_lowercase(),
        "punct" => c.is_ascii_punctuation(),
        "xdigit" => c.is_ascii_hexdigit(),
        "blank" => c == ' ' || c == '\t',
        "print" => !c.is_control(),
        "graph" => !c.is_control() && !c.is_whitespace(),
        "cntrl" => c.is_control(),
        _ => false,
    }
}

fn class_match(items: &[ClassItem], c: char, icase: bool) -> bool {
    let folded: Vec<char> = if icase {
        c.to_lowercase().chain(c.to_uppercase()).collect()
    } else {
        vec![c]
    };
    items.iter().any(|it| {
        folded.iter().any(|&f| match it {
            ClassItem::Ch(x) => *x == f,
            ClassItem::Range(lo, hi) => *lo <= f && f <= *hi,
            ClassItem::Named(n) => named_match(n, f),
        })
    })
}

type Cont<'a> = &'a mut dyn FnMut(usize, &mut Caps) -> bool;

fn m_alt(alt: &Alt, t: &[char], pos: usize, caps: &mut Caps, ic: bool, k: Cont) -> bool {
    for seq in &alt.0 {
        let saved = caps.clone();
        if m_seq(&seq.0, t, pos, caps, ic, k) {
            return true;
        }
        *caps = saved;
    }
    false
}

fn m_seq(nodes: &[Node], t: &[char], pos: usize, caps: &mut Caps, ic: bool, k: Cont) -> bool {
    match nodes.split_first() {
        None => k(pos, caps),
        Some((n, rest)) => m_node(n, t, pos, caps, ic, &mut |p, c| m_seq(rest, t, p, c, ic, k)),
    }
}

fn m_node(n: &Node, t: &[char], pos: usize, caps: &mut Caps, ic: bool, k: Cont) -> bool {
    match n {
        Node::Char(c) => pos < t.len() && eqc(t[pos], *c, ic) && k(pos + 1, caps),
        Node::Any => pos < t.len() && k(pos + 1, caps),
        Node::Class { neg, items } => {
            pos < t.len() && (class_match(items, t[pos], ic) != *neg) && k(pos + 1, caps)
        }
        Node::Bol => pos == 0 && k(pos, caps),
        Node::Eol => pos == t.len() && k(pos, caps),
        Node::Group(idx, alt) => {
            let (idx, start) = (*idx, pos);
            m_alt(alt, t, pos, caps, ic, &mut |p, c| {
                let old = c[idx];
                c[idx] = Some((start, p));
                if k(p, c) {
                    true
                } else {
                    c[idx] = old;
                    false
                }
            })
        }
        Node::Repeat { node, min, max } => m_rep(node, *min, *max, t, pos, caps, ic, 0, k),
    }
}

#[expect(
    clippy::too_many_arguments,
    reason = "the repetition matcher threads the node, its bounds, the input position, the captures and the continuation through one frame"
)]
fn m_rep(
    node: &Node,
    min: u32,
    max: Option<u32>,
    t: &[char],
    pos: usize,
    caps: &mut Caps,
    ic: bool,
    count: u32,
    k: Cont,
) -> bool {
    // Greedy: consume another repetition before yielding to the continuation.
    if max.is_none_or(|m| count < m) {
        let saved = caps.clone();
        let more = m_node(node, t, pos, caps, ic, &mut |p, c| {
            // An empty repetition would loop forever; stop once the minimum is met.
            p != pos && m_rep(node, min, max, t, p, c, ic, count + 1, k)
        });
        if more {
            return true;
        }
        *caps = saved;
    }
    count >= min && k(pos, caps)
}
