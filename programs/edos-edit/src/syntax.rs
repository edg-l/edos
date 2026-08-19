//! One tokenizer over a table. Adding a language is a table entry, not code.
//!
//! Tokenizing is per line and cached on the line it belongs to (`Buffer::
//! tokens_for`, `Buffer::retokenize_from`). This module only turns one line
//! of text plus the state carried into it into a set of colored ranges.

use edos_render::theme::Theme;

/// What a token is colored as. `Text` is the fallback for a character no
/// other rule claimed.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum TokenKind {
    Keyword,
    String,
    Number,
    Type,
    Function,
    Comment,
    Operator,
    Punct,
    Special,
    Text,
}

/// A colored range within a line, in characters rather than bytes — the same
/// rule `Position` follows, for the same reason.
#[derive(Clone, Copy)]
pub struct Token {
    pub start: usize,
    pub len: usize,
    pub kind: TokenKind,
}

/// What Tab inserts, and how wide an indent guide step is.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Indent {
    Spaces(usize),
    Tab,
}

impl Indent {
    /// The literal text one press of Tab inserts.
    pub fn text(&self) -> String {
        match self {
            Indent::Spaces(n) => " ".repeat(*n),
            Indent::Tab => "\t".to_string(),
        }
    }

    /// What the status strip calls this indent.
    pub fn label(&self) -> String {
        match self {
            Indent::Spaces(1) => "1 space".to_string(),
            Indent::Spaces(n) => format!("{n} spaces"),
            Indent::Tab => "tabs".to_string(),
        }
    }

    /// Characters of leading whitespace one indent guide step covers.
    fn guide_step(&self) -> usize {
        match self {
            Indent::Spaces(n) => *n,
            Indent::Tab => 1,
        }
    }
}

/// Markdown is not keyword-shaped — headings, emphasis, code spans and links
/// need their own small pass — and a plain-text file has no tokens to find
/// at all. Everything else is one keyword-table tokenizer.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Flavour {
    CLike,
    Markdown,
    Plain,
}

/// One entry in the language table.
pub struct Lang {
    pub name: &'static str,
    pub exts: &'static [&'static str],
    /// Matched against the whole file name, for files with no extension.
    pub names: &'static [&'static str],
    pub keywords: &'static [&'static str],
    /// Names colored as types. Empty means "a leading capital is a type".
    pub types: &'static [&'static str],
    pub line_comment: Option<&'static str>,
    pub block_comment: Option<(&'static str, &'static str)>,
    pub strings: &'static [char],
    /// Whether a line that is wholly `[...]` names a section, and is coloured
    /// as one. Off for every language whose arrays can start a line, where the
    /// same shape is a value rather than a heading.
    pub section_headers: bool,
    pub indent: Indent,
    pub flavour: Flavour,
}

