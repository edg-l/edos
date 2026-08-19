//! An HTTP/1.1 client with TLS, shared by `wget`, `http` and `grab`.
//!
//! This is a separate crate rather than a module of `edos_lib` because every
//! program in the tree links `edos_lib`, and rustls has no business inside
//! `true` and `yes`.

use flate2::write::GzDecoder;
use std::{
    fmt,
    io::{self, BufReader, Read, Write},
    net::{TcpStream, ToSocketAddrs},
    sync::Mutex,
    time::Duration,
};

pub mod tls;
pub mod url;

use url::{Scheme, Url};

/// Longest status or header line accepted, so a hostile server cannot make the
/// client allocate without bound before a single byte of body arrives.
const MAX_LINE: usize = 16 * 1024;
/// Ceiling on the number of header lines, for the same reason.
const MAX_HEADERS: usize = 128;
const COPY_BUF: usize = 32 * 1024;

#[derive(Debug)]
pub enum Error {
    Url(String),
    Connect { addr: String, source: io::Error },
    Io(io::Error),
    Tls(String),
    Clock(String),
    Protocol(String),
    TooLarge { limit: u64 },
    TooManyRedirects(u8),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Url(what) => write!(f, "bad URL: {}", what),
            Error::Connect { addr, source } => write!(f, "connect to {}: {}", addr, source),
            Error::Io(e) => write!(f, "{}", e),
            Error::Tls(what) => write!(f, "TLS: {}", what),
            Error::Clock(what) => write!(f, "{}", what),
            Error::Protocol(what) => write!(f, "bad response: {}", what),
            Error::TooLarge { limit } => {
                write!(f, "response is larger than the {} byte limit", limit)
            }
            Error::TooManyRedirects(n) => write!(f, "more than {} redirects", n),
        }
    }
}

impl std::error::Error for Error {}

impl From<io::Error> for Error {
    fn from(e: io::Error) -> Self {
        Error::Io(e)
    }
}

pub struct Options {
    /// Refuse a body larger than this, both by the declared length and by what
    /// actually arrives.
    pub max_body: u64,
    pub max_redirects: u8,
    /// How long to wait for a connection before giving up on an address.
    ///
    /// The deadline is the caller's rather than the kernel's: a blocking
    /// `connect` waits its own five seconds for a host that is not answering
    /// and cannot be told a shorter number, which is the whole cost of an
    /// unreachable repository. The default matches that wait, so a program has
    /// to ask to be more impatient than the system is.
    pub connect_timeout: Duration,
    pub user_agent: String,
    /// Correct an unset clock over SNTP before a TLS handshake. See
    /// [`tls::ensure_clock_usable`].
    pub fix_clock: bool,
    /// Keep a connection open after a response and use it for the next request
    /// to the same host.
    ///
    /// A page is a document plus its stylesheets and its images, and without
    /// this each of those pays a TCP handshake, a TLS handshake and a
    /// certificate verification of its own, all of it in software here.
    pub keep_alive: bool,
    /// Ask for the body gzipped, and inflate what comes back.
    ///
    /// Worth it for anything the server does not already store compressed: a
    /// documentation page's stylesheet is around 100 KB of text, and this
    /// machine's TLS is software, so the bytes it does not have to receive and
    /// decrypt cost more than the inflate does. A download of something
    /// already compressed -- a package, an image -- gains nothing, which is
    /// what `grab` turns this off for.
    pub accept_gzip: bool,
    pub extra_headers: Vec<(String, String)>,
}

impl Default for Options {
    fn default() -> Self {
        Options {
            max_body: 256 * 1024 * 1024,
            max_redirects: 5,
            connect_timeout: Duration::from_secs(5),
            user_agent: concat!("grab/", env!("CARGO_PKG_VERSION"), " (EDOS)").to_string(),
            fix_clock: true,
            keep_alive: true,
            accept_gzip: true,
            extra_headers: Vec::new(),
        }
    }
}

/// A response's status and headers, without its body.
pub struct Head {
    pub status: u16,
    pub reason: String,
    pub headers: Vec<(String, String)>,
    /// Where the response came from, after any redirects.
    pub final_url: String,
    /// The request as it went on the wire, for `http -v`.
    pub sent: String,
    /// The response header block as it arrived, for `http -i`.
    pub raw_headers: String,
}

