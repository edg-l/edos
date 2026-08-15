//! Talking to the repository, and refusing to believe it without proof.

use crate::{Error, Progress, Result, trust};
use edos_http::{Options, fetch, get};
use grab_index::{Index, Package};
use std::fs;

/// Where a verified index is kept between runs.
pub const CACHE: &str = "/var/cache/grab";

/// Fetch the index and its signature, verify both, and cache the result.
///
/// Nothing here trusts the transport. TLS says the bytes came from the server
/// unmodified; the signature says the server is publishing what the repository
/// owner signed, which is the property that survives a compromised CDN.
pub fn fetch_index(base: &str) -> Result<Index> {
    let opts = Options {
        // An index is small. Refusing a large one costs nothing and removes
        // the case where a hostile server streams until the machine dies.
        max_body: 8 * 1024 * 1024,
        ..Options::default()
    };

    let index_bytes = get_ok(&format!("{}/index", base), &opts)?;
    let signature = get_ok(&format!("{}/index.sig", base), &opts)?;

    trust::verify(&index_bytes, &signature)?;

    let text = String::from_utf8(index_bytes.clone())
        .map_err(|_| Error::Malformed("the index is not UTF-8".to_string()))?;
    let index = Index::parse(&text).map_err(Error::Malformed)?;

    // A signed *old* index is a valid signature over a lie by omission: it
    // hides that a newer version was published. The serial is what makes that
    // detectable, so a decrease is refused even though the signature is good.
    if let Some(cached) = cached_index()
        && index.serial < cached.serial
    {
        return Err(Error::Untrusted(format!(
            "the repository offers serial {} but this machine already has {}",
            index.serial, cached.serial
        )));
    }

    let _ = fs::create_dir_all(CACHE);
    write_cache("index", &index_bytes)?;
    write_cache("index.sig", &signature)?;

    Ok(index)
}

/// The last verified index, if there is one.
///
/// The cached copy is re-verified on the way out. It sits on an ordinary
/// writable filesystem, so reading it back without checking the signature
/// would make local tampering as good as a valid publish.
pub fn cached_index() -> Option<Index> {
    let bytes = fs::read(format!("{}/index", CACHE)).ok()?;
    let signature = fs::read(format!("{}/index.sig", CACHE)).ok()?;
    trust::verify(&bytes, &signature).ok()?;
    Index::parse(&String::from_utf8(bytes).ok()?).ok()
}

/// Download a package and check it against the index before handing it back.
pub fn fetch_package(
    base: &str,
    package: &Package,
    progress: &mut dyn Progress,
) -> Result<Vec<u8>> {
    let opts = Options {
        // The index declares the size, so anything beyond it is already wrong.
        // The slack covers nothing but a mis-declared length; the hash below is
        // what actually decides.
        max_body: package.size + 4096,
        // A package is already gzip content: asking for the transfer to be
        // gzipped as well buys nothing, and the size the index declares is the
        // size on the wire.
        accept_gzip: false,
        ..Options::default()
    };

    let url = format!("{}/{}", base, package.file);
    let mut body = Vec::with_capacity(package.size as usize);
    let head = fetch(&url, &opts, &mut body, &mut |done, total| {
        progress.transfer(done, total.or(Some(package.size)))
    })?;

    if !head.is_success() {
        return Err(Error::NotFound(format!(
            "{}: {} {}",
            url, head.status, head.reason
        )));
    }
    if body.len() as u64 != package.size {
        return Err(Error::Untrusted(format!(
            "{} is {} bytes but the index says {}",
            package.file,
            body.len(),
            package.size
        )));
    }
    trust::verify_sha256(&body, &package.sha256)?;

    Ok(body)
}

fn get_ok(url: &str, opts: &Options) -> Result<Vec<u8>> {
    let response = get(url, opts)?;
    if !response.head.is_success() {
        return Err(Error::NotFound(format!(
            "{}: {} {}",
            url, response.head.status, response.head.reason
        )));
    }
    Ok(response.body)
}

fn write_cache(name: &str, bytes: &[u8]) -> Result<()> {
    fs::write(format!("{}/{}", CACHE, name), bytes)
        .map_err(|e| Error::Io(format!("{}/{}: {}", CACHE, name, e)))
}