pub const LANGS: &[Lang] = &[
    Lang {
        name: "Rust",
        exts: &["rs"],
        names: &[],
        keywords: &[
            "as", "async", "await", "break", "const", "continue", "crate", "dyn", "else", "enum",
            "extern", "fn", "for", "if", "impl", "in", "let", "loop", "match", "mod", "move",
            "mut", "pub", "ref", "return", "self", "Self", "static", "struct", "super", "trait",
            "type", "unsafe", "use", "where", "while", "yield",
        ],
        types: &[],
        line_comment: Some("//"),
        block_comment: Some(("/*", "*/")),
        strings: &['"', '\''],
        section_headers: false,
        indent: Indent::Spaces(4),
        flavour: Flavour::CLike,
    },
    Lang {
        name: "C",
        exts: &["c", "h"],
        names: &[],
        keywords: &[
            "auto", "break", "case", "const", "continue", "default", "do", "else", "enum",
            "extern", "for", "goto", "if", "inline", "register", "return", "sizeof", "static",
            "struct", "switch", "typedef", "union", "volatile", "while",
        ],
        types: &[
            "void", "char", "short", "int", "long", "float", "double", "signed", "unsigned",
            "bool", "size_t", "int8_t", "int16_t", "int32_t", "int64_t", "uint8_t", "uint16_t",
            "uint32_t", "uint64_t", "FILE",
        ],
        line_comment: Some("//"),
        block_comment: Some(("/*", "*/")),
        strings: &['"', '\''],
        section_headers: false,
        indent: Indent::Spaces(4),
        flavour: Flavour::CLike,
    },
    Lang {
        name: "TOML",
        exts: &["toml"],
        names: &[],
        keywords: &[],
        types: &[],
        line_comment: Some("#"),
        block_comment: None,
        strings: &['"', '\''],
        section_headers: true,
        indent: Indent::Spaces(2),
        flavour: Flavour::CLike,
    },
    Lang {
        name: "JSON",
        exts: &["json"],
        names: &[],
        keywords: &[],
        types: &[],
        line_comment: None,
        block_comment: None,
        strings: &['"'],
        section_headers: false,
        indent: Indent::Spaces(2),
        flavour: Flavour::CLike,
    },
    Lang {
        name: "Shell",
        exts: &["sh", "bash"],
        names: &[".profile", ".bashrc", ".bash_profile", ".zshrc"],
        keywords: &[
            "break", "case", "continue", "declare", "do", "done", "elif", "else", "esac", "export",
            "fi", "for", "function", "if", "in", "local", "readonly", "return", "select", "then",
            "time", "until", "while",
        ],
        types: &[],
        line_comment: Some("#"),
        block_comment: None,
        strings: &['"', '\''],
        section_headers: false,
        indent: Indent::Tab,
        flavour: Flavour::CLike,
    },
    Lang {
        name: "Markdown",
        exts: &["md", "markdown"],
        names: &[],
        keywords: &[],
        types: &[],
        line_comment: None,
        block_comment: None,
        strings: &[],
        section_headers: false,
        indent: Indent::Spaces(2),
        flavour: Flavour::Markdown,
    },
    Lang {
        name: "Plain text",
        exts: &[],
        names: &[],
        keywords: &[],
        types: &[],
        line_comment: None,
        block_comment: None,
        strings: &[],
        section_headers: false,
        indent: Indent::Spaces(4),
        flavour: Flavour::Plain,
    },
];

/// The language for `path`, by extension, then by whole file name, then
/// plain text — always the last entry in `LANGS`.
pub fn for_path(path: &str) -> &'static Lang {
    let name = path.rsplit('/').next().unwrap_or(path);
    if let Some(ext) = extension(name)
        && let Some(lang) = LANGS
            .iter()
            .find(|l| l.exts.iter().any(|e| e.eq_ignore_ascii_case(ext)))
    {
        return lang;
    }
    if let Some(lang) = LANGS.iter().find(|l| l.names.contains(&name)) {
        return lang;
    }
    LANGS
        .iter()
        .find(|l| l.flavour == Flavour::Plain)
        .expect("the plain language is always in LANGS")
}

/// The part of `name` after its last dot, when it has one that is not the
/// whole name. A leading dot makes a hidden file, not an extension —
/// `.profile` has none.
fn extension(name: &str) -> Option<&str> {
    let at = name.rfind('.')?;
    if at == 0 || at + 1 == name.len() {
        return None;
    }
    Some(&name[at + 1..])
}

/// The color a token of `kind` draws in. No literal colour appears in this
/// file — every one comes from `Theme::DEFAULT`.
pub fn color(kind: TokenKind) -> u32 {
    let theme = &Theme::DEFAULT;
    match kind {
        TokenKind::Keyword => theme.syn_keyword.raw(),
        TokenKind::String => theme.syn_string.raw(),
        TokenKind::Number => theme.syn_number.raw(),
        TokenKind::Type => theme.syn_type.raw(),
        TokenKind::Function => theme.syn_function.raw(),
        TokenKind::Comment => theme.syn_comment.raw(),
        TokenKind::Operator => theme.syn_operator.raw(),
        TokenKind::Punct => theme.syn_punct.raw(),
        TokenKind::Special => theme.syn_special.raw(),
        TokenKind::Text => theme.text_primary.raw(),
    }
}

