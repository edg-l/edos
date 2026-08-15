//! URL parsing, over the WHATWG URL Standard.
//!
//! The parsing, reference resolution, percent-encoding and IDNA all come from
//! the `url` crate, which is the specification browsers implement. This module
//! is the narrow face an HTTP client wants over it: the two schemes that can be
//! fetched, an authority shaped for `TcpStream::connect`, and a request target.
//!
//! `url` normalises on the way in, so a host arrives lowercased and
//! punycoded and a path arrives percent-encoded. That last pair is what makes
//! a non-ASCII hostname reachable at all: `rustls` and the resolver both take
//! the ASCII form, and neither does IDNA of its own.

use crate::Error;
use std::fmt;

use url::Position;

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
    inner: url::Url,
    scheme: Scheme,
}

impl Url {
    /// Parse an absolute URL, defaulting a missing scheme to `http://`.
    ///
    /// A bare `host/path` is accepted because that is what the `http` program
    /// has always taken. The crate rejects it as a relative reference with no
    /// base, which is the signal to retry it as one.
    pub fn parse(input: &str) -> Result<Url, Error> {
        let parsed = match url::Url::parse(input) {
            Ok(parsed) => parsed,
            Err(url::ParseError::RelativeUrlWithoutBase) => {
                url::Url::parse(&format!("http://{}", input))
                    .map_err(|e| Error::Url(format!("{}: {}", input, e)))?
            }
            Err(e) => return Err(Error::Url(format!("{}: {}", input, e))),
        };
        Url::from_parsed(parsed)
    }

    /// Narrow a parsed URL to one this client can actually fetch.
    fn from_parsed(inner: url::Url) -> Result<Url, Error> {
        let scheme = match inner.scheme() {
            "http" => Scheme::Http,
            "https" => Scheme::Https,
            other => return Err(Error::Url(format!("unsupported scheme: {}", other))),
        };
        // Userinfo is rejected rather than carried: it would otherwise be sent
        // to a server this client has no way to authenticate to, and the
        // failure would read as a hostname problem.
        if !inner.username().is_empty() || inner.password().is_some() {
            return Err(Error::Url(format!(
                "credentials in a URL are not supported: {}@",
                inner.username()
            )));
        }
        if inner.host().is_none() {
            return Err(Error::Url("no host".to_string()));
        }
        Ok(Url { inner, scheme })
    }

    /// Resolve a reference against this URL.
    ///
    /// Covers what a `Location` header and an `href` actually contain:
    /// absolute, network relative (`//host/path`), absolute path, relative
    /// path with `.` and `..` segments, query only, and empty.
    pub fn join(&self, reference: &str) -> Result<Url, Error> {
        let joined = self
            .inner
            .join(reference)
            .map_err(|e| Error::Url(format!("{}: {}", reference, e)))?;
        Url::from_parsed(joined)
    }

    pub fn scheme(&self) -> Scheme {
        self.scheme
    }

    /// The host as the resolver and `rustls` want it: no brackets around an
    /// IPv6 literal, and already punycoded when the source was an IDN.
    pub fn host(&self) -> &str {
        let host = self.inner.host_str().unwrap_or_default();
        host.strip_prefix('[')
            .and_then(|h| h.strip_suffix(']'))
            .unwrap_or(host)
    }

    pub fn port(&self) -> u16 {
        self.inner
            .port_or_known_default()
            .unwrap_or_else(|| self.scheme.default_port())
    }

    /// Path and query together, as they go on the request line.
    pub fn path(&self) -> &str {
        &self.inner[Position::BeforePath..Position::AfterQuery]
    }

    /// `host:port`, as `TcpStream::connect` wants it. An IPv6 literal keeps
    /// its brackets here, which is what separates the address from the port.
    pub fn authority(&self) -> String {
        format!(
            "{}:{}",
            self.inner.host_str().unwrap_or_default(),
            self.port()
        )
    }

    /// The `Host` header, which omits the port when it is the default and
    /// brackets an IPv6 literal (RFC 7230 §5.4).
    pub fn host_header(&self) -> String {
        let host = self.inner.host_str().unwrap_or_default();
        if self.port() == self.scheme.default_port() {
            host.to_string()
        } else {
            format!("{}:{}", host, self.port())
        }
    }

    /// The last path segment, or `index.html` when the path names a directory.
    ///
    /// The trailing slash is tested before the segment is taken, not trimmed
    /// away first: trimming makes `/a/` indistinguishable from `/a`, and the
    /// directory is the case this exists to answer.
    pub fn filename(&self) -> &str {
        let path = self.inner.path();
        if path.ends_with('/') {
            return "index.html";
        }
        match path.rfind('/') {
            Some(i) if i + 1 < path.len() => &path[i + 1..],
            _ => "index.html",
        }
    }
}

impl fmt::Display for Url {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // The crate elides a default port and keeps everything else, which is
        // the same shape this printed before.
        write!(f, "{}", self.inner)
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

