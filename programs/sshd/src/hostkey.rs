//! The server's `ssh-ed25519` host key: how it is stored, published and used
//! to sign an exchange hash.

use std::fs;
use std::io;
use std::path::Path;

use ed25519_dalek::{Signer, SigningKey};
use sha2::{Digest, Sha256};

use crate::wire::{Writer, base64};

pub struct HostKey {
    signing: SigningKey,
}

impl HostKey {
    /// Load the key at `path`, generating one on first run.
    ///
    /// The file holds the raw 32-byte seed, not an OpenSSH private key: nothing
    /// reads it but this program, and the OpenSSH container would need a
    /// bcrypt KDF and a cipher that no other part of the server uses.
    ///
    /// There is no `chmod` on this system, so the file is created with whatever
    /// permissions the filesystem gives it. It is the reason the key belongs
    /// somewhere only the server reads.
    pub fn load_or_create(path: &str) -> io::Result<Self> {
        if let Ok(seed) = fs::read(path) {
            if seed.len() == 32 {
                let mut bytes = [0u8; 32];
                bytes.copy_from_slice(&seed);
                return Ok(Self {
                    signing: SigningKey::from_bytes(&bytes),
                });
            }
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("{path} is not a 32-byte ed25519 seed"),
            ));
        }

        let mut seed = [0u8; 32];
        edos_lib::getrandom(&mut seed);
        if seed == [0u8; 32] {
            return Err(io::Error::other("getrandom returned no entropy"));
        }
        if let Some(dir) = Path::new(path).parent() {
            let _ = fs::create_dir_all(dir);
        }
        fs::write(path, seed)?;
        Ok(Self {
            signing: SigningKey::from_bytes(&seed),
        })
    }

    /// The public key blob: `string "ssh-ed25519" || string key`. This is both
    /// `K_S` in the exchange hash and what an `authorized_keys` line encodes.
    pub fn public_blob(&self) -> Vec<u8> {
        let mut w = Writer::new();
        w.text("ssh-ed25519");
        w.string(self.signing.verifying_key().as_bytes());
        w.finish()
    }

    /// A signature blob: `string "ssh-ed25519" || string signature`.
    pub fn sign(&self, data: &[u8]) -> Vec<u8> {
        let sig = self.signing.sign(data);
        let mut w = Writer::new();
        w.text("ssh-ed25519");
        w.string(&sig.to_bytes());
        w.finish()
    }

    /// The `SHA256:...` fingerprint OpenSSH prints, so the first connection can
    /// be checked against the server's own log line.
    pub fn fingerprint(&self) -> String {
        let digest = Sha256::digest(self.public_blob());
        format!("SHA256:{}", base64(&digest).trim_end_matches('='))
    }

    /// The `authorized_keys`-style one-liner for this key.
    pub fn public_line(&self) -> String {
        format!("ssh-ed25519 {}", base64(&self.public_blob()))
    }
}
