//! URL parsing, for the subset an HTTP client needs.

use crate::Error;
use std::fmt;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Scheme {
    Http,
    Https,
}

impl Scheme {
    pub fn default_port(self) -> u16 {
        match self {
            Scheme::Http => 80,
            Scheme::Https => 443,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Scheme::Http => "http",
            Scheme::Https => "https",
        }
    }
}

#[derive(Clone, Debug)]
pub struct Url {
    pub scheme: Scheme,
    pub host: String,
    pub port: u16,
    /// Path and query together, as they go on the request line.
    pub path: String,
}

impl Url {
    /// Parse an absolute URL, defaulting a missing scheme to `http://`.
    ///
    /// A bare `host/path` is accepted because that is what the existing `http`
    /// program has always taken.
    pub fn parse(input: &str) -> Result<Url, Error> {
        let (scheme, rest) = if let Some(rest) = input.strip_prefix("https://") {
            (Scheme::Https, rest)
        } else if let Some(rest) = input.strip_prefix("http://") {
            (Scheme::Http, rest)
        } else if let Some(i) = input.find("://") {
            return Err(Error::Url(format!("unsupported scheme: {}", &input[..i])));
        } else {
            (Scheme::Http, input)
        };

        // The authority ends at the first '/', '?' or '#'.
        let end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
        let (authority, tail) = rest.split_at(end);

        // Userinfo is parsed only to reject it: it would otherwise be read as
        // part of the hostname and produce a baffling DNS failure.
        if let Some(i) = authority.find('@') {
            return Err(Error::Url(format!(
                "credentials in a URL are not supported: {}@",
                &authority[..i]
            )));
        }

        let (host, port) = split_host_port(authority, scheme)?;
        if host.is_empty() {
            return Err(Error::Url("no host".to_string()));
        }

        let path = match tail.find('#') {
            Some(i) => &tail[..i],
            None => tail,
        };
        let path = if path.is_empty() || path.starts_with('?') {
            format!("/{}", path)
        } else {
            path.to_string()
        };

        Ok(Url {
            scheme,
            host: host.to_string(),
            port,
            path,
        })
    }

    /// Resolve a `Location` header against this URL.
    ///
    /// Handles the three forms a redirect actually takes: absolute, network
    /// relative (`//host/path`), and path relative.
    pub fn join(&self, location: &str) -> Result<Url, Error> {
        if location.starts_with("http://") || location.starts_with("https://") {
            return Url::parse(location);
        }
        if let Some(rest) = location.strip_prefix("//") {
            return Url::parse(&format!("{}://{}", self.scheme.as_str(), rest));
        }
        if location.starts_with('/') {
            return Ok(Url {
                path: location.to_string(),
                ..self.clone()
            });
        }
        let base = match self.path.rfind('/') {
            Some(i) => &self.path[..=i],
            None => "/",
        };
        Ok(Url {
            path: format!("{}{}", base, location),
            ..self.clone()
        })
    }

    /// `host:port`, as `TcpStream::connect` wants it.
    pub fn authority(&self) -> String {
        format!("{}:{}", self.host, self.port)
    }

    /// The `Host` header, which omits the port when it is the default.
    pub fn host_header(&self) -> String {
        if self.port == self.scheme.default_port() {
            self.host.clone()
        } else {
            self.authority()
        }
    }

    /// The last path segment, or `index.html` when the path names a directory.
    pub fn filename(&self) -> &str {
        let path = match self.path.find('?') {
            Some(i) => &self.path[..i],
            None => &self.path,
        };
        let trimmed = path.trim_end_matches('/');
        match trimmed.rfind('/') {
            Some(i) if i + 1 < trimmed.len() => &trimmed[i + 1..],
            _ => "index.html",
        }
    }
}

fn split_host_port(authority: &str, scheme: Scheme) -> Result<(&str, u16), Error> {
    // A bracketed IPv6 literal holds colons that are not the port separator.
    if let Some(rest) = authority.strip_prefix('[') {
        let Some(close) = rest.find(']') else {
            return Err(Error::Url("unterminated IPv6 literal".to_string()));
        };
        let host = &rest[..close];
        let after = &rest[close + 1..];
        let port = match after.strip_prefix(':') {
            Some(p) => parse_port(p)?,
            None if after.is_empty() => scheme.default_port(),
            None => return Err(Error::Url(format!("trailing junk in host: {}", after))),
        };
        return Ok((host, port));
    }

    match authority.rfind(':') {
        Some(i) => Ok((&authority[..i], parse_port(&authority[i + 1..])?)),
        None => Ok((authority, scheme.default_port())),
    }
}

fn parse_port(text: &str) -> Result<u16, Error> {
    text.parse()
        .map_err(|_| Error::Url(format!("bad port: {}", text)))
}

impl fmt::Display for Url {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.port == self.scheme.default_port() {
            write!(f, "{}://{}{}", self.scheme.as_str(), self.host, self.path)
        } else {
            write!(
                f,
                "{}://{}:{}{}",
                self.scheme.as_str(),
                self.host,
                self.port,
                self.path
            )
        }
    }
}
