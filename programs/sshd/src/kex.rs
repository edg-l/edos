//! Key exchange: `curve25519-sha256` with an `ssh-ed25519` host key,
//! RFC 4253 §7-8 and RFC 8731.
//!
//! The exchange hash `H` is the whole security argument. It commits to both
//! version strings, both `KEXINIT` payloads, the host key, both ephemeral
//! public keys and the shared secret, and the host key signs it — so a peer
//! that saw a different algorithm list or a different host key computes a
//! different `H` and the signature does not verify.

use sha2::{Digest, Sha256};
use x25519_dalek::{PublicKey, StaticSecret};

use crate::error::{Error, Result, protocol_err};
use crate::hostkey::HostKey;
use crate::packet::{
    self, DISCONNECT_KEY_EXCHANGE_FAILED, MSG_KEX_ECDH_INIT, MSG_KEX_ECDH_REPLY, MSG_KEXINIT,
    MSG_NEWKEYS, Transport,
};
use crate::wire::{Reader, Writer};

/// `curve25519-sha256@libssh.org` is the same exchange under the name it had
/// before RFC 8731, and clients that predate the RFC still ask for it.
const KEX_ALGS: &[&str] = &["curve25519-sha256", "curve25519-sha256@libssh.org"];
const HOSTKEY_ALGS: &[&str] = &["ssh-ed25519"];
const CIPHERS: &[&str] = &["aes128-ctr"];
const MACS: &[&str] = &["hmac-sha2-256"];
const COMPRESSION: &[&str] = &["none"];

/// The identification string this server sends. RFC 4253 §4.2 fixes the shape
/// up to the software version, and the comment field is left off.
pub const VERSION: &str = "SSH-2.0-EDOS_0.1";

/// What the version exchange produced, kept for every later exchange hash.
pub struct Versions {
    pub client: String,
    pub server: String,
}

/// Send our identification and read the client's.
pub fn exchange_versions(t: &Transport) -> Result<Versions> {
    t.write_line(VERSION)?;
    loop {
        let line = t.read_line()?;
        if line.starts_with("SSH-") {
            if !line.starts_with("SSH-2.0-") && !line.starts_with("SSH-1.99-") {
                return protocol_err!("unsupported protocol version {line:?}");
            }
            return Ok(Versions {
                client: line,
                server: VERSION.to_string(),
            });
        }
        // Anything before the identification is a banner and is ignored.
    }
}

/// Our `KEXINIT` payload, built fresh each time because of the cookie.
fn server_kexinit() -> Vec<u8> {
    let mut cookie = [0u8; 16];
    edos_lib::getrandom(&mut cookie);

    let mut w = Writer::msg(MSG_KEXINIT);
    w.raw(&cookie);
    w.name_list(KEX_ALGS);
    w.name_list(HOSTKEY_ALGS);
    w.name_list(CIPHERS); // client to server
    w.name_list(CIPHERS); // server to client
    w.name_list(MACS);
    w.name_list(MACS);
    w.name_list(COMPRESSION);
    w.name_list(COMPRESSION);
    w.name_list(&[]); // languages
    w.name_list(&[]);
    w.bool(false); // no guessed packet follows
    w.u32(0); // reserved
    w.finish()
}

/// Pick the first client preference this server also implements, which is the
/// rule RFC 4253 §7.1 states for every list.
fn negotiate<'a>(client: &[&'a str], ours: &[&str], what: &str) -> Result<&'a str> {
    client
        .iter()
        .find(|c| ours.contains(c))
        .copied()
        .ok_or_else(|| {
            Error::Protocol(format!(
                "no shared {what}: client offered {}, this server has {}",
                client.join(","),
                ours.join(",")
            ))
        })
}