impl Head {
    /// The first value for `name`, matched case-insensitively as RFC 9110 §5.1
    /// requires.
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }

    pub fn is_success(&self) -> bool {
        (200..300).contains(&self.status)
    }
}

pub struct Response {
    pub head: Head,
    pub body: Vec<u8>,
}

/// Fetch `url` into memory.
pub fn get(url: &str, opts: &Options) -> Result<Response, Error> {
    let mut body = Vec::new();
    let head = fetch(url, opts, &mut body, &mut |_, _| {})?;
    Ok(Response { head, body })
}

/// Fetch `url`, writing the body to `sink` as it arrives.
///
/// `progress` is called with the bytes written so far and the total when the
/// server declared one. Streaming rather than buffering is what keeps a large
/// package off the heap on a machine that may not have room for it.
pub fn fetch(
    url: &str,
    opts: &Options,
    sink: &mut dyn Write,
    progress: &mut dyn FnMut(u64, Option<u64>),
) -> Result<Head, Error> {
    let mut target = Url::parse(url)?;

    for _ in 0..=opts.max_redirects {
        let (mut reader, head, http11) = request_once(&target, opts)?;

        if is_redirect(head.status)
            && let Some(location) = head.header("Location")
        {
            let next = target.join(location)?;
            // The body of a redirect is of no interest, but reading it is
            // what leaves the connection at the start of the next response
            // rather than in the middle of this one. A body that does not
            // frame its own end is not worth draining, since draining it
            // *is* reading to the close.
            let drained = read_body(&mut reader, &head, opts, &mut io::sink(), &mut |_, _| {})
                .unwrap_or(false);
            if opts.keep_alive && reusable(&head, http11, drained) {
                put_idle(pool_key(&target), reader);
            }
            target = next;
            continue;
        }

        // A gzipped body is inflated on the way to the sink rather than
        // buffered and inflated after: the caller asked for a stream, and a
        // response that is 100 KB on the wire and a megabyte after it should
        // not exist twice in memory to make that true.
        let definite = if opts.accept_gzip && head.header("Content-Encoding").is_some_and(is_gzip) {
            let mut decoder = GzDecoder::new(Limited {
                sink,
                written: 0,
                limit: opts.max_body,
            });
            let definite = read_body(&mut reader, &head, opts, &mut decoder, progress)?;
            decoder.finish()?;
            definite
        } else {
            read_body(&mut reader, &head, opts, sink, progress)?
        };
        if opts.keep_alive && reusable(&head, http11, definite) {
            put_idle(pool_key(&target), reader);
        }
        return Ok(head);
    }

    Err(Error::TooManyRedirects(opts.max_redirects))
}

/// Send one request and read its head, retrying once when a pooled connection
/// turns out to have been closed by the far end.
///
/// A server may close an idle connection at any time and the client cannot be
/// told, so a reused connection failing before the status line is ordinary
/// rather than an error. A *fresh* connection failing the same way is the
/// error it looks like, which is why only the reused case retries.
fn request_once(target: &Url, opts: &Options) -> Result<(BufReader<Conn>, Head, bool), Error> {
    let mut last = None;
    for attempt in 0..2 {
        let (mut reader, sent, reused) = send_request(target, opts)?;
        let read = read_status_line(&mut reader).and_then(|status| {
            let (headers, raw_headers) = read_headers(&mut reader)?;
            Ok((status, headers, raw_headers))
        });
        match read {
            Ok(((status, reason, http11), headers, raw_headers)) => {
                let head = Head {
                    status,
                    reason,
                    headers,
                    final_url: target.to_string(),
                    sent,
                    raw_headers,
                };
                return Ok((reader, head, http11));
            }
            Err(err) if reused && attempt == 0 => last = Some(err),
            Err(err) => return Err(err),
        }
    }
    Err(last.unwrap_or_else(|| Error::Protocol("no response".to_string())))
}

/// Whether a `Content-Encoding` names gzip. `x-gzip` is the older spelling and
/// still arrives from servers configured a decade ago.
fn is_gzip(value: &str) -> bool {
    let value = value.trim().to_ascii_lowercase();
    value == "gzip" || value == "x-gzip"
}

/// A sink that refuses to take more than `limit` bytes.
///
/// `max_body` bounds what arrives on the wire, and compressed bytes are not
/// what fills a machine: a few kilobytes of gzip expand to as much as the
/// sender likes. This is what makes the limit mean the same thing either way.
struct Limited<'a> {
    sink: &'a mut dyn Write,
    written: u64,
    limit: u64,
}

