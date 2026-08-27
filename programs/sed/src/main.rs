//! A stream editor: addresses, `s///`, `y///`, `p`, `d`, `q`, `=`, `a`, `i`, `c`.

mod regex;

use edos_lib::args::{Opt, Spec};
use regex::{Caps, Regex};
use std::fs;
use std::io::{self, BufRead};
use std::process::exit;

#[derive(Clone, Copy, PartialEq)]
enum Case {
    Upper,
    Lower,
}

enum ReplPart {
    Lit(String),
    /// `&` is group 0.
    Group(usize),
    /// `\U`, `\L`, `\u`, `\l`; `None` is `\E`.
    Case(Option<Case>, bool),
}

enum Addr {
    Line(usize),
    Last,
    Re(Regex),
}

struct AddrSpec {
    a1: Option<Addr>,
    a2: Option<Addr>,
    neg: bool,
    active: bool,
}

enum Cmd {
    Subst {
        re: Regex,
        repl: Vec<ReplPart>,
        global: bool,
        nth: usize,
        print: bool,
    },
    Translit(Vec<char>, Vec<char>),
    Print,
    Delete,
    Quit(i32),
    LineNum,
    Append(String),
    Insert(String),
    Change(String),
    Block(Vec<Command>),
}

/// What a command did to the rest of the script for this line.
enum Flow {
    Next,
    Delete,
    Quit(i32),
}

struct Command {
    addr: AddrSpec,
    cmd: Cmd,
}

impl AddrSpec {
    fn none() -> AddrSpec {
        AddrSpec {
            a1: None,
            a2: None,
            neg: false,
            active: false,
        }
    }

    /// Returns (selected, at range end).
    fn selects(&mut self, lineno: usize, line: &[char], last: bool) -> (bool, bool) {
        let (hit, end) = match (&self.a1, &self.a2) {
            (None, _) => (true, true),
            (Some(a1), None) => (addr_match(a1, lineno, line, last), true),
            (Some(a1), Some(a2)) => {
                if self.active {
                    if addr_match(a2, lineno, line, last)
                        || matches!(a2, Addr::Line(n) if *n <= lineno)
                    {
                        self.active = false;
                        (true, true)
                    } else {
                        (true, false)
                    }
                } else if addr_match(a1, lineno, line, last) {
                    // A numeric end address at or before the start line ends it here.
                    if matches!(a2, Addr::Line(n) if *n <= lineno) {
                        (true, true)
                    } else {
                        self.active = true;
                        (true, false)
                    }
                } else {
                    (false, false)
                }
            }
        };
        if self.neg { (!hit, end) } else { (hit, end) }
    }
}

fn addr_match(a: &Addr, lineno: usize, line: &[char], last: bool) -> bool {
    match a {
        Addr::Line(n) => *n == lineno,
        Addr::Last => last,
        Addr::Re(re) => re.is_match(line),
    }
}

fn die(msg: &str) -> ! {
    eprintln!("sed: {}", msg);
    exit(1);
}

struct ScriptParser {
    src: Vec<char>,
    pos: usize,
    ere: bool,
}

impl ScriptParser {
    fn peek(&self) -> Option<char> {
        self.src.get(self.pos).copied()
    }

    fn skip_blank(&mut self) {
        while self.peek().is_some_and(|c| c == ' ' || c == '\t') {
            self.pos += 1;
        }
    }

    fn skip_separators(&mut self) {
        while self
            .peek()
            .is_some_and(|c| c == ' ' || c == '\t' || c == '\n' || c == ';')
        {
            self.pos += 1;
        }
    }

    fn number(&mut self) -> usize {
        let mut n = 0usize;
        while let Some(c) = self.peek() {
            match c.to_digit(10) {
                Some(d) => {
                    n = n * 10 + d as usize;
                    self.pos += 1;
                }
                None => break,
            }
        }
        n
    }

