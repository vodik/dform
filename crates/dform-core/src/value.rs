use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(tag = "t", content = "v")]
pub enum Value {
    Str(String),
    Int(i64),
    /// A decimal number (R-75): a decimal literal (`0.5`), a document's
    /// `1.5`, `float(x)`. Finite, so it is equal to, ordered and hashed
    /// by its value.
    Float(Float),
    Bool(bool),
    List(Vec<Value>),
    Obj(BTreeMap<String, Value>),
    Ip(u32),
    IpNet {
        addr: u32,
        prefix: u8,
    },
    IpRange {
        start: u32,
        end: u32,
    },
    /// A quantity (R-66): bytes, cpu or a duration, in its base unit;
    /// printed canonically (`1536Mi`, `500m`, `1h30m`).
    Quantity(crate::quantity::Quantity),
    /// A zoned instant (R-62), printed canonically
    /// (`2026-10-02T09:00:00+02:00[Europe/Paris]`).
    Time(crate::time::Time),
    /// A url (the url ticket): its canonical text, parsed at the edge
    /// (`url(s)`, a literal in a `url` position), so two spellings of one
    /// url are equal and a url never equals a string. `.scheme`, `.host`,
    /// `.port`, `.path`, `.query`, `.fragment` read its components.
    Url(String),
    /// A container image reference (R-133): its canonical text,
    /// `[registry/]repository[:tag][@digest]`, parsed at the edge
    /// (`oci(s)`, a literal in an `oci` position) as a url is, so two
    /// spellings of one reference are equal and an `oci` never equals a
    /// string. `.registry`, `.repository`, `.tag`, `.digest` read its
    /// parts ([`OciRef`]).
    Oci(String),
    Ref {
        typ: String,
        name: String,
        attr: String,
    },
    CloudRef {
        typ: String,
        name: String,
        attr: String,
    },
    /// A labeled null (proposal E). `label` is the Skolem name, conventionally
    /// "type/addr#attr"; `class` decides equality, materialization and phase;
    /// `ty` is the schema type the eventual constant will have.
    Null {
        label: String,
        class: NullClass,
        ty: String,
    },
}

/// A float's value: an `f64` that is finite (NaN and the infinities are
/// errors where a float is read, [`Float::new`]) and never `-0` (read as
/// `0`), so equality, order and hash are total and agree.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(try_from = "f64", into = "f64")]
pub struct Float(f64);

impl Float {
    /// `f` as a float; none for NaN or an infinity.
    pub fn new(f: f64) -> Option<Float> {
        f.is_finite()
            .then_some(Float(if f == 0.0 { 0.0 } else { f }))
    }

    pub fn get(self) -> f64 {
        self.0
    }

    /// `text` read as a float (`1.5`, `2`, `-0.25`, `1e-3`), or why it is
    /// not one: what `--set` and `float(s)` read.
    pub fn parse(text: &str) -> Result<Float, String> {
        let f: f64 = text
            .trim()
            .parse()
            .map_err(|_| format!("{text:?} is not a number"))?;
        Float::new(f).ok_or_else(|| format!("{text:?} is not a finite number"))
    }
}

impl TryFrom<f64> for Float {
    type Error = String;
    fn try_from(f: f64) -> Result<Float, String> {
        Float::new(f).ok_or_else(|| format!("{f} is not a finite number"))
    }
}

impl From<Float> for f64 {
    fn from(f: Float) -> f64 {
        f.0
    }
}

impl PartialEq for Float {
    fn eq(&self, other: &Float) -> bool {
        self.0.to_bits() == other.0.to_bits()
    }
}

impl Eq for Float {}

impl PartialOrd for Float {
    fn partial_cmp(&self, other: &Float) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Float {
    fn cmp(&self, other: &Float) -> std::cmp::Ordering {
        self.0.total_cmp(&other.0)
    }
}

impl std::hash::Hash for Float {
    fn hash<H: std::hash::Hasher>(&self, h: &mut H) {
        self.0.to_bits().hash(h);
    }
}

/// The shortest text that reads back as the same float, always with a
/// fraction so it reads back as a float: `1.5`, `2.0`, `0.001`.
impl std::fmt::Display for Float {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = self.0.to_string();
        if s.contains('.') {
            f.write_str(&s)
        } else {
            write!(f, "{s}.0")
        }
    }
}

