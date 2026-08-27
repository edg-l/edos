//! Command-line option parsing for the CLI programs.
//!
//! A program declares a [`Spec`] and calls [`Spec::parse_env`]; the parser
//! handles short clusters (`-abc`), attached and separated option values
//! (`-n5`, `-n 5`, `--lines=5`, `--lines 5`), `--` as end-of-options, `-` as
//! the positional that means stdin, and `--help`. Every program that parses
//! through here therefore accepts the same syntax, which a per-program flag
//! loop cannot promise.

use std::env;
use std::process::exit;

/// Whether an option carries a value, and what to call it in the usage text.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Value {
    /// A flag: `-c`.
    None,
    /// A value is required: `-n5`, `-n 5`, `--lines=5`, `--lines 5`.
    Required(&'static str),
    /// A value is taken only when attached: `-i.bak` sets it, `-i` does not
    /// and does not eat the next argument. This is what `sed -i` needs.
    Optional(&'static str),
}

/// One option a program accepts.
pub struct Opt {
    pub short: Option<char>,
    pub long: Option<&'static str>,
    pub value: Value,
    pub help: &'static str,
}

impl Opt {
    /// A flag with both spellings.
    pub const fn flag(short: char, long: &'static str, help: &'static str) -> Self {
        Opt {
            short: Some(short),
            long: Some(long),
            value: Value::None,
            help,
        }
    }

    /// A flag with no long spelling.
    pub const fn short_flag(short: char, help: &'static str) -> Self {
        Opt {
            short: Some(short),
            long: None,
            value: Value::None,
            help,
        }
    }

    /// An option taking a required value.
    pub const fn arg(
        short: char,
        long: &'static str,
        meta: &'static str,
        help: &'static str,
    ) -> Self {
        Opt {
            short: Some(short),
            long: Some(long),
            value: Value::Required(meta),
            help,
        }
    }

    /// An option taking a required value, with no long spelling.
    pub const fn short_arg(short: char, meta: &'static str, help: &'static str) -> Self {
        Opt {
            short: Some(short),
            long: None,
            value: Value::Required(meta),
            help,
        }
    }

    /// An option whose value is taken only when written attached to it.
    pub const fn optional_arg(
        short: char,
        long: &'static str,
        meta: &'static str,
        help: &'static str,
    ) -> Self {
        Opt {
            short: Some(short),
            long: Some(long),
            value: Value::Optional(meta),
            help,
        }
    }
}

/// What a program accepts: its name, its one-line synopsis, and its options.
pub struct Spec {
    pub name: &'static str,
    pub synopsis: &'static str,
    pub opts: &'static [Opt],
    /// The option a bare `-<digits>` argument sets, as `head -20` sets `-n`.
    /// Without it a leading digit is an unknown option, which is what every
    /// tool but `head` and `tail` wants.
    pub numeric_shorthand: Option<char>,
}

impl Spec {
    pub const fn new(name: &'static str, synopsis: &'static str, opts: &'static [Opt]) -> Self {
        Spec {
            name,
            synopsis,
            opts,
            numeric_shorthand: None,
        }
    }

    /// Accept `-<digits>` as a value for the option named by `short`.
    pub const fn numeric(mut self, short: char) -> Self {
        self.numeric_shorthand = Some(short);
        self
    }

    /// Parse the process arguments, exiting on `--help` or a bad argument.
    ///
    /// `--help` prints the usage text on stdout and exits 0; a parse error
    /// prints on stderr and exits 1. Both are what a user of a coreutil
    /// expects, and neither is something a caller can usefully recover from.
    pub fn parse_env(&self) -> Matches<'_> {
        let argv: Vec<String> = env::args().skip(1).collect();
        match self.parse(&argv) {
            Ok(m) if m.help => {
                print!("{}", self.usage());
                exit(0);
            }
            Ok(m) => m,
            Err(e) => {
                eprintln!("{}: {}", self.name, e);
                eprintln!("try '{} --help'", self.name);
                exit(1);
            }
        }
    }

    /// Parse `argv`, which must not contain the program name.
    pub fn parse(&self, argv: &[String]) -> Result<Matches<'_>, Error> {
        let mut m = Matches {
            spec: self,
            hits: Vec::new(),
            positional: Vec::new(),
            help: false,
        };
        let mut i = 0;
        while i < argv.len() {
            let arg = argv[i].as_str();
            i += 1;

            // `--` ends the options; `-` is the positional meaning stdin.
            if arg == "--" {
                m.positional.extend(argv[i..].iter().cloned());
                break;
            }
            if arg == "-" || !arg.starts_with('-') {
                m.positional.push(arg.to_string());
                continue;
            }

            if let Some(long) = arg.strip_prefix("--") {
                let (name, attached) = match long.split_once('=') {
                    Some((n, v)) => (n, Some(v.to_string())),
                    None => (long, None),
                };
                if name == "help" {
                    m.help = true;
                    continue;
                }
                let idx = self
                    .opts
                    .iter()
                    .position(|o| o.long == Some(name))
                    .ok_or_else(|| Error::Unknown(format!("--{}", name)))?;
                let value = match (self.opts[idx].value, attached) {
                    (Value::None, Some(_)) => {
                        return Err(Error::UnexpectedValue(format!("--{}", name)));
                    }
                    (Value::None, None) => None,
                    (_, Some(v)) => Some(v),
                    (Value::Optional(_), None) => Some(String::new()),
                    (Value::Required(_), None) => {
                        let v = argv
                            .get(i)
                            .ok_or_else(|| Error::MissingValue(format!("--{}", name)))?;
                        i += 1;
                        Some(v.clone())
                    }
                };
                m.hits.push((idx, value));
                continue;
            }

            let body = &arg[1..];
            if let Some(short) = self.numeric_shorthand
                && body.chars().all(|c| c.is_ascii_digit())
            {
                let idx = self.short_index(short)?;
                m.hits.push((idx, Some(body.to_string())));
                continue;
            }

            let chars: Vec<char> = body.chars().collect();
            let mut j = 0;
            while j < chars.len() {
                let c = chars[j];
                j += 1;
                let idx = self.short_index(c)?;
                let rest: String = chars[j..].iter().collect();
                let value = match self.opts[idx].value {
                    Value::None => None,
                    Value::Optional(_) => {
                        j = chars.len();
                        Some(rest)
                    }
                    Value::Required(_) => {
                        if rest.is_empty() {
                            let v = argv
                                .get(i)
                                .ok_or_else(|| Error::MissingValue(format!("-{}", c)))?;
                            i += 1;
                            Some(v.clone())
                        } else {
                            j = chars.len();
                            Some(rest)
                        }
                    }
                };
                m.hits.push((idx, value));
            }
        }
        Ok(m)
    }

    fn short_index(&self, c: char) -> Result<usize, Error> {
        self.opts
            .iter()
            .position(|o| o.short == Some(c))
            .ok_or_else(|| Error::Unknown(format!("-{}", c)))
    }

    /// The `--help` text: name, synopsis, then one line per option.
    pub fn usage(&self) -> String {
        let mut s = format!("usage: {} {}\n", self.name, self.synopsis);
        if self.opts.is_empty() {
            return s;
        }
        s.push('\n');
        let mut rows: Vec<(String, &str)> = Vec::new();
        for o in self.opts {
            let meta = match o.value {
                Value::None => String::new(),
                Value::Required(m) => format!(" {}", m),
                Value::Optional(m) => format!("[{}]", m),
            };
            let head = match (o.short, o.long) {
                (Some(c), Some(l)) => format!("-{}, --{}{}", c, l, meta),
                (Some(c), None) => format!("-{}{}", c, meta),
                (None, Some(l)) => format!("    --{}{}", l, meta),
                (None, None) => continue,
            };
            rows.push((head, o.help));
        }
        rows.push(("    --help".to_string(), "print this message and exit"));
        let width = rows.iter().map(|(h, _)| h.len()).max().unwrap_or(0);
        for (head, help) in rows {
            s.push_str(&format!("  {:<width$}  {}\n", head, help, width = width));
        }
        s
    }

    /// Report a usage error the parser cannot see, such as a missing operand.
    pub fn fail(&self, msg: &str) -> ! {
        eprintln!("{}: {}", self.name, msg);
        eprintln!("try '{} --help'", self.name);
        exit(1);
    }
}