/// Check the client's `KEXINIT` against what this server implements, and report
/// whether a guessed `KEX_ECDH_INIT` follows that this server must discard.
///
/// RFC 4253 §7.1: a client may send its first key-exchange packet immediately
/// after `KEXINIT`, betting on which algorithm will be chosen. The guess counts
/// only if its first key-exchange *and* host-key preferences are the ones that
/// win; otherwise the packet belongs to an algorithm that was not chosen, and
/// reading it as the real one gets a key of the wrong length at best.
fn check_algorithms(client_kexinit: &[u8]) -> Result<bool> {
    let mut r = Reader::new(client_kexinit);
    if r.byte()? != MSG_KEXINIT {
        return protocol_err!("expected KEXINIT");
    }
    r.skip(16)?; // cookie
    let client_kex = r.name_list()?;
    let client_hostkey = r.name_list()?;
    negotiate(&client_kex, KEX_ALGS, "key exchange")?;
    negotiate(&client_hostkey, HOSTKEY_ALGS, "host key algorithm")?;
    negotiate(&r.name_list()?, CIPHERS, "client-to-server cipher")?;
    negotiate(&r.name_list()?, CIPHERS, "server-to-client cipher")?;
    negotiate(&r.name_list()?, MACS, "client-to-server MAC")?;
    negotiate(&r.name_list()?, MACS, "server-to-client MAC")?;
    negotiate(&r.name_list()?, COMPRESSION, "client-to-server compression")?;
    negotiate(&r.name_list()?, COMPRESSION, "server-to-client compression")?;
    r.name_list()?; // languages, client to server
    r.name_list()?; // languages, server to client
    let guessed = r.bool()?;

    let guessed_right = client_kex.first().is_some_and(|k| KEX_ALGS.contains(k))
        && client_hostkey
            .first()
            .is_some_and(|h| HOSTKEY_ALGS.contains(h));
    Ok(guessed && !guessed_right)
}

/// Run one key exchange and install the keys it produces.
///
/// `client_kexinit` is already in hand when a rekey is what brought us here;
/// on the first exchange it is still to be read.
pub fn run(
    t: &mut Transport,
    host: &HostKey,
    versions: &Versions,
    client_kexinit: Option<Vec<u8>>,
) -> Result<()> {
    let server_kexinit_payload = server_kexinit();
    t.send(&server_kexinit_payload)?;

    let client_kexinit = match client_kexinit {
        Some(p) => p,
        None => {
            let p = t.recv()?;
            if p.first() != Some(&MSG_KEXINIT) {
                return protocol_err!("expected KEXINIT, got message {:?}", p.first());
            }
            p
        }
    };

    let discard_guess = match check_algorithms(&client_kexinit) {
        Ok(discard) => discard,
        Err(e) => {
            t.disconnect(DISCONNECT_KEY_EXCHANGE_FAILED, &e.to_string());
            return Err(e);
        }
    };
    if discard_guess {
        // A guess for an algorithm that did not win. It is a whole packet and
        // has to go somewhere, or every field after it reads one packet late.
        t.recv()?;
    }

    let payload = t.recv()?;
    let mut r = Reader::new(&payload);
    if r.byte()? != MSG_KEX_ECDH_INIT {
        return protocol_err!("expected KEX_ECDH_INIT");
    }
    let client_public = r.string()?;
    let client_public: [u8; 32] = client_public
        .try_into()
        .map_err(|_| Error::Protocol("client curve25519 key is not 32 bytes".into()))?;
    let client_public = PublicKey::from(client_public);

    let mut secret_bytes = [0u8; 32];
    edos_lib::getrandom(&mut secret_bytes);
    if secret_bytes == [0u8; 32] {
        return Err(Error::Local("getrandom returned no entropy".into()));
    }
    let secret = StaticSecret::from(secret_bytes);
    let server_public = PublicKey::from(&secret);
    let shared = secret.diffie_hellman(&client_public);

    // An all-zero shared secret means a small-order client key, which cannot
    // authenticate anything. RFC 8731 §3 requires aborting rather than
    // continuing with a secret the peer chose.
    if !shared.was_contributory() {
        t.disconnect(DISCONNECT_KEY_EXCHANGE_FAILED, "degenerate curve25519 key");
        return protocol_err!("client sent a small-order curve25519 key");
    }

    let host_blob = host.public_blob();
    let mut h = Writer::new();
    h.text(&versions.client);
    h.text(&versions.server);
    h.string(&client_kexinit);
    h.string(&server_kexinit_payload);
    h.string(&host_blob);
    h.string(client_public.as_bytes());
    h.string(server_public.as_bytes());
    h.mpint(shared.as_bytes());
    let exchange_hash: [u8; 32] = Sha256::digest(h.finish()).into();

    let mut reply = Writer::msg(MSG_KEX_ECDH_REPLY);
    reply.string(&host_blob);
    reply.string(server_public.as_bytes());
    reply.string(&host.sign(&exchange_hash));
    t.send(&reply.finish())?;

    let session_id = *t.session_id.get_or_insert(exchange_hash);

    // Outgoing keys take effect the moment our NEWKEYS is on the wire; incoming
    // ones only once the client's arrives, and that one is still under the old
    // keys. Installing either early loses the connection.
    t.send(&[MSG_NEWKEYS])?;
    let keys = Keys::derive(shared.as_bytes(), &exchange_hash, &session_id);
    t.set_tx_keys(&keys.server_key, &keys.server_iv, &keys.server_mac);

    let payload = t.recv()?;
    if payload.first() != Some(&MSG_NEWKEYS) {
        return protocol_err!("expected NEWKEYS, got message {:?}", payload.first());
    }
    t.set_rx_keys(&keys.client_key, &keys.client_iv, &keys.client_mac);
    Ok(())
}