/// The three null classes of proposal E (grafted from direction C). The class is
/// assigned by the schema, never written by the user.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum NullClass {
    /// A computed identity (id, arn, self_link). Unique Name Assumption: never
    /// equal to any constant or to a null with another label.
    Fresh,
    /// A computed non-identity (endpoint, allocated cidr, zones). Equality with
    /// anything but itself is unknown until resolved.
    Open,
    /// A sensitive computed value. As `Open`, and the constant is never given to
    /// the engine; it is materialized only inside a provider call.
    Secret,
}

impl NullClass {
    pub fn name(self) -> &'static str {
        match self {
            NullClass::Fresh => "fresh",
            NullClass::Open => "open",
            NullClass::Secret => "secret",
        }
    }
    pub fn parse(s: &str) -> Option<NullClass> {
        Some(match s {
            "fresh" => NullClass::Fresh,
            "open" => NullClass::Open,
            "secret" => NullClass::Secret,
            _ => return None,
        })
    }
}

/// The Skolem label of the null the prelude mints for `(T, A, P)`.
pub fn null_label(typ: &str, addr: &str, path: &str) -> String {
    format!("{typ}/{addr}#{path}")
}

/// The parts `(T, A, P)` of a null label `T/A#P`.
pub fn null_parts(label: &str) -> Option<(String, String, String)> {
    let (ta, p) = label.split_once('#')?;
    let (t, a) = ta.split_once('/')?;
    Some((t.to_string(), a.to_string(), p.to_string()))
}

/// The owner `(T, A)` of a null label `T/A#P`.
pub fn null_owner(label: &str) -> Option<(String, String)> {
    let (ta, _) = label.split_once('#')?;
    let (t, a) = ta.split_once('/')?;
    Some((t.to_string(), a.to_string()))
}

impl Value {
    pub fn as_str(&self) -> Option<&str> {
        match self {
            Value::Str(s) => Some(s),
            _ => None,
        }
    }

    /// The canonical text of a quantity, a time, a url or an image
    /// reference (`1536Mi`, `2026-10-02T09:00:00+02:00[Europe/Paris]`,
    /// `https://h/p`, `ghcr.io/o/app:1.2`): what `str()`,
    /// interpolation and the plan print where no schema renders it.
    pub fn typed_text(&self) -> Option<String> {
        match self {
            Value::Quantity(q) => Some(q.to_string()),
            Value::Time(t) => Some(t.to_string()),
            Value::Url(u) | Value::Oci(u) => Some(u.clone()),
            _ => None,
        }
    }
}

/// `text` read as a url: its canonical text, or why it is not one.
pub fn parse_url(text: &str) -> Result<Value, url::ParseError> {
    url::Url::parse(text).map(|u| Value::Url(u.to_string()))
}

/// A url's components as an object: `scheme`, `host`, `port` (absent: the
/// scheme's default), `path`, `query` (its pairs), `fragment` (absent:
/// none). What `.host` on a url and `url.parse` read; no value for a url
/// without a host (`mailto:x`).
pub fn url_parts(text: &str) -> Option<Value> {
    let u = url::Url::parse(text).ok()?;
    let mut m = BTreeMap::new();
    m.insert("scheme".to_string(), Value::Str(u.scheme().to_string()));
    m.insert("host".to_string(), Value::Str(u.host_str()?.to_string()));
    if let Some(port) = u.port() {
        m.insert("port".to_string(), Value::Int(i64::from(port)));
    }
    m.insert("path".to_string(), Value::Str(u.path().to_string()));
    let q = u
        .query_pairs()
        .map(|(k, v)| (k.into_owned(), Value::Str(v.into_owned())))
        .collect();
    m.insert("query".to_string(), Value::Obj(q));
    if let Some(f) = u.fragment() {
        m.insert("fragment".to_string(), Value::Str(f.to_string()));
    }
    Some(Value::Obj(m))
}

/// A container image reference: the OCI distribution reference grammar,
/// `[registry/]repository[:tag][@digest]`, normalized as Docker's
/// familiar names are. The registry is absent for the default registry
/// (`docker.io`, also written `index.docker.io`), whose one-component
/// repositories are `library/`'s (`nginx` is `library/nginx`); a
/// reference may carry both a tag and a digest (the digest decides what
/// is pulled, the tag says what it was).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OciRef {
    pub registry: Option<String>,
    pub repository: String,
    pub tag: Option<String>,
    pub digest: Option<String>,
}

