//! `uri` (R-134): a URI by RFC 3986's generic syntax, not a browser's
//! WHATWG URL. What a program holds is `s3://bucket/key`,
//! `postgres://user:pw@host/db`, `git+ssh://git@host/repo`, `file:///etc`,
//! and the forms with no authority, `mailto:ops@example.com`,
//! `tel:+15551234`, `urn:ietf:rfc:3986`, `data:text/plain,hi`: a scheme
//! and the parts the generic syntax gives every scheme alike.
//!
//! A uri is held normalized (RFC 3986 section 6.2.2 and, for the schemes
//! that define them, 6.2.3): its scheme lower-case, its percent-escapes'
//! hex upper-case and an escaped unreserved character unescaped, `.` and
//! `..` resolved in a hierarchical path, a default port dropped and an
//! empty path `/` for `http`, `https`, `ws`, `wss`; so two spellings of
//! one uri are equal.
//!
//! A host is held as written (its Unicode form, NFC, lower-case) and
//! printed so everywhere; two hosts are equal by their A-labels (UTS 46,
//! the `idna` crate), so `bücher.example` and `xn--bcher-kva.example` are
//! one host. The A-label form is what crosses the provider boundary
//! ([`Uri::ascii`]); a host read back from the world prints as read,
//! never decoded, unless it is equal to the program's.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// A URI, its parts normalized.
#[derive(Debug, Clone)]
pub struct Uri {
    pub scheme: String,
    /// The authority's userinfo before its first `:`, as escaped.
    pub user: Option<String>,
    /// The userinfo after its first `:`, as escaped.
    pub password: Option<String>,
    /// The host as written: a registered name (Unicode, NFC,
    /// lower-case), an IPv4 address or a bracketed IP literal. With an
    /// authority a uri has a host, which may be empty (`file:///etc`).
    pub host: Option<String>,
    /// An explicit port that is not the scheme's default.
    pub port: Option<u16>,
    pub path: String,
    pub query: Option<String>,
    pub fragment: Option<String>,
}

/// The schemes whose default port and empty path RFC 3986 section 6.2.3
/// normalizes (their own specifications define them).
fn default_port(scheme: &str) -> Option<u16> {
    Some(match scheme {
        "http" | "ws" => 80,
        "https" | "wss" => 443,
        "ftp" => 21,
        _ => return None,
    })
}

fn web(scheme: &str) -> bool {
    matches!(scheme, "http" | "https" | "ws" | "wss")
}

const SHAPE: &str = "a uri is `scheme:` and its parts, RFC 3986: `https://host/path`, \
                     `postgres://user@host:5432/db`, `mailto:ops@example.com`";

fn unreserved(c: u8) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, b'-' | b'.' | b'_' | b'~')
}

fn sub_delim(c: u8) -> bool {
    matches!(
        c,
        b'!' | b'$' | b'&' | b'\'' | b'(' | b')' | b'*' | b'+' | b',' | b';' | b'='
    )
}

/// `s` as a component allows (`extra`: its own delimiters, `:` and `@` in
/// a path): percent-escapes upper-cased and an unreserved character's
/// escape decoded (6.2.2.1, 6.2.2.2); a non-ASCII character escaped as
/// its UTF-8 (an IRI's text read as the URI it maps to, RFC 3987); a
/// character no component allows an error.
fn normalize(s: &str, what: &str, extra: &[u8]) -> Result<String, String> {
    let b = s.as_bytes();
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    while i < b.len() {
        let c = b[i];
        if c == b'%' {
            let hex = s
                .get(i + 1..i + 3)
                .filter(|h| h.bytes().all(|x| x.is_ascii_hexdigit()));
            let Some(hex) = hex else {
                return Err(format!("its {what} has a `%` that is no escape (`%2F`)"));
            };
            let v = u8::from_str_radix(hex, 16).unwrap_or_default();
            if unreserved(v) {
                out.push(v as char);
            } else {
                out.push('%');
                out.push_str(&hex.to_ascii_uppercase());
            }
            i += 3;
            continue;
        }
        if c >= 0x80 {
            let ch = s[i..].chars().next().unwrap_or_default();
            let mut buf = [0u8; 4];
            for x in ch.encode_utf8(&mut buf).bytes() {
                out.push_str(&format!("%{x:02X}"));
            }
            i += ch.len_utf8();
            continue;
        }
        if unreserved(c) || sub_delim(c) || extra.contains(&c) {
            out.push(c as char);
            i += 1;
            continue;
        }
        return Err(format!(
            "its {what} holds {:?}, which a uri escapes (`%{c:02X}`)",
            c as char
        ));
    }
    Ok(out)
}