    /// Read up to the next unescaped `delim`, turning `\<delim>` into a bare
    /// delimiter and leaving every other escape pair intact.
    fn until_delim(&mut self, delim: char) -> Result<String, String> {
        let mut out = String::new();
        loop {
            let c = self.peek().ok_or(format!("unterminated `{}'", delim))?;
            self.pos += 1;
            if c == delim {
                return Ok(out);
            }
            if c == '\\' {
                let n = self.peek().ok_or("trailing backslash".to_string())?;
                self.pos += 1;
                if n == delim {
                    out.push(delim);
                } else {
                    out.push('\\');
                    out.push(n);
                }
            } else {
                out.push(c);
            }
        }
    }

    fn parse_addr(&mut self) -> Result<Option<Addr>, String> {
        match self.peek() {
            Some(c) if c.is_ascii_digit() => Ok(Some(Addr::Line(self.number()))),
            Some('$') => {
                self.pos += 1;
                Ok(Some(Addr::Last))
            }
            Some('/') => {
                self.pos += 1;
                let src = self.until_delim('/')?;
                let mut icase = false;
                if self.peek() == Some('I') {
                    icase = true;
                    self.pos += 1;
                }
                Ok(Some(Addr::Re(Regex::new(&src, self.ere, icase)?)))
            }
            _ => Ok(None),
        }
    }

    /// Text operand of `a`, `i` and `c`, in both the one-line and `a\` forms.
    fn text_operand(&mut self) -> String {
        self.skip_blank();
        if self.peek() == Some('\\') {
            self.pos += 1;
            if self.peek() == Some('\n') {
                self.pos += 1;
            }
        }
        let mut out = String::new();
        while let Some(c) = self.peek() {
            self.pos += 1;
            if c == '\\' {
                match self.peek() {
                    Some('\n') => {
                        self.pos += 1;
                        out.push('\n');
                    }
                    Some(n) => {
                        self.pos += 1;
                        out.push(n);
                    }
                    None => {}
                }
                continue;
            }
            if c == '\n' {
                break;
            }
            out.push(c);
        }
        out
    }

    /// Parse commands until the end of the script, or until the `}` that closes
    /// the enclosing block when `nested`.
    fn parse(&mut self, nested: bool) -> Result<Vec<Command>, String> {
        let mut cmds = Vec::new();
        loop {
            self.skip_separators();
            let Some(c) = self.peek() else {
                if nested {
                    return Err("unmatched `{'".to_string());
                }
                break;
            };
            if c == '}' {
                if !nested {
                    return Err("unexpected `}'".to_string());
                }
                self.pos += 1;
                break;
            }
            if c == '#' {
                while self.peek().is_some_and(|c| c != '\n') {
                    self.pos += 1;
                }
                continue;
            }
            let mut addr = AddrSpec::none();
            addr.a1 = self.parse_addr()?;
            if addr.a1.is_some() {
                self.skip_blank();
                if self.peek() == Some(',') {
                    self.pos += 1;
                    self.skip_blank();
                    addr.a2 = Some(self.parse_addr()?.ok_or("expected address after ,")?);
                }
            }
            self.skip_blank();
            while self.peek() == Some('!') {
                addr.neg = !addr.neg;
                self.pos += 1;
                self.skip_blank();
            }
            let letter = self.peek().ok_or("missing command")?;
            self.pos += 1;
            let cmd = match letter {
                '{' => Cmd::Block(self.parse(true)?),
                's' => self.parse_subst()?,
                'y' => self.parse_translit()?,
                'p' => Cmd::Print,
                'd' => Cmd::Delete,
                '=' => Cmd::LineNum,
                'q' => {
                    self.skip_blank();
                    let code = if self.peek().is_some_and(|c| c.is_ascii_digit()) {
                        self.number() as i32
                    } else {
                        0
                    };
                    Cmd::Quit(code)
                }
                'a' => Cmd::Append(self.text_operand()),
                'i' => Cmd::Insert(self.text_operand()),
                'c' => Cmd::Change(self.text_operand()),
                other => return Err(format!("unknown command `{}'", other)),
            };
            cmds.push(Command { addr, cmd });
        }
        Ok(cmds)
    }