/// What `oci(..)` and an `oci`-typed position say a reference is.
pub const OCI_GRAMMAR: &str = "`[registry/]repository[:tag][@digest]`";

impl OciRef {
    /// `text` read as a reference, or why it is not one.
    pub fn parse(text: &str) -> Result<OciRef, String> {
        let (rest, digest) = match text.split_once('@') {
            Some((a, d)) => {
                oci_digest(d)?;
                (a, Some(d.to_string()))
            }
            None => (text, None),
        };
        let last_slash = rest.rfind('/');
        let (name, tag) = match rest.rfind(':') {
            Some(ci) if last_slash.is_none_or(|si| ci > si) => {
                let tag = &rest[ci + 1..];
                oci_tag(tag)?;
                (&rest[..ci], Some(tag.to_string()))
            }
            _ => (rest, None),
        };
        if name.is_empty() {
            return Err("it has no repository".to_string());
        }
        let (registry, repository) = match name.split_once('/') {
            Some((head, tail)) if oci_is_registry(head) => {
                oci_registry(head)?;
                (Some(head.to_string()), tail)
            }
            _ => (None, name),
        };
        let mut r = OciRef {
            registry: None,
            repository: oci_repository(repository)?.to_string(),
            tag,
            digest,
        };
        r.set_registry(registry);
        Ok(r)
    }

    /// The registry set, the default one (`docker.io`) read as absent.
    pub fn set_registry(&mut self, registry: Option<String>) {
        self.registry = registry.filter(|r| !matches!(r.as_str(), "docker.io" | "index.docker.io"));
        if self.registry.is_none() && !self.repository.contains('/') {
            self.repository = format!("library/{}", self.repository);
        }
    }

    /// The reference as a value: its canonical text.
    pub fn value(&self) -> Value {
        Value::Oci(self.to_string())
    }

    /// Its parts as an object: `repository`, and `registry` (absent: the
    /// default registry), `tag` and `digest` where it has them. What
    /// `.digest` on a reference reads.
    pub fn parts(&self) -> Value {
        let mut m = BTreeMap::new();
        let mut put = |k: &str, v: &Option<String>| {
            if let Some(v) = v {
                m.insert(k.to_string(), Value::Str(v.clone()));
            }
        };
        put("registry", &self.registry);
        put("repository", &Some(self.repository.clone()));
        put("tag", &self.tag);
        put("digest", &self.digest);
        Value::Obj(m)
    }
}

/// The familiar form: the default registry left out, and with it
/// `library/` (`nginx:1.27`, `ghcr.io/o/app@sha256:..`).
impl std::fmt::Display for OciRef {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.registry {
            Some(r) => write!(f, "{r}/{}", self.repository)?,
            None => match self.repository.strip_prefix("library/") {
                Some(short) if !short.contains('/') => f.write_str(short)?,
                _ => f.write_str(&self.repository)?,
            },
        }
        if let Some(t) = &self.tag {
            write!(f, ":{t}")?;
        }
        if let Some(d) = &self.digest {
            write!(f, "@{d}")?;
        }
        Ok(())
    }
}

/// Whether a reference's first component names a registry rather than
/// a repository's: it has a `.` or a port, or is `localhost`.
fn oci_is_registry(s: &str) -> bool {
    s.contains('.') || s.contains(':') || s == "localhost"
}

/// A registry, `host[:port]`.
pub fn oci_registry(s: &str) -> Result<(), String> {
    let (host, port) = match s.rsplit_once(':') {
        Some((h, p)) => (h, Some(p)),
        None => (s, None),
    };
    let label = |l: &str| {
        !l.is_empty()
            && l.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
            && !l.starts_with('-')
            && !l.ends_with('-')
    };
    if host.is_empty() || !host.split('.').all(label) {
        return Err(format!("{s:?} is not a registry (`host[:port]`)"));
    }
    if port.is_some_and(|p| p.parse::<u16>().is_err()) {
        return Err(format!("{s:?} has no port after its `:`"));
    }
    Ok(())
}