/// How far an indent guide steps for `lang`, used by `draw_pane`.
pub fn indent_guide_step(lang: &Lang) -> usize {
    lang.indent.guide_step()
}

/// Tokenize one line, given whether it begins inside a block comment.
/// Returns the tokens and whether the line still ends inside one — the next
/// line's `opens_in_block`.
pub fn tokenize(line: &str, opens_in_block: bool, lang: &Lang) -> (Vec<Token>, bool) {
    match lang.flavour {
        Flavour::CLike => tokenize_clike(line, opens_in_block, lang),
        Flavour::Markdown => (tokenize_markdown(line), false),
        Flavour::Plain => (
            vec![Token {
                start: 0,
                len: line.chars().count(),
                kind: TokenKind::Text,
            }],
            false,
        ),
    }
}

/// Characters read as an operator: `+ - = < > & | !` and friends.
const OPERATOR_CHARS: &str = "+-=<>&|!*/%^~?";
/// Characters read as punctuation: `( ) { } [ ] , ; :` and the member `.`.
const PUNCT_CHARS: &str = "(){}[],;:.";

/// The whole line as one `Type` token when it is nothing but a bracketed name,
/// as a TOML `[section]` or `[[array.of.tables]]` header is. Anything after the
/// closing bracket means the brackets held a value, so it is not a header.
fn section_header(chars: &[char]) -> Option<Token> {
    let start = chars.iter().position(|c| !c.is_whitespace())?;
    if chars[start] != '[' {
        return None;
    }
    let end = chars.iter().rposition(|c| !c.is_whitespace())?;
    if chars[end] != ']' || end <= start + 1 {
        return None;
    }
    Some(Token {
        start,
        len: end - start + 1,
        kind: TokenKind::Type,
    })
}

fn tokenize_clike(line: &str, opens_in_block: bool, lang: &Lang) -> (Vec<Token>, bool) {
    let chars: Vec<char> = line.chars().collect();
    let mut tokens = Vec::new();
    let mut i = 0;

    if opens_in_block {
        let close = lang.block_comment.map_or("", |(_, close)| close);
        match block_comment_token(&chars, 0, 0, close, &mut tokens) {
            Some(stop) => i = stop,
            None => return (tokens, true),
        }
    }

    if lang.section_headers
        && !opens_in_block
        && let Some(header) = section_header(&chars)
    {
        tokens.push(header);
        return (tokens, false);
    }

    while i < chars.len() {
        let c = chars[i];

        if c.is_whitespace() {
            i += 1;
            continue;
        }

        if let Some((open, close)) = lang.block_comment
            && starts_with_at(&chars, i, open)
        {
            let search_from = i + open.chars().count();
            match block_comment_token(&chars, i, search_from, close, &mut tokens) {
                Some(stop) => {
                    i = stop;
                    continue;
                }
                None => return (tokens, true),
            }
        }

        if let Some(lc) = lang.line_comment
            && starts_with_at(&chars, i, lc)
        {
            tokens.push(Token {
                start: i,
                len: chars.len() - i,
                kind: TokenKind::Comment,
            });
            break;
        }

        if lang.strings.contains(&c) {
            i = tokenize_string(&chars, i, c, &mut tokens);
            continue;
        }

        if c.is_ascii_digit() {
            let start = i;
            while i < chars.len()
                && (chars[i].is_ascii_alphanumeric() || chars[i] == '_' || chars[i] == '.')
            {
                i += 1;
            }
            tokens.push(Token {
                start,
                len: i - start,
                kind: TokenKind::Number,
            });
            continue;
        }

        if c == '#' && chars.get(i + 1) == Some(&'[') {
            let start = i;
            i += 2;
            let mut depth = 1;
            while i < chars.len() && depth > 0 {
                match chars[i] {
                    '[' => depth += 1,
                    ']' => depth -= 1,
                    _ => {}
                }
                i += 1;
            }
            tokens.push(Token {
                start,
                len: i - start,
                kind: TokenKind::Special,
            });
            continue;
        }

        if c == '$' {
            let start = i;
            i += 1;
            if chars.get(i) == Some(&'{') {
                i += 1;
                while i < chars.len() && chars[i] != '}' {
                    i += 1;
                }
                if i < chars.len() {
                    i += 1;
                }
            } else {
                while i < chars.len() && (chars[i].is_alphanumeric() || chars[i] == '_') {
                    i += 1;
                }
            }
            tokens.push(Token {
                start,
                len: i - start,
                kind: TokenKind::Special,
            });
            continue;
        }

        if c.is_alphabetic() || c == '_' {
            let start = i;
            while i < chars.len() && (chars[i].is_alphanumeric() || chars[i] == '_') {
                i += 1;
            }
            let word: String = chars[start..i].iter().collect();
            let kind = if matches!(word.as_str(), "true" | "false" | "null") {
                TokenKind::Number
            } else if lang.keywords.contains(&word.as_str()) {
                TokenKind::Keyword
            } else if lang.types.contains(&word.as_str())
                || (lang.types.is_empty() && word.chars().next().is_some_and(|c| c.is_uppercase()))
            {
                TokenKind::Type
            } else if chars.get(i) == Some(&'(') {
                TokenKind::Function
            } else {
                TokenKind::Text
            };
            tokens.push(Token {
                start,
                len: i - start,
                kind,
            });
            continue;
        }

        let kind = if OPERATOR_CHARS.contains(c) {
            TokenKind::Operator
        } else if PUNCT_CHARS.contains(c) {
            TokenKind::Punct
        } else {
            TokenKind::Text
        };
        tokens.push(Token {
            start: i,
            len: 1,
            kind,
        });
        i += 1;
    }

    (tokens, false)
}