    fn parse_subst(&mut self) -> Result<Cmd, String> {
        let delim = self.peek().ok_or("`s' needs a delimiter")?;
        if delim == '\\' || delim == '\n' {
            return Err("invalid `s' delimiter".to_string());
        }
        self.pos += 1;
        let pattern = self.until_delim(delim)?;
        let replacement = self.until_delim(delim)?;
        let (mut global, mut print, mut icase, mut nth) = (false, false, false, 0usize);
        while let Some(c) = self.peek() {
            match c {
                'g' => global = true,
                'p' => print = true,
                'i' | 'I' => icase = true,
                '0'..='9' => {
                    nth = self.number();
                    continue;
                }
                _ => break,
            }
            self.pos += 1;
        }
        Ok(Cmd::Subst {
            re: Regex::new(&pattern, self.ere, icase)?,
            repl: parse_replacement(&replacement),
            global,
            nth: nth.max(1),
            print,
        })
    }

    fn parse_translit(&mut self) -> Result<Cmd, String> {
        let delim = self.peek().ok_or("`y' needs a delimiter")?;
        self.pos += 1;
        let from = unescape(&self.until_delim(delim)?);
        let to = unescape(&self.until_delim(delim)?);
        if from.len() != to.len() {
            return Err("strings for `y' command are different lengths".to_string());
        }
        Ok(Cmd::Translit(from, to))
    }
}

fn unescape(s: &str) -> Vec<char> {
    let mut out = Vec::new();
    let mut it = s.chars();
    while let Some(c) = it.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match it.next() {
            Some('n') => out.push('\n'),
            Some('t') => out.push('\t'),
            Some('r') => out.push('\r'),
            Some(other) => out.push(other),
            None => out.push('\\'),
        }
    }
    out
}

fn parse_replacement(s: &str) -> Vec<ReplPart> {
    let mut parts = Vec::new();
    let mut lit = String::new();
    let chars: Vec<char> = s.chars().collect();
    let mut i = 0;
    let flush = |lit: &mut String, parts: &mut Vec<ReplPart>| {
        if !lit.is_empty() {
            parts.push(ReplPart::Lit(std::mem::take(lit)));
        }
    };
    while i < chars.len() {
        let c = chars[i];
        i += 1;
        if c == '&' {
            flush(&mut lit, &mut parts);
            parts.push(ReplPart::Group(0));
            continue;
        }
        if c != '\\' {
            lit.push(c);
            continue;
        }
        let Some(&n) = chars.get(i) else {
            lit.push('\\');
            break;
        };
        i += 1;
        match n {
            '0'..='9' => {
                flush(&mut lit, &mut parts);
                parts.push(ReplPart::Group(n as usize - '0' as usize));
            }
            'n' => lit.push('\n'),
            't' => lit.push('\t'),
            'r' => lit.push('\r'),
            'U' | 'L' | 'u' | 'l' | 'E' => {
                flush(&mut lit, &mut parts);
                let case = match n {
                    'U' | 'u' => Some(Case::Upper),
                    'L' | 'l' => Some(Case::Lower),
                    _ => None,
                };
                parts.push(ReplPart::Case(case, n == 'u' || n == 'l'));
            }
            other => lit.push(other),
        }
    }
    if !lit.is_empty() {
        parts.push(ReplPart::Lit(lit));
    }
    parts
}

/// Append `text` to `out`, honouring the `\U`/`\l` state machine.
fn push_cased(out: &mut String, text: &str, span: &mut Option<Case>, once: &mut Option<Case>) {
    for ch in text.chars() {
        let case = once.take().or(*span);
        match case {
            Some(Case::Upper) => out.extend(ch.to_uppercase()),
            Some(Case::Lower) => out.extend(ch.to_lowercase()),
            None => out.push(ch),
        }
    }
}

fn expand(repl: &[ReplPart], text: &[char], caps: &Caps) -> String {
    let mut out = String::new();
    let mut span = None;
    let mut once = None;
    for part in repl {
        match part {
            ReplPart::Lit(s) => push_cased(&mut out, s, &mut span, &mut once),
            ReplPart::Group(n) => {
                if let Some(Some((a, b))) = caps.get(*n) {
                    let s: String = text[*a..*b].iter().collect();
                    push_cased(&mut out, &s, &mut span, &mut once);
                }
            }
            ReplPart::Case(c, single) => {
                if *single {
                    once = *c;
                } else {
                    span = *c;
                    once = None;
                }
            }
        }
    }
    out
}