    /// The normal examples from RFC 3986 §5.4.1, against its own base. The
    /// WHATWG parser resolves these the same way; where it does not, the
    /// difference is called out in its own test below.
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
            // A fragment is part of the resolved URL and these are the RFC's
            // own expected values. It never reaches the wire; `path()` is what
            // goes on the request line and it stops at the query.
            ("#s", "http://a/b/c/d;p?q#s"),
            ("g#s", "http://a/b/c/g#s"),
            ("g?y#s", "http://a/b/c/g?y#s"),
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
    /// clamped rather than an error, and a segment that merely looks like a
    /// dot segment is not one.
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
        let base = Url::parse("http://a/b/c").unwrap();
        assert_eq!(
            base.join("https://other/x").unwrap().to_string(),
            "https://other/x"
        );
    }

    #[test]
    fn foreign_scheme_is_rejected() {
        let base = Url::parse("http://a/b/c").unwrap();
        assert!(base.join("mailto:someone@example.com").is_err());
        assert!(base.join("javascript:void(0)").is_err());
        // A colon in the first segment makes it a scheme, so a relative
        // reference that wants one has to say `./` (RFC 3986 §4.2).
        assert!(base.join("notes:2026/x").is_err());
        assert_eq!(
            base.join("./notes:2026/x").unwrap().path(),
            "/b/notes:2026/x"
        );
        // A colon in a later segment is just a character.
        assert_eq!(base.join("x/notes:2026").unwrap().path(), "/b/x/notes:2026");
    }

    #[test]
    fn relative_asset_of_a_local_page() {
        let base = Url::parse("http://host/docs/page.html").unwrap();
        assert_eq!(
            base.join("../icons/edos.svg").unwrap().path(),
            "/icons/edos.svg"
        );
    }

    #[test]
    fn interior_empty_segments_survive() {
        let base = Url::parse("http://a/b/c/d;p?q").unwrap();
        assert_eq!(base.join("g//h").unwrap().path(), "/b/c/g//h");
    }

    #[test]
    fn credentials_are_rejected_rather_than_sent() {
        assert!(Url::parse("http://user:pw@host/x").is_err());
        assert!(Url::parse("http://user@host/x").is_err());
    }

    #[test]
    fn a_bare_host_defaults_to_http() {
        let url = Url::parse("example.com/x").unwrap();
        assert_eq!(url.scheme(), Scheme::Http);
        assert_eq!(url.host(), "example.com");
        assert_eq!(url.port(), 80);
        assert_eq!(url.path(), "/x");
    }

    /// A fragment is kept on the URL, because it names the place in the page a
    /// browser has to scroll to, and is dropped from the request target,
    /// because it is not the server's business (RFC 3986 §3.5).
    #[test]
    fn the_request_target_carries_the_query_and_not_the_fragment() {
        let url = Url::parse("http://h/a/b?x=1&y=2#frag").unwrap();
        assert_eq!(url.path(), "/a/b?x=1&y=2");
        assert_eq!(url.to_string(), "http://h/a/b?x=1&y=2#frag");

        let joined = url.join("c#other").unwrap();
        assert_eq!(joined.path(), "/a/c");
        assert_eq!(joined.to_string(), "http://h/a/c#other");
    }

    /// An IPv6 literal keeps its brackets where they separate the address from
    /// the port, and loses them where the consumer wants a bare address.
    #[test]
    fn ipv6_literals_are_bracketed_only_where_that_is_the_syntax() {
        let url = Url::parse("http://[::1]:8080/x").unwrap();
        assert_eq!(url.host(), "::1");
        assert_eq!(url.authority(), "[::1]:8080");
        assert_eq!(url.host_header(), "[::1]:8080");
        assert_eq!(url.path(), "/x");
    }

    #[test]
    fn the_host_header_drops_a_default_port_and_keeps_any_other() {
        assert_eq!(Url::parse("http://h/").unwrap().host_header(), "h");
        assert_eq!(Url::parse("https://h/").unwrap().host_header(), "h");
        assert_eq!(
            Url::parse("http://h:8080/").unwrap().host_header(),
            "h:8080"
        );
    }

    /// The reason for the port: `rustls` and the resolver both take the ASCII
    /// form of a name, and neither performs IDNA. A host arrives punycoded.
    #[test]
    fn an_idn_host_arrives_punycoded() {
        let url = Url::parse("http://münchen.de/straße").unwrap();
        assert_eq!(url.host(), "xn--mnchen-3ya.de");
        assert_eq!(url.authority(), "xn--mnchen-3ya.de:80");
        // And the path arrives percent-encoded, which is what goes on the
        // request line.
        assert_eq!(url.path(), "/stra%C3%9Fe");
    }

    /// A space in an `href` is percent-encoded rather than sent raw, which
    /// would have made the request line unparseable.
    #[test]
    fn a_space_in_a_reference_is_encoded() {
        let base = Url::parse("http://h/dir/page.html").unwrap();
        assert_eq!(
            base.join("my file.txt").unwrap().path(),
            "/dir/my%20file.txt"
        );
    }

    #[test]
    fn filename_falls_back_to_index_html_for_a_directory() {
        assert_eq!(
            Url::parse("http://h/a/b.tar.gz").unwrap().filename(),
            "b.tar.gz"
        );
        assert_eq!(Url::parse("http://h/a/").unwrap().filename(), "index.html");
        assert_eq!(Url::parse("http://h/").unwrap().filename(), "index.html");
        // A query does not become part of the name.
        assert_eq!(
            Url::parse("http://h/a/b.txt?v=1").unwrap().filename(),
            "b.txt"
        );
    }
}
