//! What this machine is willing to believe.

use crate::{Error, Result};
use ed25519_dalek::{Signature, Verifier, VerifyingKey};

/// The repository's ed25519 public key.
///
/// Compiled in rather than read from `/etc`, because a key on a writable
/// filesystem is only as trustworthy as write access to that filesystem, and
/// the whole point of signing is to not have to trust the delivery path. The
/// cost is that rotating it means rebuilding `grab`, which is the honest trade
/// at this size.
pub const REPO_KEY: [u8; 32] = [
    0x7a, 0x33, 0x32, 0xb8, 0xa9, 0xa7, 0x4b, 0x55, 0xc9, 0xb6, 0x4a, 0x42, 0x3b, 0xd9, 0xcb, 0xd0,
    0xb2, 0x6f, 0x9d, 0x10, 0x8b, 0xf5, 0x2f, 0xb0, 0x4c, 0x00, 0x97, 0xbb, 0xbd, 0x64, 0x6d, 0xda,
];

/// Check a detached signature over `message`.
pub fn verify(message: &[u8], signature: &[u8]) -> Result<()> {
    let key = VerifyingKey::from_bytes(&REPO_KEY)
        .map_err(|e| Error::Untrusted(format!("the built-in repository key is unusable: {}", e)))?;

    let bytes: [u8; 64] = signature.try_into().map_err(|_| {
        Error::Untrusted(format!(
            "the signature is {} bytes, not 64",
            signature.len()
        ))
    })?;

    key.verify(message, &Signature::from_bytes(&bytes))
        .map_err(|_| {
            Error::Untrusted("the signature does not match the repository key".to_string())
        })
}

/// Check that `data` hashes to `expected`, written as lowercase hex.
pub fn verify_sha256(data: &[u8], expected: &str) -> Result<()> {
    use sha2::{Digest, Sha256};

    let actual = Sha256::digest(data);
    let actual: String = actual.iter().map(|b| format!("{:02x}", b)).collect();

    if actual.eq_ignore_ascii_case(expected) {
        Ok(())
    } else {
        Err(Error::Untrusted(format!(
            "the archive hashes to {} but the index says {}",
            actual, expected
        )))
    }
}
