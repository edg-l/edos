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

    /// Resolve a reference against this URL, per RFC 3986 §5.2.
    ///
    /// Covers what a `Location` header and an `href` actually contain:
    /// absolute, network relative (`//host/path`), absolute path, relative
    /// path with `.` and `..` segments, query only, and empty.
    pub fn join(&self, reference: &str) -> Result<Url, Error> {
        // A fragment names a place in the retrieved document and never reaches
        // the server, so it is stripped before anything else looks at it.
        let reference = match reference.find('#') {
            Some(i) => &reference[..i],
            None => reference,
        };

        match scheme_prefix(reference) {
            Some("http") | Some("https") => return Url::parse(reference),
            Some(other) => return Err(Error::Url(format!("unsupported scheme: {}", other))),
            None => {}
        }
        if let Some(rest) = reference.strip_prefix("//") {
            return Url::parse(&format!("{}://{}", self.scheme.as_str(), rest));
        }

        let (base_path, base_query) = split_query(&self.path);
        let (ref_path, ref_query) = split_query(reference);

        let path = if ref_path.is_empty() {
            base_path.to_string()
        } else if ref_path.starts_with('/') {
            remove_dot_segments(ref_path)
        } else {
            remove_dot_segments(&merge(base_path, ref_path))
        };
        // An empty reference path keeps the base's query, but only when the
        // reference carries none of its own (RFC 3986 §5.2.2).
        let query = if ref_path.is_empty() && ref_query.is_none() {
            base_query
        } else {
            ref_query
        };

        Ok(Url {
            path: match query {
                Some(q) => format!("{}?{}", path, q),
                None => path,
            },
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

/// The scheme of a reference that has one, per RFC 3986 §3.1.
///
/// A colon inside a path segment (`notes:2026/x`) is not a scheme, and neither
/// is one that follows a `/`, `?` or `#`.
fn scheme_prefix(reference: &str) -> Option<&str> {
    let end = reference.find(|c| c == ':' || c == '/' || c == '?' || c == '#')?;
    if reference.as_bytes()[end] != b':' || end == 0 {
        return None;
    }
    let scheme = &reference[..end];
    let mut bytes = scheme.bytes();
    if !bytes.next()?.is_ascii_alphabetic() {
        return None;
    }
    bytes
        .all(|b| b.is_ascii_alphanumeric() || b == b'+' || b == b'-' || b == b'.')
        .then_some(scheme)
}

/// Split a request target into its path and its query, without the `?`.
fn split_query(target: &str) -> (&str, Option<&str>) {
    match target.find('?') {
        Some(i) => (&target[..i], Some(&target[i + 1..])),
        None => (target, None),
    }
}

/// Merge a relative path onto a base path, per RFC 3986 §5.3.
fn merge(base_path: &str, reference: &str) -> String {
    // Every URL here has an authority, so an empty base path is `/`.
    match base_path.rfind('/') {
        Some(i) => format!("{}{}", &base_path[..=i], reference),
        None => format!("/{}", reference),
    }
}

/// Resolve `.` and `..` segments, per RFC 3986 §5.2.4.
fn remove_dot_segments(path: &str) -> String {
    let segments: Vec<&str> = path.split('/').collect();
    let mut out: Vec<&str> = Vec::new();
    for (i, segment) in segments.iter().enumerate() {
        let last = i + 1 == segments.len();
        match *segment {
            "." => {}
            ".." => {
                out.pop();
            }
            // The empty segments a leading and a trailing `/` produce are the
            // separators themselves; an interior one is a real empty segment.
            "" if i == 0 || last => {}
            segment => out.push(segment),
        }
    }

    // A path whose last segment is empty, `.` or `..` names a directory, so
    // the result keeps its trailing slash.
    let directory = matches!(segments.last(), Some(&"") | Some(&".") | Some(&".."));
    let mut resolved = String::new();
    if path.starts_with('/') {
        resolved.push('/');
    }
    resolved.push_str(&out.join("/"));
    if directory && !resolved.ends_with('/') {
        resolved.push('/');
    }
    resolved
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

#[cfg(test)]
mod tests {
    use super::*;

    fn joined(base: &str, reference: &str) -> String {
        Url::parse(base)
            .unwrap()
            .join(reference)
            .unwrap()
            .to_string()
    }

    /// The normal examples from RFC 3986 §5.4.1, against its own base.
    #[test]
    fn rfc3986_normal_examples() {
        let base = "http://a/b/c/d;p?q";
        for (reference, expected) in [
            ("g", "http://a/b/c/g"),
            ("./g", "http://a/b/c/g"),
            ("g/", "http://a/b/c/g/"),
            ("/g", "http://a/g"),
            ("//g", "http://g/"),
            ("?y", "http://a/b/c/d;p?y"),
            ("g?y", "http://a/b/c/g?y"),
            ("#s", "http://a/b/c/d;p?q"),
            ("g#s", "http://a/b/c/g"),
            ("g?y#s", "http://a/b/c/g?y"),
            (";x", "http://a/b/c/;x"),
            ("g;x", "http://a/b/c/g;x"),
            ("", "http://a/b/c/d;p?q"),
            (".", "http://a/b/c/"),
            ("./", "http://a/b/c/"),
            ("..", "http://a/b/"),
            ("../", "http://a/b/"),
            ("../g", "http://a/b/g"),
            ("../..", "http://a/"),
            ("../../g", "http://a/g"),
        ] {
            assert_eq!(
                joined(base, reference),
                expected,
                "reference {:?}",
                reference
            );
        }
    }

    /// The abnormal examples from RFC 3986 §5.4.2: climbing past the root is
    /// absorbed rather than an error, and a dot is only a dot when it is a
    /// whole segment.
    #[test]
    fn rfc3986_abnormal_examples() {
        let base = "http://a/b/c/d;p?q";
        for (reference, expected) in [
            ("../../../g", "http://a/g"),
            ("../../../../g", "http://a/g"),
            ("/./g", "http://a/g"),
            ("/../g", "http://a/g"),
            ("g.", "http://a/b/c/g."),
            (".g", "http://a/b/c/.g"),
            ("g..", "http://a/b/c/g.."),
            ("..g", "http://a/b/c/..g"),
            ("./../g", "http://a/b/g"),
            ("./g/.", "http://a/b/c/g/"),
            ("g/./h", "http://a/b/c/g/h"),
            ("g/../h", "http://a/b/c/h"),
            ("g;x=1/./y", "http://a/b/c/g;x=1/y"),
            ("g;x=1/../y", "http://a/b/c/y"),
        ] {
            assert_eq!(
                joined(base, reference),
                expected,
                "reference {:?}",
                reference
            );
        }
    }

    #[test]
    fn absolute_reference_wins_over_the_base() {
        assert_eq!(joined("http://a/b/c", "https://x/y?z"), "https://x/y?z");
    }

    /// A scheme this client cannot fetch is refused, rather than merged into
    /// the base and turned into a nonsense HTTP request.
    #[test]
    fn foreign_scheme_is_rejected() {
        let base = Url::parse("http://a/b/c").unwrap();
        assert!(base.join("mailto:someone@example.com").is_err());
        assert!(base.join("javascript:void(0)").is_err());
        // A colon in the first segment makes it a scheme, so a relative
        // reference that wants one has to say `./` (RFC 3986 §4.2).
        assert!(base.join("notes:2026/x").is_err());
        assert_eq!(base.join("./notes:2026/x").unwrap().path, "/b/notes:2026/x");
        // A colon in a later segment is just a character.
        assert_eq!(base.join("x/notes:2026").unwrap().path, "/b/x/notes:2026");
    }

    /// A local page's own directory is the base, which is what lets an
    /// installed document reference a sibling asset.
    #[test]
    fn relative_asset_of_a_local_page() {
        let base = Url::parse("http://localhost/share/web/welcome.html").unwrap();
        assert_eq!(
            base.join("../icons/edos.svg").unwrap().path,
            "/share/icons/edos.svg"
        );
        assert_eq!(base.join("style.css").unwrap().path, "/share/web/style.css");
    }

    #[test]
    fn interior_empty_segments_survive() {
        let base = Url::parse("http://a/b/c").unwrap();
        assert_eq!(base.join("/x//y").unwrap().path, "/x//y");
    }
}
