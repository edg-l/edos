//! gzip and gunzip: RFC 1952 containers over RFC 1951 deflate.
//!
//! One implementation behind two names, the way the tools have always been
//! paired: `gunzip` is `gzip -d`, and nothing else about it differs.

use flate2::{Compression, read::GzDecoder, write::GzEncoder};
use std::{
    env,
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

/// `decompress` is what the two binaries differ by, and a `-d` on the command
/// line can still turn it on.
pub fn run(decompress: bool) -> ExitCode {
    let name = if decompress { "gunzip" } else { "gzip" };

    let opts = match parse_args(decompress) {
        Ok(opts) => opts,
        Err(message) => {
            eprintln!("{}: {}", name, message);
            usage(name);
            return ExitCode::from(2);
        }
    };

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

fn parse_args(mut decompress: bool) -> Result<Options, String> {
    let mut stdout = false;
    let mut keep = false;
    let mut force = false;
    let mut level = Compression::default();
    let mut files = Vec::new();
    let mut end_of_options = false;

    for arg in env::args().skip(1) {
        if end_of_options || !arg.starts_with('-') || arg == "-" {
            files.push(arg);
            continue;
        }
        if arg == "--" {
            end_of_options = true;
            continue;
        }
        match arg.as_str() {
            "--decompress" | "--uncompress" => decompress = true,
            "--stdout" | "--to-stdout" => stdout = true,
            "--keep" => keep = true,
            "--force" => force = true,
            "--fast" => level = Compression::fast(),
            "--best" => level = Compression::best(),
            "--help" => return Err("help".to_string()),
            _ if arg.starts_with("--") => return Err(format!("unknown option {}", arg)),
            _ => {
                for c in arg[1..].chars() {
                    match c {
                        'd' => decompress = true,
                        'c' => stdout = true,
                        'k' => keep = true,
                        'f' => force = true,
                        '1'..='9' => {
                            level = Compression::new(c.to_digit(10).unwrap());
                        }
                        _ => return Err(format!("unknown option -{}", c)),
                    }
                }
            }
        }
    }

    Ok(Options {
        decompress,
        stdout,
        keep,
        force,
        level,
        files,
    })
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

fn usage(name: &str) {
    eprintln!("usage: {} [-cdfk] [-1..-9] [FILE...]", name);
    eprintln!("  -c  write to stdout and keep every input");
    eprintln!("  -d  decompress");
    eprintln!("  -f  overwrite an existing output file");
    eprintln!("  -k  keep the input file");
    eprintln!("  -1  fastest, -9 smallest (default 6)");
    eprintln!("With no FILE, or with '-', the standard streams are used.");
}
