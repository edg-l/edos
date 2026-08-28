//! gzip and gunzip: RFC 1952 containers over RFC 1951 deflate.
//!
//! One implementation behind two names, the way the tools have always been
//! paired: `gunzip` is `gzip -d`, and nothing else about it differs.

use edos_lib::args::{Opt, Spec, Value};
use flate2::{Compression, read::GzDecoder, write::GzEncoder};
use std::{
    fs::{self, File},
    io::{self, Read, Write},
    process::ExitCode,
};

const SUFFIX: &str = ".gz";

struct Options {
    decompress: bool,
    /// Write to standard output and leave every input file alone.
    stdout: bool,
    /// Keep the input file rather than replacing it with the result.
    keep: bool,
    force: bool,
    level: Compression,
    files: Vec<String>,
}

/// A digit is a level, and it has to be nine separate options rather than
/// `Spec::numeric`: `gzip -9k` is a cluster, so the digit must be a short
/// option the cluster loop can recognise, and `numeric` only ever sees an
/// argument that is digits all the way through.
const OPTS: &[Opt] = &[
    Opt::flag('d', "decompress", "decompress"),
    Opt {
        short: None,
        long: Some("uncompress"),
        value: Value::None,
        help: "a spelling of -d",
    },
    Opt::flag('c', "stdout", "write to stdout and keep every input"),
    Opt {
        short: None,
        long: Some("to-stdout"),
        value: Value::None,
        help: "a spelling of -c",
    },
    Opt::flag('k', "keep", "keep the input file"),
    Opt::flag('f', "force", "overwrite an existing output file"),
    Opt {
        short: None,
        long: Some("fast"),
        value: Value::None,
        help: "a spelling of -1",
    },
    Opt {
        short: None,
        long: Some("best"),
        value: Value::None,
        help: "a spelling of -9",
    },
    Opt::short_flag('1', "fastest"),
    Opt::short_flag('2', ""),
    Opt::short_flag('3', ""),
    Opt::short_flag('4', ""),
    Opt::short_flag('5', ""),
    Opt::short_flag('6', "the default"),
    Opt::short_flag('7', ""),
    Opt::short_flag('8', ""),
    Opt::short_flag('9', "smallest"),
];

const SYNOPSIS: &str =
    "[-cdfk] [-1..-9] [FILE...]\n\nWith no FILE, or with `-`, the standard streams are used.";

const GZIP: Spec = Spec::new("gzip", SYNOPSIS, OPTS);
const GUNZIP: Spec = Spec::new("gunzip", SYNOPSIS, OPTS);

/// `decompress` is what the two binaries differ by, and a `-d` on the command
/// line can still turn it on.
pub fn run(decompress: bool) -> ExitCode {
    let spec = if decompress { &GUNZIP } else { &GZIP };
    let name = spec.name;
    let opts = parse_args(spec, decompress);

    if opts.files.is_empty() {
        return match through_stdio(&opts) {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("{}: {}", name, e);
                ExitCode::FAILURE
            }
        };
    }

    let mut failed = false;
    for file in &opts.files {
        if let Err(e) = one_file(&opts, file) {
            eprintln!("{}: {}: {}", name, file, e);
            failed = true;
        }
    }

    if failed {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    }
}

/// One pass over the options in the order they were written, because the level
/// is last-wins: `gzip -9 -1` compresses fast, not small.
fn parse_args(spec: &Spec, mut decompress: bool) -> Options {
    let m = spec.parse_env();
    let mut stdout = false;
    let mut keep = false;
    let mut force = false;
    let mut level = Compression::default();

    for (opt, _) in m.occurrences() {
        match (opt.short, opt.long) {
            (Some('d'), _) | (_, Some("uncompress")) => decompress = true,
            (Some('c'), _) | (_, Some("to-stdout")) => stdout = true,
            (Some('k'), _) => keep = true,
            (Some('f'), _) => force = true,
            (_, Some("fast")) => level = Compression::fast(),
            (_, Some("best")) => level = Compression::best(),
            (Some(c), _) if c.is_ascii_digit() => {
                level = Compression::new(c.to_digit(10).expect("checked ascii digit"));
            }
            _ => {}
        }
    }

    Options {
        decompress,
        stdout,
        keep,
        force,
        level,
        files: m.positional().to_vec(),
    }
}

/// With no file operands the stream is the whole job, and the result always
/// goes to standard output whatever `-c` says.
fn through_stdio(opts: &Options) -> Result<(), String> {
    let stdin = io::stdin();
    let stdout = io::stdout();
    let mut input = stdin.lock();
    let mut output = stdout.lock();

    if opts.decompress {
        let mut decoder = GzDecoder::new(&mut input);
        io::copy(&mut decoder, &mut output).map_err(|e| e.to_string())?;
    } else {
        let mut encoder = GzEncoder::new(&mut output, opts.level);
        io::copy(&mut input, &mut encoder).map_err(|e| e.to_string())?;
        // The trailer holds the CRC and the uncompressed length, and only
        // `finish` writes it. A dropped encoder can do no more than discard the
        // error, leaving a stream that every other tool rejects.
        encoder.finish().map_err(|e| e.to_string())?;
    }
    output.flush().map_err(|e| e.to_string())
}

fn one_file(opts: &Options, path: &str) -> Result<(), String> {
    if path == "-" {
        return through_stdio(opts);
    }

    let target = if opts.decompress {
        match path.strip_suffix(SUFFIX) {
            Some(stem) => stem.to_string(),
            None if opts.stdout => path.to_string(),
            None => return Err(format!("does not end in {}", SUFFIX)),
        }
    } else {
        if path.ends_with(SUFFIX) && !opts.stdout {
            return Err(format!("already ends in {}", SUFFIX));
        }
        format!("{}{}", path, SUFFIX)
    };

    let mut source = File::open(path).map_err(|e| e.to_string())?;

    if opts.stdout {
        let stdout = io::stdout();
        let mut output = stdout.lock();
        convert(opts, &mut source, &mut output)?;
        return output.flush().map_err(|e| e.to_string());
    }

    if !opts.force && fs::exists(&target).unwrap_or(false) {
        return Err(format!("{} already exists", target));
    }

    let mut output = File::create(&target).map_err(|e| format!("{}: {}", target, e))?;
    // A failed conversion leaves a partial file behind, which is worse than no
    // file: it looks like a result. Remove it before reporting.
    if let Err(e) = convert(opts, &mut source, &mut output) {
        drop(output);
        let _ = fs::remove_file(&target);
        return Err(e);
    }
    output.flush().map_err(|e| e.to_string())?;
    drop(output);

    if !opts.keep {
        drop(source);
        fs::remove_file(path).map_err(|e| e.to_string())?;
    }
    Ok(())
}

fn convert(opts: &Options, source: &mut dyn Read, output: &mut dyn Write) -> Result<(), String> {
    if opts.decompress {
        let mut decoder = GzDecoder::new(source);
        io::copy(&mut decoder, output).map_err(|e| e.to_string())?;
    } else {
        let mut encoder = GzEncoder::new(output, opts.level);
        io::copy(source, &mut encoder).map_err(|e| e.to_string())?;
        encoder.finish().map_err(|e| e.to_string())?;
    }
    Ok(())
}