impl Write for Limited<'_> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.written += buf.len() as u64;
        if self.written > self.limit {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("the body inflates past {} bytes", self.limit),
            ));
        }
        self.sink.write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.sink.flush()
    }
}

/// Idle connections, by scheme and authority, waiting to carry another
/// request.
///
/// HTTP/1.1 leaves a connection open unless someone says otherwise, and this
/// client was saying otherwise on every request: a page is a document plus its
/// stylesheets and its images, and each of those was a TCP handshake, a TLS
/// handshake and a certificate verification of its own, all of it in software
/// on this machine.
///
/// One pool for the process rather than one per thread, and the difference is
/// not academic: `edos-web` loads each page on a thread of its own, so a
/// thread-local pool would be empty on every navigation and would hold a
/// connection only for the subresources of the page that opened it. A
/// connection is only ever in the pool *between* responses, never during one,
/// so a lock around the list is the whole of what sharing it costs.
///
/// A pooled connection may have been closed by the server since it was put
/// back, and there is no way to ask. That is what the retry in
/// [`request_once`] is for, and it is why a request is only ever retried when
/// it went out on a reused connection: a fresh one failing is a real failure.
struct Idle {
    key: String,
    reader: BufReader<Conn>,
}

static POOL: Mutex<Vec<Idle>> = Mutex::new(Vec::new());

/// How many idle connections to keep. A page reaches one or two hosts, and a
/// connection nobody claims is a socket the server is holding open too.
const MAX_IDLE: usize = 4;

fn pool_key(target: &Url) -> String {
    format!("{}//{}", target.scheme().as_str(), target.authority())
}

fn take_idle(key: &str) -> Option<BufReader<Conn>> {
    let mut pool = POOL.lock().ok()?;
    let at = pool.iter().position(|idle| idle.key == key)?;
    Some(pool.remove(at).reader)
}

fn put_idle(key: String, reader: BufReader<Conn>) {
    let Ok(mut pool) = POOL.lock() else {
        return;
    };
    if pool.len() >= MAX_IDLE {
        pool.remove(0);
    }
    pool.push(Idle { key, reader });
}

/// Whether the connection a response arrived on can carry another request.
///
/// Three things have to hold: the response framed its body definitely, so the
/// reader is positioned at the end of it and not somewhere inside it; neither
/// side asked to close; and the response is HTTP/1.1, since 1.0 closes by
/// default and a 1.0 server that means otherwise says so.
fn reusable(head: &Head, http11: bool, definite: bool) -> bool {
    let connection = head
        .header("Connection")
        .unwrap_or_default()
        .to_ascii_lowercase();
    definite && http11 && !connection.contains("close")
}

fn is_redirect(status: u16) -> bool {
    matches!(status, 301 | 302 | 303 | 307 | 308)
}

/// Connect to `authority`, giving up on an address after `timeout` instead of
/// waiting out the kernel's own handshake wait.
///
/// Resolution comes first, because a deadline is only meaningful against a
/// concrete address. Each address gets the full timeout, the way the blocking
/// `TcpStream::connect` gives each one a full attempt: a host that answers on
/// its second address is reachable, and a deadline shared out among them would
/// make how reachable it is depend on how many addresses it publishes.
fn connect(authority: &str, timeout: Duration) -> io::Result<TcpStream> {
    let mut last = None;
    for addr in authority.to_socket_addrs()? {
        match TcpStream::connect_timeout(&addr, timeout) {
            Ok(stream) => return Ok(stream),
            Err(e) => last = Some(e),
        }
    }
    Err(last.unwrap_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            "the name resolved to no usable address",
        )
    }))
}