/// A string or char literal starting at `i` (`chars[i] == quote`), split
/// into one `String` token per run of literal text with each escape
/// sequence's own `Special` token between them. Returns where the next
/// token can begin.
///
/// A bare `'` that does not close within a few characters is a Rust
/// lifetime, not a char literal — read as a single `Punct` so `'static`
/// does not swallow the rest of the line hunting for a quote that never
/// comes.
fn tokenize_string(chars: &[char], i: usize, quote: char, tokens: &mut Vec<Token>) -> usize {
    if quote == '\'' && !looks_like_char_literal(chars, i) {
        tokens.push(Token {
            start: i,
            len: 1,
            kind: TokenKind::Punct,
        });
        return i + 1;
    }

    let mut pos = i + 1;
    let mut seg_start = i;
    while pos < chars.len() && chars[pos] != quote {
        if chars[pos] == '\\' && pos + 1 < chars.len() {
            if pos > seg_start {
                tokens.push(Token {
                    start: seg_start,
                    len: pos - seg_start,
                    kind: TokenKind::String,
                });
            }
            let esc_start = pos;
            pos += 2;
            tokens.push(Token {
                start: esc_start,
                len: pos - esc_start,
                kind: TokenKind::Special,
            });
            seg_start = pos;
        } else {
            pos += 1;
        }
    }
    if pos < chars.len() {
        pos += 1; // the closing quote
    }
    if pos > seg_start {
        tokens.push(Token {
            start: seg_start,
            len: pos - seg_start,
            kind: TokenKind::String,
        });
    }
    pos
}

/// Whether `chars[i]` (a `'`) opens a char literal that closes nearby,
/// rather than a lifetime like `'static`.
fn looks_like_char_literal(chars: &[char], i: usize) -> bool {
    if chars.get(i + 1) == Some(&'\\') {
        let limit = (i + 8).min(chars.len());
        return (i + 2..limit).any(|j| chars[j] == '\'');
    }
    chars.get(i + 1).is_some() && chars.get(i + 2) == Some(&'\'')
}

