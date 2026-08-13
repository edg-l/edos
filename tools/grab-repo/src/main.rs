//! grab-repo - build, sign and publish the `grab` package repository.
//!
//! Runs on the host. It reads a `pkg.toml` beside each program that declares
//! itself packaged, packs the binary the build staged, writes the index, and
//! signs it with the repository key.

mod manifest;
mod pack;

use ed25519_dalek::{Signer, SigningKey};
use grab_index::{Index, Package};
use std::{
    env,
    fs::{self, File},
    io::Read,
    path::{Path, PathBuf},
    process::ExitCode,
};

const USAGE: &str = "usage: grab-repo [--programs DIR] [--staging DIR] [--repo DIR] [--key FILE] [--repo-name NAME]";

struct Options {
    programs: PathBuf,
    staging: PathBuf,
    repo: PathBuf,
    key: PathBuf,
    repo_name: String,
}

fn main() -> ExitCode {
    let opts = match parse_args() {
        Ok(Some(opts)) => opts,
        Ok(None) => return ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("grab-repo: {}\n{}", e, USAGE);
            return ExitCode::from(2);
        }
    };

    match publish(&opts) {
        Ok(count) => {
            println!("published {} package(s) to {}", count, opts.repo.display());
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("grab-repo: {}", e);
            ExitCode::FAILURE
        }
    }
}

fn parse_args() -> Result<Option<Options>, String> {
    let home = env::var("HOME").unwrap_or_default();
    let mut opts = Options {
        programs: PathBuf::from("programs"),
        staging: PathBuf::from("pkgstage"),
        repo: PathBuf::from("/srv/edos-pkg"),
        key: PathBuf::from(format!("{}/.config/edos/grab-repo.key", home)),
        repo_name: "edos".to_string(),
    };

    let mut args = env::args().skip(1);
    while let Some(arg) = args.next() {
        let mut value = || {
            args.next()
                .ok_or_else(|| format!("{} needs an argument", arg))
        };
        match arg.as_str() {
            "-h" | "--help" => {
                println!("{}", USAGE);
                return Ok(None);
            }
            "--programs" => opts.programs = PathBuf::from(value()?),
            "--staging" => opts.staging = PathBuf::from(value()?),
            "--repo" => opts.repo = PathBuf::from(value()?),
            "--key" => opts.key = PathBuf::from(value()?),
            "--repo-name" => opts.repo_name = value()?,
            _ => return Err(format!("unknown option {}", arg)),
        }
    }

    Ok(Some(opts))
}

fn publish(opts: &Options) -> Result<usize, String> {
    let key = load_key(&opts.key)?;

    let declared = manifest::scan(&opts.programs)?;
    let packaged: Vec<_> = declared.into_iter().filter(|m| !m.shipped).collect();
    if packaged.is_empty() {
        return Err(format!(
            "no program under {} declares itself packaged (shipped = false in pkg.toml)",
            opts.programs.display()
        ));
    }

    fs::create_dir_all(opts.repo.join("p")).map_err(|e| e.to_string())?;
    fs::create_dir_all(opts.repo.join("icons")).map_err(|e| e.to_string())?;

    let mut packages = Vec::new();
    for entry in &packaged {
        packages.push(build_one(opts, entry)?);
    }

    // The serial only ever climbs, and a client refuses to move backwards, so
    // it is read from whatever is already published rather than counted from
    // the packages.
    let serial = previous_serial(&opts.repo) + 1;
    let index = Index {
        repo: opts.repo_name.clone(),
        serial,
        generated: timestamp(),
        packages,
    };

    let rendered = index.render();
    let signature = key.sign(rendered.as_bytes());

    fs::write(opts.repo.join("index"), rendered.as_bytes()).map_err(|e| e.to_string())?;
    fs::write(opts.repo.join("index.sig"), signature.to_bytes()).map_err(|e| e.to_string())?;

    println!("index serial {}", serial);
    Ok(index.packages.len())
}

