//! TLS: the crypto provider, the trust anchors, and the clock they depend on.

use crate::Error;
use edos_lib::{
    net,
    time::{self, CLOCK_PLAUSIBLE_FROM_NANOS, ClockTime, NTP_PORT},
};
use rustls::{ClientConfig, RootCertStore, pki_types::ServerName};
use std::sync::{
    Arc, OnceLock,
    atomic::{AtomicBool, Ordering},
};

/// Randomness for the handshake, from `SYS_GETRANDOM`.
///
/// [`edos_lib::getrandom`] cannot serve here: it discards the syscall's error
/// and leaves the buffer as the caller passed it, so a failure would hand
/// rustls a zeroed key share that looks exactly like success.
fn edos_rng(buf: &mut [u8]) -> Result<(), getrandom::Error> {
    edos_lib::try_getrandom(buf).map_err(|_| getrandom::Error::UNSUPPORTED)
}

getrandom::register_custom_getrandom!(edos_rng);

/// Where the clock is corrected from when it is unset. Overridable through
/// `/etc/ntp`, since a machine with no route to the pool needs somewhere else
/// to ask.
const DEFAULT_NTP_SERVER: &str = "pool.ntp.org";
const NTP_TIMEOUT_MS: u64 = 3_000;

/// The shared client configuration: RustCrypto primitives over the compiled-in
/// root store.
///
/// Built once. Loading the trust anchors is the expensive part, and every
/// connection in a process wants the same set.
pub fn client_config() -> Result<Arc<ClientConfig>, Error> {
    static CONFIG: OnceLock<Result<Arc<ClientConfig>, String>> = OnceLock::new();

    CONFIG
        .get_or_init(|| {
            let mut roots = RootCertStore::empty();
            roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());

            let config =
                ClientConfig::builder_with_provider(Arc::new(rustls_rustcrypto::provider()))
                    .with_safe_default_protocol_versions()
                    .map_err(|e| format!("no usable TLS protocol version: {}", e))?
                    .with_root_certificates(roots)
                    .with_no_client_auth();

            Ok(Arc::new(config))
        })
        .clone()
        .map_err(Error::Tls)
}

/// The server name for SNI and certificate matching.
///
/// Cloudflare, which fronts the package repository, requires SNI and answers a
/// connection without it with a certificate for the wrong site.
pub fn server_name(host: &str) -> Result<ServerName<'static>, Error> {
    ServerName::try_from(host.to_string())
        .map_err(|_| Error::Tls(format!("not a valid server name: {}", host)))
}

/// Make sure the wall clock can support certificate validation, correcting it
/// over SNTP if it is unset and `auto_sync` allows.
///
/// Certificate validity is checked against this clock, so a clock left at the
/// epoch rejects every certificate that exists, reporting a certificate problem
/// for what is really a time problem. Catching it here is what lets the error
/// name the actual cause.
pub fn ensure_clock_usable(auto_sync: bool) -> Result<(), Error> {
    static SYNC_ATTEMPTED: AtomicBool = AtomicBool::new(false);

    if clock_is_plausible() {
        return Ok(());
    }
    if auto_sync && !SYNC_ATTEMPTED.swap(true, Ordering::SeqCst) {
        // A failure here is not reported on its own: if the clock ends up
        // usable the sync worked, and if it does not the message below says so
        // in terms of the thing the caller actually cares about.
        let _ = sync_clock();
        if clock_is_plausible() {
            return Ok(());
        }
    }

    Err(Error::Clock(format!(
        "system clock reads {}, so no certificate can be valid; run `sntp -s`",
        now_text()
    )))
}

fn clock_is_plausible() -> bool {
    time::clock_gettime_nanos().is_some_and(|nanos| nanos >= CLOCK_PLAUSIBLE_FROM_NANOS)
}

fn sync_clock() -> Result<(), String> {
    let server =
        edos_lib::config::read("/etc/ntp").unwrap_or_else(|| DEFAULT_NTP_SERVER.to_string());
    let ip = net::resolve_host(&server).ok_or_else(|| format!("cannot resolve {}", server))?;
    let sample = time::sntp_query(ip, NTP_PORT, NTP_TIMEOUT_MS)?;
    time::sntp_step_clock(&sample)
}

/// The wall clock as a human-readable UTC timestamp, for error messages.
pub fn now_text() -> String {
    let Some(nanos) = time::clock_gettime_nanos() else {
        return "an unreadable clock".to_string();
    };
    let t = ClockTime::from_unix_nanos(nanos);
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z",
        t.year, t.month, t.day, t.hour, t.minute, t.second
    )
}

/// Turn a rustls error into one that names the clock when the clock is what
/// the peer's certificate was judged against.
pub fn explain(err: rustls::Error) -> Error {
    let text = err.to_string();
    let time_related = matches!(
        err,
        rustls::Error::InvalidCertificate(
            rustls::CertificateError::Expired | rustls::CertificateError::NotValidYet
        )
    );
    if time_related {
        Error::Tls(format!("{} (the system clock reads {})", text, now_text()))
    } else {
        Error::Tls(text)
    }
}