/// Why a command line could not be parsed.
#[derive(Debug)]
pub enum Error {
    Unknown(String),
    MissingValue(String),
    UnexpectedValue(String),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::Unknown(o) => write!(f, "unknown option {}", o),
            Error::MissingValue(o) => write!(f, "option {} requires a value", o),
            Error::UnexpectedValue(o) => write!(f, "option {} takes no value", o),
        }
    }
}

impl std::error::Error for Error {}

/// What a command line said.
pub struct Matches<'a> {
    spec: &'a Spec,
    hits: Vec<(usize, Option<String>)>,
    positional: Vec<String>,
    help: bool,
}

impl Matches<'_> {
    /// Was the short option given?
    pub fn is_set(&self, short: char) -> bool {
        self.index_of(short)
            .is_some_and(|i| self.hits.iter().any(|(h, _)| *h == i))
    }

    /// The last value given for a short option, if any.
    pub fn value(&self, short: char) -> Option<&str> {
        let i = self.index_of(short)?;
        self.hits
            .iter()
            .rev()
            .find(|(h, _)| *h == i)
            .and_then(|(_, v)| v.as_deref())
    }

    /// The value of a short option parsed as `T`, or `None` when the option
    /// was not given. A value that will not parse is a usage error, not a
    /// silent default: `head -n banana` printing ten lines hides a typo.
    pub fn parsed<T: std::str::FromStr>(&self, short: char) -> Option<T> {
        let raw = self.value(short)?;
        match raw.parse() {
            Ok(v) => Some(v),
            Err(_) => self
                .spec
                .fail(&format!("invalid value for -{}: {}", short, raw)),
        }
    }

    /// Every option given, in the order it appeared, paired with its value.
    /// `sed` needs this: `-e` and `-f` build one script between them, so the
    /// order they were written in is part of the meaning.
    pub fn occurrences(&self) -> impl Iterator<Item = (&Opt, Option<&str>)> {
        self.hits
            .iter()
            .map(|(i, v)| (&self.spec.opts[*i], v.as_deref()))
    }

    /// The non-option arguments, in order. `-` is one of them.
    pub fn positional(&self) -> &[String] {
        &self.positional
    }

    fn index_of(&self, short: char) -> Option<usize> {
        self.spec.opts.iter().position(|o| o.short == Some(short))
    }
}