/// RFC 3986 section 5.2.4: `.` and `..` segments removed.
fn remove_dot_segments(path: &str) -> String {
    let mut input = path.to_string();
    let mut out = String::new();
    while !input.is_empty() {
        if let Some(r) = input.strip_prefix("../") {
            input = r.to_string();
        } else if let Some(r) = input.strip_prefix("./") {
            input = r.to_string();
        } else if input.starts_with("/./") {
            input = input[2..].to_string();
        } else if input == "/." {
            input = "/".to_string();
        } else if input.starts_with("/../") || input == "/.." {
            input = format!("/{}", &input[if input == "/.." { 3 } else { 4 }..]);
            match out.rfind('/') {
                Some(i) => out.truncate(i),
                None => out.clear(),
            }
        } else if input == "." || input == ".." {
            input.clear();
        } else {
            let start = usize::from(input.starts_with('/'));
            let end = input[start..].find('/').map_or(input.len(), |i| i + start);
            out.push_str(&input[..end]);
            input = input[end..].to_string();
        }
    }
    out
}

/// A host as written, normalized: an IP literal lower-case, a
/// registered name NFC and lower-case, checked as a name IDNA reads.
fn host(s: &str) -> Result<String, String> {
    if let Some(lit) = s.strip_prefix('[') {
        let Some(inner) = lit.strip_suffix(']') else {
            return Err(format!("its host {s:?} opens a `[` it does not close"));
        };
        if inner.parse::<std::net::Ipv6Addr>().is_err() && !inner.starts_with(['v', 'V']) {
            return Err(format!("its host {s:?} is no IPv6 address"));
        }
        return Ok(s.to_ascii_lowercase());
    }
    if s.contains('%') {
        return Err(format!(
            "its host {s:?} is escaped: write the name as it reads (`bücher.example`)"
        ));
    }
    use unicode_normalization::UnicodeNormalization;
    let h: String = s.nfc().collect::<String>().to_lowercase();
    if h.is_ascii() {
        if let Some(c) = h.bytes().find(|c| !(unreserved(*c) || sub_delim(*c))) {
            return Err(format!("its host {s:?} holds {:?}", c as char));
        }
    } else if idna::domain_to_ascii(&h).is_err() {
        return Err(format!("its host {s:?} is not a name IDNA reads (UTS 46)"));
    }
    Ok(h)
}

