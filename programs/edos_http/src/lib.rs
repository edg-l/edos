//! An HTTP/1.1 client with TLS, shared by `wget`, `http` and `grab`.
//!
//! This is a separate crate rather than a module of `edos_lib` because every
//! program in the tree links `edos_lib`, and rustls has no business inside
//! `true` and `yes`.

use std::{
    fmt,
    io::{self, BufReader, Read, Write},
    net::{TcpStream, ToSocketAddrs},
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
        let (mut reader, sent) = send_request(&target, opts)?;
        let (status, reason) = read_status_line(&mut reader)?;
        let (headers, raw_headers) = read_headers(&mut reader)?;

        let head = Head {
            status,
            reason,
            headers,
            final_url: target.to_string(),
            sent,
            raw_headers,
        };

        if is_redirect(status) {
            if let Some(location) = head.header("Location") {
                // The body of a redirect is of no interest, and the connection
                // is closed rather than drained: `Connection: close` means
                // there is nothing to reuse it for.
                target = target.join(location)?;
                continue;
            }
        }

        read_body(&mut reader, &head, opts, sink, progress)?;
        return Ok(head);
    }

    Err(Error::TooManyRedirects(opts.max_redirects))
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

/// Open a connection and write the request, returning a reader positioned at
/// the response.
fn send_request(target: &Url, opts: &Options) -> Result<(BufReader<Conn>, String), Error> {
    let addr = target.authority();
    let tcp = connect(&addr, opts.connect_timeout).map_err(|source| Error::Connect {
        addr: addr.clone(),
        source,
    })?;

    let mut request = format!(
        "GET {} HTTP/1.1\r\nHost: {}\r\nUser-Agent: {}\r\n",
        target.path(),
        target.host_header(),
        opts.user_agent
    );
    // Asking for `identity` keeps a content-decode path out of the client. A
    // package is already gzip content; gzipping the transfer as well buys
    // nothing and adds a branch that only some servers exercise.
    request.push_str("Accept: */*\r\nAccept-Encoding: identity\r\nConnection: close\r\n");
    for (name, value) in &opts.extra_headers {
        request.push_str(&format!("{}: {}\r\n", name, value));
    }
    request.push_str("\r\n");

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

    Ok((BufReader::new(conn), request))
}

fn read_status_line(reader: &mut BufReader<Conn>) -> Result<(u16, String), Error> {
    let line = read_line(reader)?;
    let line = line.trim_end();
    let mut parts = line.splitn(3, ' ');

    let version = parts
        .next()
        .ok_or_else(|| Error::Protocol("empty status line".to_string()))?;
    if !version.starts_with("HTTP/") {
        return Err(Error::Protocol(format!("not an HTTP response: {:?}", line)));
    }
    let status: u16 = parts
        .next()
        .and_then(|code| code.parse().ok())
        .ok_or_else(|| Error::Protocol(format!("no status code in {:?}", line)))?;
    let reason = parts.next().unwrap_or("").to_string();

    Ok((status, reason))
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

fn read_body(
    reader: &mut BufReader<Conn>,
    head: &Head,
    opts: &Options,
    sink: &mut dyn Write,
    progress: &mut dyn FnMut(u64, Option<u64>),
) -> Result<(), Error> {
    let chunked = head
        .header("Transfer-Encoding")
        .is_some_and(|v| v.to_ascii_lowercase().contains("chunked"));
    let declared: Option<u64> = head
        .header("Content-Length")
        .and_then(|v| v.trim().parse().ok());

    if let Some(length) = declared {
        if length > opts.max_body {
            return Err(Error::TooLarge {
                limit: opts.max_body,
            });
        }
    }

    if chunked {
        read_chunked(reader, opts, sink, progress)
    } else {
        // Without a length the body runs to end of stream, which is what
        // `Connection: close` promises.
        let limit = declared.unwrap_or(u64::MAX);
        copy_limited(reader, sink, limit, opts.max_body, declared, progress)?;
        Ok(())
    }
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