/// Write the request on a connection, opening one if the pool has none for
/// this host.
///
/// The third part of the answer is whether the connection was reused, which is
/// what decides if a failure reading the response is worth retrying.
fn send_request(target: &Url, opts: &Options) -> Result<(BufReader<Conn>, String, bool), Error> {
    let request = request_text(target, opts);
    let key = pool_key(target);

    if opts.keep_alive
        && let Some(mut reader) = take_idle(&key)
    {
        // A write to a connection the far end has since closed may well
        // succeed -- the failure arrives as a reset or an empty read on
        // the way back -- so this is not where a stale connection is
        // caught. The retry is.
        let sent = reader
            .get_mut()
            .write_all(request.as_bytes())
            .and_then(|()| reader.get_mut().flush());
        if sent.is_ok() {
            return Ok((reader, request, true));
        }
    }

    let addr = target.authority();
    let tcp = connect(&addr, opts.connect_timeout).map_err(|source| Error::Connect {
        addr: addr.clone(),
        source,
    })?;

    let mut conn = match target.scheme() {
        Scheme::Http => Conn::Plain(tcp),
        Scheme::Https => {
            tls::ensure_clock_usable(opts.fix_clock)?;
            let config = tls::client_config()?;
            let name = tls::server_name(target.host())?;
            let client = rustls::ClientConnection::new(config, name).map_err(tls::explain)?;
            Conn::Tls(Box::new(rustls::StreamOwned::new(client, tcp)))
        }
    };

    conn.write_all(request.as_bytes())?;
    conn.flush()?;

    Ok((BufReader::new(conn), request, false))
}

/// The request as it goes on the wire.
fn request_text(target: &Url, opts: &Options) -> String {
    let mut request = format!(
        "GET {} HTTP/1.1\r\nHost: {}\r\nUser-Agent: {}\r\n",
        target.path(),
        target.host_header(),
        opts.user_agent
    );
    let encoding = if opts.accept_gzip { "gzip" } else { "identity" };
    let connection = if opts.keep_alive {
        "keep-alive"
    } else {
        "close"
    };
    request.push_str(&format!(
        "Accept: */*\r\nAccept-Encoding: {}\r\nConnection: {}\r\n",
        encoding, connection
    ));
    for (name, value) in &opts.extra_headers {
        request.push_str(&format!("{}: {}\r\n", name, value));
    }
    request.push_str("\r\n");
    request
}

/// The status line, as the status, the reason, and whether the responder
/// speaks HTTP/1.1 -- which is what decides whether its connection stays open
/// without being asked.
fn read_status_line(reader: &mut BufReader<Conn>) -> Result<(u16, String, bool), Error> {
    let line = read_line(reader)?;
    let line = line.trim_end();
    let mut parts = line.splitn(3, ' ');

    let version = parts
        .next()
        .ok_or_else(|| Error::Protocol("empty status line".to_string()))?;
    if !version.starts_with("HTTP/") {
        return Err(Error::Protocol(format!("not an HTTP response: {:?}", line)));
    }
    let http11 = version == "HTTP/1.1";
    let status: u16 = parts
        .next()
        .and_then(|code| code.parse().ok())
        .ok_or_else(|| Error::Protocol(format!("no status code in {:?}", line)))?;
    let reason = parts.next().unwrap_or("").to_string();

    Ok((status, reason, http11))
}

fn read_headers(reader: &mut BufReader<Conn>) -> Result<(Vec<(String, String)>, String), Error> {
    let mut headers = Vec::new();
    let mut raw = String::new();

    loop {
        let line = read_line(reader)?;
        raw.push_str(&line);
        let trimmed = line.trim_end_matches(['\r', '\n']);
        if trimmed.is_empty() {
            return Ok((headers, raw));
        }
        if headers.len() >= MAX_HEADERS {
            return Err(Error::Protocol(format!(
                "more than {} headers",
                MAX_HEADERS
            )));
        }
        if let Some(i) = trimmed.find(':') {
            headers.push((
                trimmed[..i].to_string(),
                trimmed[i + 1..].trim_start().to_string(),
            ));
        }
    }
}

/// Read one CRLF-terminated line, capped at [`MAX_LINE`].
fn read_line(reader: &mut BufReader<Conn>) -> Result<String, Error> {
    let mut out = Vec::new();
    let mut byte = [0u8; 1];

    loop {
        let n = reader.read(&mut byte)?;
        if n == 0 {
            if out.is_empty() {
                return Err(Error::Protocol(
                    "the connection closed before the response".to_string(),
                ));
            }
            break;
        }
        out.push(byte[0]);
        if byte[0] == b'\n' {
            break;
        }
        if out.len() > MAX_LINE {
            return Err(Error::Protocol(format!(
                "a line longer than {} bytes",
                MAX_LINE
            )));
        }
    }

    String::from_utf8(out).map_err(|_| Error::Protocol("a header line is not UTF-8".to_string()))
}