impl Uri {
    /// `text` read as a uri, or why it is not one.
    pub fn parse(text: &str) -> Result<Uri, String> {
        let why = |w: String| format!("{text:?} is not a uri: {w}; {SHAPE}");
        let colon = text
            .find(':')
            .ok_or_else(|| why("it has no scheme".into()))?;
        let scheme = &text[..colon];
        let ok = scheme
            .bytes()
            .next()
            .is_some_and(|c| c.is_ascii_alphabetic())
            && scheme
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'+' | b'-' | b'.'));
        if !ok {
            return Err(why(format!("{scheme:?} is no scheme")));
        }
        let scheme = scheme.to_ascii_lowercase();
        let rest = &text[colon + 1..];
        let (rest, fragment) = match rest.split_once('#') {
            Some((r, f)) => (r, Some(f)),
            None => (rest, None),
        };
        let (rest, query) = match rest.split_once('?') {
            Some((r, q)) => (r, Some(q)),
            None => (rest, None),
        };
        let (authority, path) = match rest.strip_prefix("//") {
            Some(a) => match a.find('/') {
                Some(i) => (Some(&a[..i]), &a[i..]),
                None => (Some(a), ""),
            },
            None => (None, rest),
        };
        let mut u = Uri {
            scheme,
            user: None,
            password: None,
            host: None,
            port: None,
            path: String::new(),
            query: query
                .map(|q| normalize(q, "query", b":@/?"))
                .transpose()
                .map_err(why)?,
            fragment: fragment
                .map(|f| normalize(f, "fragment", b":@/?"))
                .transpose()
                .map_err(why)?,
        };
        if let Some(a) = authority {
            let (userinfo, hostport) = match a.rsplit_once('@') {
                Some((u, h)) => (Some(u), h),
                None => (None, a),
            };
            if let Some(ui) = userinfo {
                let (user, pw) = match ui.split_once(':') {
                    Some((u, p)) => (u, Some(p)),
                    None => (ui, None),
                };
                u.user = Some(normalize(user, "user", b"").map_err(why)?);
                u.password = pw
                    .map(|p| normalize(p, "password", b":"))
                    .transpose()
                    .map_err(why)?;
            }
            // The port is after the last `:` outside an IP literal.
            let split = match hostport.rfind(']') {
                Some(close) => hostport[close..].find(':').map(|i| i + close),
                None => hostport.rfind(':'),
            };
            let (h, port) = match split {
                Some(i) => (&hostport[..i], Some(&hostport[i + 1..])),
                None => (hostport, None),
            };
            u.host = Some(if h.is_empty() {
                String::new()
            } else {
                host(h).map_err(why)?
            });
            u.port = match port.filter(|p| !p.is_empty()) {
                None => None,
                Some(p) => Some(
                    p.parse::<u16>()
                        .map_err(|_| why(format!("its port {p:?} is no port (0 to 65535)")))?,
                ),
            };
            if u.port.is_some() && u.port == default_port(&u.scheme) {
                u.port = None;
            }
        }
        let path = normalize(path, "path", b":@/").map_err(why)?;
        u.path = if authority.is_some() || path.starts_with('/') {
            remove_dot_segments(&path)
        } else {
            path
        };
        if authority.is_some() && u.path.is_empty() && web(&u.scheme) {
            u.path = "/".to_string();
        }
        Ok(u)
    }

    /// Whether the host has a label that is not ASCII.
    pub fn unicode_host(&self) -> bool {
        self.host.as_deref().is_some_and(|h| !h.is_ascii())
    }

    /// The host's A-label form (`xn--bcher-kva.example`): what crosses
    /// the provider boundary and what two hosts compare by.
    pub fn host_ascii(&self) -> Option<String> {
        let h = self.host.as_deref()?;
        Some(match h.is_ascii() {
            true => h.to_string(),
            false => idna::domain_to_ascii(h).unwrap_or_else(|_| h.to_string()),
        })
    }

    /// The text with the host as `host` gives it.
    fn text_with(&self, host: Option<&str>) -> String {
        let mut s = format!("{}:", self.scheme);
        if let Some(h) = host {
            s.push_str("//");
            if let Some(u) = &self.user {
                s.push_str(u);
                if let Some(p) = &self.password {
                    s.push(':');
                    s.push_str(p);
                }
                s.push('@');
            }
            s.push_str(h);
            if let Some(p) = self.port {
                s.push_str(&format!(":{p}"));
            }
        }
        s.push_str(&self.path);
        if let Some(q) = &self.query {
            s.push('?');
            s.push_str(q);
        }
        if let Some(f) = &self.fragment {
            s.push('#');
            s.push_str(f);
        }
        s
    }

    /// The uri with its host's A-labels: what a provider receives.
    pub fn ascii(&self) -> String {
        self.text_with(self.host_ascii().as_deref())
    }

    /// The query's pairs, each part unescaped: what `u.query` reads.
    pub fn query_pairs(&self) -> BTreeMap<String, String> {
        let unescape = |s: &str| {
            percent_encoding::percent_decode_str(s)
                .decode_utf8_lossy()
                .into_owned()
        };
        self.query
            .iter()
            .flat_map(|q| q.split('&'))
            .filter(|p| !p.is_empty())
            .map(|p| match p.split_once('=') {
                Some((k, v)) => (unescape(k), unescape(v)),
                None => (unescape(p), String::new()),
            })
            .collect()
    }

    /// Re-read after a part was set, so the result is normalized as a
    /// parsed one is (`with_*`); why not when the part does not fit.
    pub fn reparse(&self) -> Result<Uri, String> {
        Uri::parse(&self.to_string())
    }

    /// The uri with an authority: a rootless path takes a `/`, as RFC
    /// 3986 section 3.3 has a path after an authority begin (`with_host`
    /// on `mailto:ops@example.com`).
    pub fn with_authority(mut self) -> Uri {
        if !self.path.is_empty() && !self.path.starts_with('/') {
            self.path = format!("/{}", self.path);
        }
        self
    }
}