/// The six keys RFC 4253 §7.2 derives, named by the direction that uses them.
struct Keys {
    client_iv: [u8; 16],
    server_iv: [u8; 16],
    client_key: [u8; 16],
    server_key: [u8; 16],
    client_mac: [u8; 32],
    server_mac: [u8; 32],
}

impl Keys {
    fn derive(shared: &[u8; 32], exchange_hash: &[u8; 32], session_id: &[u8; 32]) -> Self {
        // K enters every derivation as an mpint, the same encoding the exchange
        // hash used; hashing the raw 32 bytes instead is the classic way to
        // interoperate with nothing.
        let mut k = Writer::new();
        k.mpint(shared);
        let k = k.finish();

        let mut out = Self {
            client_iv: [0; 16],
            server_iv: [0; 16],
            client_key: [0; 16],
            server_key: [0; 16],
            client_mac: [0; 32],
            server_mac: [0; 32],
        };
        expand(&k, exchange_hash, b'A', session_id, &mut out.client_iv);
        expand(&k, exchange_hash, b'B', session_id, &mut out.server_iv);
        expand(&k, exchange_hash, b'C', session_id, &mut out.client_key);
        expand(&k, exchange_hash, b'D', session_id, &mut out.server_key);
        expand(&k, exchange_hash, b'E', session_id, &mut out.client_mac);
        expand(&k, exchange_hash, b'F', session_id, &mut out.server_mac);
        out
    }
}

/// `K1 = HASH(K || H || X || session_id)`, then `Kn+1 = HASH(K || H || K1..Kn)`
/// until enough bytes exist. Nothing here needs more than one round at
/// SHA-256's 32 bytes, but the loop is the specified construction.
fn expand(k: &[u8], exchange_hash: &[u8; 32], letter: u8, session_id: &[u8; 32], out: &mut [u8]) {
    let mut material = Vec::new();
    while material.len() < out.len() {
        let mut h = Sha256::new();
        h.update(k);
        h.update(exchange_hash);
        if material.is_empty() {
            h.update([letter]);
            h.update(session_id);
        } else {
            h.update(&material);
        }
        material.extend_from_slice(&h.finalize());
    }
    out.copy_from_slice(&material[..out.len()]);
}

/// Whether a payload is one of the transport-layer messages that may arrive at
/// any time, which during a session means the client wants to rekey.
pub fn is_kexinit(payload: &[u8]) -> bool {
    payload.first() == Some(&packet::MSG_KEXINIT)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_expansion_chains_past_one_hash() {
        let k = [1u8; 32];
        let h = [2u8; 32];
        let sid = [3u8; 32];
        let mut short = [0u8; 32];
        let mut long = [0u8; 64];
        expand(&k, &h, b'A', &sid, &mut short);
        expand(&k, &h, b'A', &sid, &mut long);
        // The first 32 bytes are the same hash either way; the rest continues it.
        assert_eq!(short, long[..32]);
        assert_ne!(&long[32..], &[0u8; 32]);
    }

    #[test]
    fn negotiation_follows_client_preference() {
        assert_eq!(
            negotiate(&["aes256-ctr", "aes128-ctr"], CIPHERS, "cipher").unwrap(),
            "aes128-ctr"
        );
        assert!(negotiate(&["3des-cbc"], CIPHERS, "cipher").is_err());
    }
}
