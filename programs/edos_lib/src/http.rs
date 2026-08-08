//! Minimal HTTP/1.0 client over the kernel socket syscalls.
//!
//! `std::net::TcpStream` is unimplemented in the std fork, so clients go
//! through [`crate::net`] directly.

use std::fmt;

use crate::net::{self, SockAddrIn};

/// A parsed `http://host:port/path` target.
#[derive(Debug, Clone, Copy)]
pub struct Url<'a> {
    pub host: &'a str,
    pub port: u16,
    pub path: &'a str,
}

impl<'a> Url<'a> {
    /// Accepts `http://host:port/path`, `host:port/path`, `host/path` and `host`.
    /// The port defaults to 80 and the path to `/`.
    pub fn parse(url: &'a str) -> Result<Self, Error> {
        if url.starts_with("https://") {
            return Err(Error::NoTls);
        }
        let rest = url.strip_prefix("http://").unwrap_or(url);

        let (hostport, path) = match rest.find('/') {
            Some(i) => (&rest[..i], &rest[i..]),
            None => (rest, "/"),
        };

        let (host, port) = match hostport.rfind(':') {
            Some(i) => match hostport[i + 1..].parse::<u16>() {
                Ok(port) => (&hostport[..i], port),
                Err(_) => (hostport, 80),
            },
            None => (hostport, 80),
        };

        if host.is_empty() {
            return Err(Error::NoHost);
        }
        Ok(Self { host, port, path })
    }

    /// The last path component, or `index.html` for a directory-style path.
    pub fn filename(&self) -> &'a str {
        let trimmed = self.path.trim_end_matches('/');
        match trimmed.rfind('/') {
            Some(i) if i + 1 < trimmed.len() => &trimmed[i + 1..],
            _ => "index.html",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    NoTls,
    NoHost,
    Resolve,
    Socket,
    Connect,
    Send,
    Recv,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            Error::NoTls => "HTTPS is not supported (no TLS)",
            Error::NoHost => "missing host",
            Error::Resolve => "cannot resolve host",
            Error::Socket => "cannot create socket",
            Error::Connect => "connection failed",
            Error::Send => "send failed",
            Error::Recv => "no response",
        };
        f.write_str(s)
    }
}

/// A response split at the first CRLF CRLF. When no such separator arrives,
/// everything is treated as head and the body is empty.
pub struct Response {
    pub head: Vec<u8>,
    pub body: Vec<u8>,
}

impl Response {
    pub fn status_line(&self) -> &str {
        let end = self
            .head
            .iter()
            .position(|&b| b == b'\r' || b == b'\n')
            .unwrap_or(self.head.len());
        std::str::from_utf8(&self.head[..end]).unwrap_or("")
    }
}

/// The request text `get` sends, so a caller can echo it.
pub fn request_text(url: &Url<'_>) -> String {
    format!(
        "GET {} HTTP/1.0\r\nHost: {}\r\nConnection: close\r\n\r\n",
        url.path, url.host
    )
}

/// Fetch `url`, reading until the peer closes the connection.
pub fn get(url: &Url<'_>) -> Result<Response, Error> {
    let ip = net::resolve_host(url.host).ok_or(Error::Resolve)?;
    let fd = net::create_tcp_socket().map_err(|_| Error::Socket)?;

    let result = fetch(fd, url, ip);
    net::close(fd);
    result
}

fn fetch(fd: u64, url: &Url<'_>, ip: [u8; 4]) -> Result<Response, Error> {
    net::connect(fd, &SockAddrIn::new(ip, url.port)).map_err(|_| Error::Connect)?;

    let request = request_text(url);
    let mut sent = 0;
    while sent < request.len() {
        match net::send(fd, &request.as_bytes()[sent..]) {
            Ok(0) | Err(()) => return Err(Error::Send),
            Ok(n) => sent += n,
        }
    }

    let raw = read_to_end(fd)?;
    let head_end = raw
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .unwrap_or(raw.len());
    let body_start = (head_end + 4).min(raw.len());

    Ok(Response {
        head: raw[..head_end].to_vec(),
        body: raw[body_start..].to_vec(),
    })
}

/// Read until the peer closes. A read error after some data has arrived ends
/// the body: HTTP/1.0 with `Connection: close` has no other terminator, and
/// there is no way to tell a reset apart from a close here.
fn read_to_end(fd: u64) -> Result<Vec<u8>, Error> {
    let mut out = Vec::new();
    let mut buf = [0u8; 4096];
    loop {
        match net::recv(fd, &mut buf) {
            Ok(0) => break,
            Ok(n) => out.extend_from_slice(&buf[..n]),
            Err(()) => break,
        }
    }
    if out.is_empty() {
        return Err(Error::Recv);
    }
    Ok(out)
}