fn substitute(
    re: &Regex,
    repl: &[ReplPart],
    global: bool,
    nth: usize,
    line: &str,
) -> Option<String> {
    let text: Vec<char> = line.chars().collect();
    let mut out = String::new();
    let mut pos = 0usize;
    let mut seen = 0usize;
    let mut changed = false;
    let mut prev_end: Option<usize> = None;
    while pos <= text.len() {
        let Some(caps) = re.find_at(&text, pos) else {
            break;
        };
        let (start, end) = caps[0].unwrap();
        out.extend(&text[pos..start]);
        // An empty match abutting the previous one is not a new occurrence.
        if start == end && prev_end == Some(start) {
            if start < text.len() {
                out.push(text[start]);
            }
            pos = start + 1;
            prev_end = None;
            continue;
        }
        seen += 1;
        if if global { seen >= nth } else { seen == nth } {
            out.push_str(&expand(repl, &text, &caps));
            changed = true;
        } else {
            out.extend(&text[start..end]);
        }
        prev_end = Some(end);
        // An empty match must still advance, or the scan never terminates.
        if end == start {
            if start < text.len() {
                out.push(text[start]);
            }
            pos = start + 1;
        } else {
            pos = end;
        }
        if !global && seen >= nth {
            break;
        }
    }
    if !changed {
        return None;
    }
    if pos < text.len() {
        out.extend(&text[pos..]);
    }
    Some(out)
}

struct Options {
    quiet: bool,
    ere: bool,
    in_place: Option<String>,
}

const SPEC: Spec = Spec::new(
    "sed",
    "[-nEs] [-i[SUFFIX]] [-e SCRIPT] [-f FILE] [SCRIPT] [file...]",
    &[
        Opt::flag('n', "quiet", "print only what the script asks for"),
        Opt::flag('E', "regexp-extended", "use extended regular expressions"),
        Opt::short_flag('r', "the same as -E"),
        Opt::short_flag('s', "treat each file separately, which is already the case"),
        Opt::arg(
            'e',
            "expression",
            "SCRIPT",
            "add SCRIPT to the commands to run",
        ),
        Opt::arg(
            'f',
            "file",
            "FILE",
            "add the contents of FILE to the commands to run",
        ),
        Opt::optional_arg(
            'i',
            "in-place",
            "SUFFIX",
            "edit files in place, keeping a SUFFIX backup",
        ),
    ],
);

fn main() {
    let m = SPEC.parse_env();
    let opts = Options {
        quiet: m.is_set('n'),
        ere: m.is_set('E') || m.is_set('r'),
        in_place: m.value('i').map(str::to_string),
    };
    let mut script_parts: Vec<String> = Vec::new();
    // `-e` and `-f` build one script between them, so the order they were
    // written in is part of the meaning.
    for (opt, value) in m.occurrences() {
        let Some(value) = value else { continue };
        match opt.short {
            Some('e') => script_parts.push(value.to_string()),
            Some('f') => match fs::read_to_string(value) {
                Ok(s) => script_parts.push(s),
                Err(e) => die(&format!("{}: {}", value, e)),
            },
            _ => {}
        }
    }
    let mut operands: Vec<String> = m.positional().to_vec();

    if script_parts.is_empty() {
        if operands.is_empty() {
            SPEC.fail("no script given");
        }
        script_parts.push(operands.remove(0));
    }
    let script = script_parts.join("\n");

    let mut parser = ScriptParser {
        src: script.chars().collect(),
        pos: 0,
        ere: opts.ere,
    };
    let mut cmds = match parser.parse(false) {
        Ok(c) => c,
        Err(e) => die(&format!("-e expression #1, char {}: {}", parser.pos, e)),
    };

    if opts.in_place.is_some() && operands.is_empty() {
        die("no input files given for -i");
    }

    let status = if let Some(suffix) = opts.in_place.clone() {
        let mut status = 0;
        for file in &operands {
            let lines = match fs::read_to_string(file) {
                Ok(c) => c.lines().map(str::to_string).collect::<Vec<_>>(),
                Err(e) => {
                    eprintln!("sed: {}: {}", file, e);
                    status = 2;
                    continue;
                }
            };
            for c in cmds.iter_mut() {
                c.addr.active = false;
            }
            let mut out = String::new();
            run(&mut cmds, &lines, opts.quiet, &mut |line| {
                out.push_str(line);
                out.push('\n');
            });
            if !suffix.is_empty() {
                let backup = format!("{}{}", file, suffix);
                if let Err(e) = fs::copy(file, &backup) {
                    eprintln!("sed: {}: {}", backup, e);
                    status = 2;
                    continue;
                }
            }
            if let Err(e) = fs::write(file, out.as_bytes()) {
                eprintln!("sed: {}: {}", file, e);
                status = 2;
            }
        }
        status
    } else {
        let mut lines: Vec<String> = Vec::new();
        let mut status = 0;
        if operands.is_empty() {
            for line in io::stdin().lock().lines() {
                match line {
                    Ok(l) => lines.push(l),
                    Err(_) => break,
                }
            }
        } else {
            for file in &operands {
                if file == "-" {
                    for line in io::stdin().lock().lines().map_while(Result::ok) {
                        lines.push(line);
                    }
                    continue;
                }
                match fs::read_to_string(file) {
                    Ok(c) => lines.extend(c.lines().map(str::to_string)),
                    Err(e) => {
                        eprintln!("sed: {}: {}", file, e);
                        status = 2;
                    }
                }
            }
        }
        let quit = run(&mut cmds, &lines, opts.quiet, &mut |line| {
            println!("{}", line);
        });
        if status == 0 { quit } else { status }
    };
    exit(status);
}