/// The text as written: its host's Unicode form.
impl std::fmt::Display for Uri {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.text_with(self.host.as_deref()))
    }
}

/// Equal by the A-label text: one host however its name is spelled.
impl PartialEq for Uri {
    fn eq(&self, other: &Uri) -> bool {
        self.ascii() == other.ascii()
    }
}

impl Eq for Uri {}

impl std::hash::Hash for Uri {
    fn hash<H: std::hash::Hasher>(&self, h: &mut H) {
        self.ascii().hash(h);
    }
}

impl PartialOrd for Uri {
    fn partial_cmp(&self, other: &Uri) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Uri {
    fn cmp(&self, other: &Uri) -> std::cmp::Ordering {
        self.ascii().cmp(&other.ascii())
    }
}

impl Serialize for Uri {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.to_string())
    }
}

impl<'de> Deserialize<'de> for Uri {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Uri, D::Error> {
        let s = String::deserialize(d)?;
        Uri::parse(&s).map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(s: &str) -> String {
        Uri::parse(s).unwrap_or_else(|e| panic!("{e}")).to_string()
    }

    /// The generic syntax: every scheme alike, userinfo kept, a path
    /// normalized only where it is hierarchical.
    #[test]
    fn a_uri_is_read_by_the_generic_syntax() {
        assert_eq!(text("HTTPS://Example.COM:443"), "https://example.com/");
        assert_eq!(text("http://h/a/./b/../c"), "http://h/a/c");
        assert_eq!(text("http://h/%7euser/%2f"), "http://h/~user/%2F");
        let u =
            Uri::parse("postgres://app:s3cr%3At@db.internal:5432/orders?sslmode=require").unwrap();
        assert_eq!(
            (u.user.as_deref(), u.password.as_deref(), u.port),
            (Some("app"), Some("s3cr%3At"), Some(5432))
        );
        assert_eq!(u.query_pairs()["sslmode"], "require");
        assert_eq!(text("s3://bucket/key/x.json"), "s3://bucket/key/x.json");
        assert_eq!(
            text("git+ssh://git@host/o/r.git"),
            "git+ssh://git@host/o/r.git"
        );
        assert_eq!(text("file:///etc/hosts"), "file:///etc/hosts");
        assert_eq!(text("oci://ghcr.io/o/app"), "oci://ghcr.io/o/app");
        assert_eq!(text("http://[::1]:8080/"), "http://[::1]:8080/");
    }

    /// The forms with no authority parse to a scheme and a path, and
    /// print back as written.
    #[test]
    fn a_form_with_no_authority_round_trips() {
        for s in [
            "tel:+15551234",
            "mailto:ops@example.com",
            "sip:alice@host",
            "urn:ietf:rfc:3986",
            "data:text/plain,hi",
        ] {
            let u = Uri::parse(s).unwrap();
            assert_eq!((u.host.as_deref(), u.to_string().as_str()), (None, s));
        }
        let u = Uri::parse("mailto:ops@example.com").unwrap();
        let mut h = u.with_authority();
        h.host = Some("mx.example".into());
        assert_eq!(
            h.reparse().unwrap().to_string(),
            "mailto://mx.example/ops@example.com"
        );
    }

    /// A host is held as written and equal by its A-labels.
    #[test]
    fn a_unicode_host_is_one_host_with_its_a_labels() {
        let a = Uri::parse("https://Bücher.example/x").unwrap();
        let b = Uri::parse("https://xn--bcher-kva.example/x").unwrap();
        assert_eq!(a.to_string(), "https://bücher.example/x");
        assert_eq!(a.ascii(), "https://xn--bcher-kva.example/x");
        assert_eq!(a, b);
        assert_eq!(b.to_string(), "https://xn--bcher-kva.example/x");
    }

    #[test]
    fn what_is_not_a_uri_says_why() {
        for (s, why) in [
            ("no scheme", "no scheme"),
            ("1x://h", "is no scheme"),
            ("http://h:99999/", "no port"),
            ("http://h/a b", "escapes"),
            ("http://h/%zz", "no escape"),
        ] {
            let e = Uri::parse(s).unwrap_err();
            assert!(e.contains(why), "{s}: {e}");
        }
    }
}