/// Read the body, and say whether its end was framed rather than found.
///
/// A body that runs to end of stream leaves the connection at end of stream,
/// which is the one case that cannot be pooled: there is nothing left to read
/// a second response from.
fn read_body(
    reader: &mut BufReader<Conn>,
    head: &Head,
    opts: &Options,
    sink: &mut dyn Write,
    progress: &mut dyn FnMut(u64, Option<u64>),
) -> Result<bool, Error> {
    let chunked = head
        .header("Transfer-Encoding")
        .is_some_and(|v| v.to_ascii_lowercase().contains("chunked"));
    let declared: Option<u64> = head
        .header("Content-Length")
        .and_then(|v| v.trim().parse().ok());

    if let Some(length) = declared
        && length > opts.max_body
    {
        return Err(Error::TooLarge {
            limit: opts.max_body,
        });
    }

    if chunked {
        read_chunked(reader, opts, sink, progress)?;
        return Ok(true);
    }
    // Without a length the body runs to end of stream, which is what
    // `Connection: close` promises.
    let limit = declared.unwrap_or(u64::MAX);
    copy_limited(reader, sink, limit, opts.max_body, declared, progress)?;
    Ok(declared.is_some())
}

/// RFC 9112 §7.1 chunked transfer coding. A CDN will use it whether or not the
/// origin does, so it is implemented even though nginx sends a length for a
/// static file.
fn read_chunked(
    reader: &mut BufReader<Conn>,
    opts: &Options,
    sink: &mut dyn Write,
    progress: &mut dyn FnMut(u64, Option<u64>),
) -> Result<(), Error> {
    let mut written = 0u64;

    loop {
        let line = read_line(reader)?;
        let text = line.trim_end();
        // Chunk extensions follow a semicolon and are ignored.
        let size_text = text.split(';').next().unwrap_or("").trim();
        let size = u64::from_str_radix(size_text, 16)
            .map_err(|_| Error::Protocol(format!("bad chunk size: {:?}", size_text)))?;

        if size == 0 {
            // Trailers, then the final blank line.
            loop {
                let line = read_line(reader)?;
                if line.trim_end().is_empty() {
                    break;
                }
            }
            progress(written, Some(written));
            return Ok(());
        }

        let before = written;
        written += copy_limited(reader, sink, size, opts.max_body, None, &mut |done, _| {
            progress(before + done, None)
        })?;

        // Each chunk is followed by its own CRLF.
        let terminator = read_line(reader)?;
        if !terminator.trim_end().is_empty() {
            return Err(Error::Protocol("chunk not terminated by CRLF".to_string()));
        }
    }
}

/// Copy exactly `take` bytes (or to end of stream when `take` is `u64::MAX`),
/// refusing to write more than `max_body` in total.
fn copy_limited(
    reader: &mut BufReader<Conn>,
    sink: &mut dyn Write,
    take: u64,
    max_body: u64,
    total: Option<u64>,
    progress: &mut dyn FnMut(u64, Option<u64>),
) -> Result<u64, Error> {
    let mut buf = vec![0u8; COPY_BUF];
    let mut done = 0u64;

    while done < take {
        let want = ((take - done).min(COPY_BUF as u64)) as usize;
        let n = reader.read(&mut buf[..want])?;
        if n == 0 {
            if take != u64::MAX && done < take {
                return Err(Error::Protocol(format!(
                    "the connection closed {} bytes short",
                    take - done
                )));
            }
            break;
        }
        done += n as u64;
        if done > max_body {
            return Err(Error::TooLarge { limit: max_body });
        }
        sink.write_all(&buf[..n])?;
        progress(done, total);
    }

    Ok(done)
}

/// A connection, with or without TLS underneath.
enum Conn {
    Plain(TcpStream),
    Tls(Box<rustls::StreamOwned<rustls::ClientConnection, TcpStream>>),
}

impl Read for Conn {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match self {
            Conn::Plain(s) => s.read(buf),
            Conn::Tls(s) => match s.read(buf) {
                // A server that drops the connection without a close_notify is
                // ordinary on the public internet, and with `Connection: close`
                // plus a length or a final chunk the body is already complete.
                Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => Ok(0),
                other => other,
            },
        }
    }
}

impl Write for Conn {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match self {
            Conn::Plain(s) => s.write(buf),
            Conn::Tls(s) => s.write(buf),
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        match self {
            Conn::Plain(s) => s.flush(),
            Conn::Tls(s) => s.flush(),
        }
    }
}