/// Run the script over `lines`, emitting output through `emit`. Returns the
/// exit code requested by a `q` command, or 0.
fn run(cmds: &mut [Command], lines: &[String], quiet: bool, emit: &mut dyn FnMut(&str)) -> i32 {
    let last_no = lines.len();
    for (idx, raw) in lines.iter().enumerate() {
        let lineno = idx + 1;
        let last = lineno == last_no;
        let mut space = raw.clone();
        let mut appended: Vec<String> = Vec::new();
        let flow = apply(cmds, lineno, last, &mut space, &mut appended, emit);

        if !quiet && !matches!(flow, Flow::Delete) {
            emit(&space);
        }
        for text in &appended {
            emit(text);
        }
        if let Flow::Quit(code) = flow {
            return code;
        }
    }
    0
}

/// Run one line through a command list (the whole script, or one `{}` block).
fn apply(
    cmds: &mut [Command],
    lineno: usize,
    last: bool,
    space: &mut String,
    appended: &mut Vec<String>,
    emit: &mut dyn FnMut(&str),
) -> Flow {
    for c in cmds.iter_mut() {
        let Command { addr, cmd } = c;
        let chars: Vec<char> = match addr.a1 {
            Some(_) => space.chars().collect(),
            None => Vec::new(),
        };
        let (hit, range_end) = addr.selects(lineno, &chars, last);
        if !hit {
            continue;
        }
        match cmd {
            Cmd::Block(inner) => match apply(inner, lineno, last, space, appended, emit) {
                Flow::Next => {}
                other => return other,
            },
            Cmd::Subst {
                re,
                repl,
                global,
                nth,
                print,
            } => {
                if let Some(new) = substitute(re, repl, *global, *nth, space) {
                    *space = new;
                    if *print {
                        emit(space);
                    }
                }
            }
            Cmd::Translit(from, to) => {
                *space = space
                    .chars()
                    .map(|ch| match from.iter().position(|f| *f == ch) {
                        Some(k) => to[k],
                        None => ch,
                    })
                    .collect();
            }
            Cmd::Print => emit(space),
            Cmd::Delete => return Flow::Delete,
            Cmd::LineNum => emit(&lineno.to_string()),
            Cmd::Quit(code) => return Flow::Quit(*code),
            Cmd::Append(text) => appended.push(text.clone()),
            Cmd::Insert(text) => emit(text),
            Cmd::Change(text) => {
                // For a range, the text replaces the range as a whole.
                if range_end {
                    emit(text);
                }
                return Flow::Delete;
            }
        }
    }
    Flow::Next
}