fn build_one(opts: &Options, entry: &manifest::Manifest) -> Result<Package, String> {
    let binary = opts.staging.join("bin").join(&entry.name);
    if !binary.exists() {
        return Err(format!(
            "{}: {} is missing; run `make packages` so the build stages it",
            entry.name,
            binary.display()
        ));
    }

    let mut installs = vec![format!("bin/{}", entry.name)];
    let mut contents = vec![pack::Item {
        path: format!("bin/{}", entry.name),
        source: binary,
        mode: 0o755,
    }];

    let icon_name = format!("icons/{}.svg", entry.name);
    let icon = match &entry.icon {
        Some(relative) => {
            let source = entry.directory.join(relative);
            if !source.exists() {
                return Err(format!(
                    "{}: icon {} is missing",
                    entry.name,
                    source.display()
                ));
            }
            // The icon ships inside the package so an installed program has
            // one, and is copied into the repository as well so the catalogue
            // can show it without downloading the package.
            fs::copy(&source, opts.repo.join(&icon_name)).map_err(|e| e.to_string())?;
            installs.push(format!("share/icons/{}.svg", entry.name));
            contents.push(pack::Item {
                path: format!("share/icons/{}.svg", entry.name),
                source,
                mode: 0o644,
            });
            Some(icon_name)
        }
        None => None,
    };

    let file = format!("p/{}-{}.tar.gz", entry.name, entry.version);
    let archive = opts.repo.join(&file);
    pack::write_archive(&archive, &contents)?;

    let (size, sha256) = measure(&archive)?;
    println!("  {} {} ({} bytes)", entry.name, entry.version, size);

    Ok(Package {
        name: entry.name.clone(),
        version: entry.version.clone(),
        summary: entry.summary.clone(),
        category: entry.category.clone(),
        size,
        sha256,
        file,
        icon,
        installs,
    })
}

fn measure(path: &Path) -> Result<(u64, String), String> {
    use sha2::{Digest, Sha256};

    let mut file = File::open(path).map_err(|e| format!("{}: {}", path.display(), e))?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 64 * 1024];
    let mut size = 0u64;

    loop {
        let n = file.read(&mut buf).map_err(|e| e.to_string())?;
        if n == 0 {
            break;
        }
        size += n as u64;
        hasher.update(&buf[..n]);
    }

    Ok((size, hex(&hasher.finalize())))
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{:02x}", b)).collect()
}

/// The signing key is the raw 32-byte ed25519 seed.
fn load_key(path: &Path) -> Result<SigningKey, String> {
    let bytes = fs::read(path).map_err(|e| {
        format!(
            "{}: {} (generate one with: openssl genpkey -algorithm ed25519 -outform DER | tail -c 32 > {})",
            path.display(),
            e,
            path.display()
        )
    })?;
    let seed: [u8; 32] = bytes.as_slice().try_into().map_err(|_| {
        format!(
            "{}: expected 32 bytes, found {}",
            path.display(),
            bytes.len()
        )
    })?;
    Ok(SigningKey::from_bytes(&seed))
}

/// The serial of whatever is already published, or 0 when nothing is.
///
/// A malformed or absent index is treated as 0 rather than as an error: it is
/// the state of a repository being created for the first time.
fn previous_serial(repo: &Path) -> u64 {
    fs::read_to_string(repo.join("index"))
        .ok()
        .and_then(|text| Index::parse(&text).ok())
        .map(|index| index.serial)
        .unwrap_or(0)
}

/// An ISO 8601 UTC timestamp. Informational only: nothing verifies against it,
/// and the serial is what orders two indexes.
fn timestamp() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};

    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);

    let days = secs / 86_400;
    let (year, month, day) = civil_from_days(days as i64);
    let rest = secs % 86_400;
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z",
        year,
        month,
        day,
        rest / 3600,
        (rest % 3600) / 60,
        rest % 60
    )
}

/// Howard Hinnant's `civil_from_days`, the standard shift-to-March algorithm.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (if m <= 2 { y + 1 } else { y }, m as u32, d as u32)
}