/// A repository: `/`-separated components of lower-case letters and
/// digits, joined inside by `.`, `_`, `__` or dashes.
fn oci_repository(s: &str) -> Result<&str, String> {
    let component = |c: &str| {
        let b = c.as_bytes();
        let alnum = |x: &u8| x.is_ascii_lowercase() || x.is_ascii_digit();
        if b.is_empty() || !alnum(&b[0]) || !alnum(&b[b.len() - 1]) {
            return false;
        }
        let mut i = 0;
        while i < b.len() {
            if alnum(&b[i]) {
                i += 1;
                continue;
            }
            let start = i;
            while i < b.len() && !alnum(&b[i]) {
                i += 1;
            }
            let sep = &c[start..i];
            if !(sep == "." || sep == "_" || sep == "__" || sep.bytes().all(|x| x == b'-')) {
                return false;
            }
        }
        true
    };
    if s.split('/').all(component) {
        Ok(s)
    } else {
        Err(format!(
            "{s:?} is not a repository (lower-case letters and digits, \
             joined by `.`, `_`, `__`, `-` or `/`)"
        ))
    }
}

/// A tag: up to 128 letters, digits, `_`, `.` and `-`, not starting with
/// `.` or `-`.
pub fn oci_tag(s: &str) -> Result<(), String> {
    let ok = s.len() <= 128
        && s.chars()
            .next()
            .is_some_and(|c| c.is_ascii_alphanumeric() || c == '_')
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-'));
    ok.then_some(())
        .ok_or_else(|| format!("{s:?} is not a tag (letters, digits, `_`, `.`, `-`)"))
}

/// A digest, `algorithm:encoded`: `sha256` 64 lower-case hex digits,
/// `sha512` 128; another algorithm's encoding letters, digits, `=`,
/// `_`, `-`.
pub fn oci_digest(s: &str) -> Result<(), String> {
    let bad = || format!("{s:?} is not a digest (`sha256:` and 64 hex digits)");
    let (algo, enc) = s.split_once(':').ok_or_else(bad)?;
    let algo_ok = !algo.is_empty()
        && algo.split(['+', '.', '_', '-']).all(|p| {
            !p.is_empty()
                && p.chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
        });
    let hex = |n: usize| enc.len() == n && enc.chars().all(|c| matches!(c, '0'..='9' | 'a'..='f'));
    let enc_ok = match algo {
        "sha256" => hex(64),
        "sha512" => hex(128),
        _ => {
            !enc.is_empty()
                && enc
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '=' | '_' | '-'))
        }
    };
    (algo_ok && enc_ok).then_some(()).ok_or_else(bad)
}

/// `text` read as an image reference: its canonical text, or why it is
/// not one (the grammar named).
pub fn parse_oci(text: &str) -> Result<Value, String> {
    OciRef::parse(text)
        .map(|r| r.value())
        .map_err(|why| format!("{text:?} is not an image reference {OCI_GRAMMAR}: {why}"))
}

pub fn ipv4_to_u32(ip: &str) -> Option<u32> {
    let ip: std::net::Ipv4Addr = ip.parse().ok()?;
    Some(u32::from(ip))
}

pub fn u32_to_ipv4(v: u32) -> String {
    std::net::Ipv4Addr::from(v).to_string()
}

pub fn parse_ipnet(s: &str) -> Option<(u32, u8)> {
    let (ip, prefix) = s.split_once('/')?;
    let addr = ipv4_to_u32(ip)?;
    let prefix: u8 = prefix.parse().ok()?;
    if prefix > 32 {
        return None;
    }
    let mask = if prefix == 0 {
        0
    } else {
        u32::MAX << (32 - prefix as u32)
    };
    Some((addr & mask, prefix))
}

pub fn ipnet_to_string(addr: u32, prefix: u8) -> String {
    format!("{}/{}", u32_to_ipv4(addr), prefix)
}

/// How two numbers order by value (R-75), an int against a float exactly
/// (no rounding of a large int); none when either is not a number.
pub fn compare_numbers(a: &Value, b: &Value) -> Option<std::cmp::Ordering> {
    use std::cmp::Ordering;
    let int_float = |i: i64, f: f64| match (i as f64).total_cmp(&f) {
        // `f` is integral and within a rounding of `i`: compare as ints
        // (`2^63` itself is past every i64).
        Ordering::Equal if f >= i64::MAX as f64 => Ordering::Less,
        Ordering::Equal => i.cmp(&(f as i64)),
        o => o,
    };
    Some(match (a, b) {
        (Value::Int(x), Value::Int(y)) => x.cmp(y),
        (Value::Float(x), Value::Float(y)) => x.cmp(y),
        (Value::Int(x), Value::Float(y)) => int_float(*x, y.get()),
        (Value::Float(x), Value::Int(y)) => int_float(*y, x.get()).reverse(),
        _ => return None,
    })
}