/// Emits a `Comment` token from `token_start` to either `close`'s end
/// (returning where the next token can begin) or the end of the line
/// (returning `None`, meaning the line still ends inside the comment).
/// `search_from` lets a comment continuing from a previous line search from
/// its own start while a comment opening mid-line searches past its opening
/// delimiter — both cases emit one token starting at `token_start`.
fn block_comment_token(
    chars: &[char],
    token_start: usize,
    search_from: usize,
    close: &str,
    tokens: &mut Vec<Token>,
) -> Option<usize> {
    match find_at(chars, search_from, close) {
        Some(end) => {
            let stop = end + close.chars().count();
            tokens.push(Token {
                start: token_start,
                len: stop - token_start,
                kind: TokenKind::Comment,
            });
            Some(stop)
        }
        None => {
            tokens.push(Token {
                start: token_start,
                len: chars.len() - token_start,
                kind: TokenKind::Comment,
            });
            None
        }
    }
}

/// Whether `needle` matches `chars` starting exactly at `i`.
fn starts_with_at(chars: &[char], i: usize, needle: &str) -> bool {
    let needle: Vec<char> = needle.chars().collect();
    i + needle.len() <= chars.len() && chars[i..i + needle.len()] == needle[..]
}

/// The index of the first occurrence of `needle` at or after `from`.
fn find_at(chars: &[char], from: usize, needle: &str) -> Option<usize> {
    let needle: Vec<char> = needle.chars().collect();
    if needle.is_empty() {
        return None;
    }
    let mut i = from;
    while i + needle.len() <= chars.len() {
        if chars[i..i + needle.len()] == needle[..] {
            return Some(i);
        }
        i += 1;
    }
    None
}

/// The index of the first `target` at or after `from`.
fn find_char_at(chars: &[char], from: usize, target: char) -> Option<usize> {
    (from.min(chars.len())..chars.len()).find(|&i| chars[i] == target)
}

/// Headings become `Type`, code spans `String`, link text `Function`, link
/// targets `Special`, emphasis `Keyword` — no colour enters this pass that
/// is not already in the CLike palette above it.
fn tokenize_markdown(line: &str) -> Vec<Token> {
    let chars: Vec<char> = line.chars().collect();
    let mut tokens = Vec::new();

    let mut hashes = 0;
    while hashes < chars.len() && hashes < 6 && chars[hashes] == '#' {
        hashes += 1;
    }
    if hashes > 0 && (hashes == chars.len() || chars[hashes] == ' ') {
        tokens.push(Token {
            start: 0,
            len: chars.len(),
            kind: TokenKind::Type,
        });
        return tokens;
    }

    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];

        if c == '`'
            && let Some(end) = find_char_at(&chars, i + 1, '`')
        {
            tokens.push(Token {
                start: i,
                len: end + 1 - i,
                kind: TokenKind::String,
            });
            i = end + 1;
            continue;
        }

        if c == '['
            && let Some(close_bracket) = find_char_at(&chars, i + 1, ']')
            && chars.get(close_bracket + 1) == Some(&'(')
            && let Some(close_paren) = find_char_at(&chars, close_bracket + 2, ')')
        {
            tokens.push(Token {
                start: i,
                len: close_bracket + 1 - i,
                kind: TokenKind::Function,
            });
            tokens.push(Token {
                start: close_bracket + 1,
                len: close_paren + 1 - (close_bracket + 1),
                kind: TokenKind::Special,
            });
            i = close_paren + 1;
            continue;
        }

        if c == '*' || c == '_' {
            let marker = c;
            let double = chars.get(i + 1) == Some(&marker);
            let marker_len = if double { 2 } else { 1 };
            let needle: String = std::iter::repeat_n(marker, marker_len).collect();
            if let Some(close) = find_at(&chars, i + marker_len, &needle)
                && close > i + marker_len
            {
                let end = close + marker_len;
                tokens.push(Token {
                    start: i,
                    len: end - i,
                    kind: TokenKind::Keyword,
                });
                i = end;
                continue;
            }
        }

        i += 1;
    }

    tokens
}
