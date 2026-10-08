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
    /// A range (R-180): `range(T)` for an ordered `T`, `0..=3`,
    /// `10.42.0.2..=10.42.0.254`, `1Gi..=500Gi`; `.start`, `.end` and a
    /// discrete one's `.len` read it ([`crate::range::Range`]).
    Range(Box<crate::range::Range>),
    /// A quantity (R-66): bytes, cpu or a duration, in its base unit;
    /// printed canonically (`1536Mi`, `500m`, `1h30m`).
    Quantity(crate::quantity::Quantity),
    /// A zoned instant (R-62), printed canonically
    /// (`2026-10-02T09:00:00+02:00[Europe/Paris]`).
    Time(crate::time::Time),
    /// A uri (R-134): RFC 3986's generic syntax, parsed at the edge (a
    /// string in a `uri` position) and normalized, so two spellings of one
    /// uri are equal, its host compared by its A-labels; a uri never
    /// equals a string. `.scheme`, `.user`, `.password`, `.host`, `.port`,
    /// `.path`, `.query`, `.fragment` read its parts ([`crate::uri::Uri`]).
    Uri(Box<crate::uri::Uri>),
    /// A container image reference (R-133): its canonical text,
    /// `[registry/]repository[:tag][@digest]`, parsed at the edge
    /// (a string in an `oci` position) as a uri is, so two
    /// spellings of one reference are equal and an `oci` never equals a
    /// string. `.registry`, `.repository`, `.tag`, `.digest` read its
    /// parts ([`OciRef`]).
    Oci(String),
    /// A semantic version (R-134), Cargo's syntax: ordered by precedence,
    /// so `a < b` compares versions; `.major`, `.minor`, `.patch`, `.pre`
    /// read its parts.
    Semver(Version),
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

/// The parts `(T, A, P)` of a null label `T/A#P`. `A` may hold a `#` (a
/// location's fragment, `vault://kv/app#key`); `P` never does.
pub fn null_parts(label: &str) -> Option<(String, String, String)> {
    let (ta, p) = label.rsplit_once('#')?;
    let (t, a) = ta.split_once('/')?;
    Some((t.to_string(), a.to_string(), p.to_string()))
}

/// The owner `(T, A)` of a null label `T/A#P`.
pub fn null_owner(label: &str) -> Option<(String, String)> {
    let (ta, _) = label.rsplit_once('#')?;
    let (t, a) = ta.split_once('/')?;
    Some((t.to_string(), a.to_string()))
}

impl Value {
    /// Whether `f` holds of a scalar inside `self` (anything but a list or
    /// an object, at any depth): the one walk the questions about a
    /// value's nulls and references ask.
    pub fn any_scalar(&self, f: &mut impl FnMut(&Value) -> bool) -> bool {
        match self {
            Value::List(xs) => xs.iter().any(|x| x.any_scalar(f)),
            Value::Obj(m) => m.values().any(|x| x.any_scalar(f)),
            v => f(v),
        }
    }

    /// `f` of every scalar inside `self`, depth first.
    pub fn for_each_scalar(&self, f: &mut impl FnMut(&Value)) {
        self.any_scalar(&mut |v| {
            f(v);
            false
        });
    }

    pub fn as_str(&self) -> Option<&str> {
        match self {
            Value::Str(s) => Some(s),
            _ => None,
        }
    }

    /// The canonical text of a quantity, a time, a uri or an image
    /// reference (`1536Mi`, `2026-10-02T09:00:00+02:00[Europe/Paris]`,
    /// `https://h/p`, `ghcr.io/o/app:1.2`): what `str()`,
    /// interpolation and the plan print where no schema renders it.
    pub fn typed_text(&self) -> Option<String> {
        match self {
            Value::Quantity(q) => Some(q.to_string()),
            Value::Time(t) => Some(t.to_string()),
            Value::Uri(u) => Some(u.to_string()),
            Value::Oci(u) => Some(u.clone()),
            Value::Semver(v) => Some(v.to_string()),
            Value::Range(r) => Some(r.to_string()),
            _ => None,
        }
    }

    /// [`Value::typed_text`] as a provider receives it: a uri's host in
    /// its A-labels (R-134: the provider boundary is IDNA's wire edge).
    pub fn wire_text(&self) -> Option<String> {
        match self {
            Value::Uri(u) => Some(u.ascii()),
            v => v.typed_text(),
        }
    }
}

/// A semantic version (`1.2.3-rc.1`), ordered by precedence.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Version(pub semver::Version);

impl Version {
    /// `text` read as a version, or why it is not one.
    pub fn parse(text: &str) -> Result<Version, String> {
        semver::Version::parse(text.trim())
            .map(Version)
            .map_err(|e| format!("{text:?} is not a semantic version ({e})"))
    }

    /// Its parts as an object: `major`, `minor`, `patch`, and `pre` where
    /// it has a pre-release tag.
    pub fn parts(&self) -> Value {
        let v = &self.0;
        let mut m = BTreeMap::new();
        m.insert("major".to_string(), Value::Int(v.major as i64));
        m.insert("minor".to_string(), Value::Int(v.minor as i64));
        m.insert("patch".to_string(), Value::Int(v.patch as i64));
        if !v.pre.is_empty() {
            m.insert("pre".to_string(), Value::Str(v.pre.to_string()));
        }
        Value::Obj(m)
    }
}

impl std::fmt::Display for Version {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

impl Serialize for Version {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.0.to_string())
    }
}

impl<'de> Deserialize<'de> for Version {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Version, D::Error> {
        let s = String::deserialize(d)?;
        Version::parse(&s).map_err(serde::de::Error::custom)
    }
}

/// The value types a string is read as where one is wanted (R-31, R-134):
/// a typed `let`, a parameter, an attribute, an input.
pub const VALUE_TYPES: &[&str] = &[
    "inet", "ip", "uri", "oci", "semver", "time", "bytes", "cpu", "duration",
];

/// Whether `ty` is a value type: one of [`VALUE_TYPES`], or `range(T)`
/// for an ordered `T` (R-180).
pub fn is_value_type(ty: &str) -> bool {
    VALUE_TYPES.contains(&ty) || crate::range::element(ty).is_some()
}

/// The type a value is of, as a program writes it: `string`, `inet`,
/// `bytes` (a quantity by its dimension), `list`.
pub fn type_name(v: &Value) -> &'static str {
    match v {
        Value::Str(_) => "string",
        Value::Int(_) => "int",
        Value::Float(_) => "float",
        Value::Bool(_) => "bool",
        Value::List(_) => "list",
        Value::Obj(_) => "object",
        Value::Ip(_) => "ip",
        Value::IpNet { .. } => "inet",
        Value::Range(_) => "range",
        Value::Quantity(q) => q.dim().name(),
        Value::Time(_) => "time",
        Value::Uri(_) => "uri",
        Value::Oci(_) => "oci",
        Value::Semver(_) => "semver",
        Value::Ref { .. } | Value::CloudRef { .. } => "ref",
        Value::Null { .. } => "null",
    }
}

/// `a string`, `an inet`: a type name with its article.
pub fn article(ty: &str) -> String {
    match ty.chars().next() {
        Some('a' | 'e' | 'i' | 'o' | 'u') => format!("an {ty}"),
        _ => format!("a {ty}"),
    }
}

/// `v` read as the value type `ty` ([`is_value_type`]): a string
/// parsed, a value of the type itself; or why it is not one. What a
/// typed position does with a computed string at run time.
pub fn read_typed(ty: &str, v: &Value) -> Result<Value, String> {
    use crate::quantity::{Dim, Quantity};
    let dim = match ty {
        "bytes" => Some(Dim::Bytes),
        "cpu" => Some(Dim::Cpu),
        "duration" => Some(Dim::Duration),
        _ => None,
    };
    match (ty, v) {
        ("inet", Value::IpNet { .. })
        | ("ip", Value::Ip(_))
        | ("uri", Value::Uri(_))
        | ("oci", Value::Oci(_))
        | ("semver", Value::Semver(_))
        | ("time", Value::Time(_)) => Ok(v.clone()),
        (_, Value::Quantity(q)) if Some(q.dim()) == dim => Ok(v.clone()),
        ("inet", Value::Str(s)) => parse_ipnet(s)
            .map(|(addr, prefix)| Value::IpNet { addr, prefix })
            .ok_or_else(|| format!("{s:?} is not a network (`a.b.c.d/n`)")),
        ("ip", Value::Str(s)) => ipv4_to_u32(s)
            .map(Value::Ip)
            .ok_or_else(|| format!("{s:?} is not an address (`a.b.c.d`)")),
        (ty, Value::Range(r)) if crate::range::element(ty).is_some() => {
            let elem = crate::range::element(ty).unwrap_or_default();
            r.read(elem).map(Value::from)
        }
        (ty, Value::Str(s)) if crate::range::element(ty).is_some() => {
            crate::range::Range::parse(crate::range::element(ty).unwrap_or_default(), s)
                .map(Value::from)
        }
        ("uri", Value::Str(s)) => parse_uri(s),
        ("oci", Value::Str(s)) => parse_oci(s),
        ("semver", Value::Str(s)) => Version::parse(s).map(Value::Semver),
        ("time", Value::Str(s)) => crate::time::Time::parse(s).map(Value::Time),
        // A number from its text, or an int widened (R-155: there is no
        // `int(s)` or `float(i)`; a typed position reads one).
        ("int", Value::Int(_)) | ("float", Value::Float(_)) => Ok(v.clone()),
        ("int", Value::Str(s)) => s
            .trim()
            .parse()
            .map(Value::Int)
            .map_err(|_| format!("{s:?} is not an int")),
        ("float", Value::Int(i)) => Float::new(*i as f64)
            .map(Value::Float)
            .ok_or_else(|| format!("{i} is not a float")),
        ("float", Value::Str(s)) => Float::parse(s).map(Value::Float),
        (_, Value::Str(s)) if dim.is_some() => {
            crate::quantity::read(dim.unwrap_or(Dim::Bytes), s).map(Value::Quantity)
        }
        // An integer is bytes or cores; a decimal cores (`0.5` is `500m`).
        ("bytes", Value::Int(n)) => Ok(Value::Quantity(Quantity::Bytes(*n))),
        ("cpu", Value::Int(n)) => n
            .checked_mul(1000)
            .map(|m| Value::Quantity(Quantity::Cpu(m)))
            .ok_or_else(|| format!("{n} cores is past the largest cpu")),
        ("cpu", Value::Float(f)) => {
            crate::quantity::read(Dim::Cpu, &f.to_string()).map(Value::Quantity)
        }
        _ => Err(format!("{} is not one", crate::partition::fmt_value(v))),
    }
}

/// The parts of a network, a version or a range as an object: what
/// `.bits` on an `inet`, `.major` on a `semver` and `.start` on a range
/// read (R-134, R-180).
pub fn parts(v: &Value) -> Option<Value> {
    match v {
        Value::IpNet { addr, prefix } => Some(Value::Obj(BTreeMap::from([
            ("addr".to_string(), Value::Ip(*addr)),
            ("bits".to_string(), Value::Int(i64::from(*prefix))),
        ]))),
        Value::Semver(s) => Some(s.parts()),
        Value::Uri(u) => Some(uri_parts(u)),
        Value::Range(r) => Some(r.parts()),
        _ => None,
    }
}

/// `text` read as a uri, or why it is not one.
pub fn parse_uri(text: &str) -> Result<Value, String> {
    crate::uri::Uri::parse(text).map(|u| Value::Uri(Box::new(u)))
}

/// A uri's parts as an object: `scheme`, `path`, `query` (its pairs, each
/// unescaped), and `user`, `password`, `host`, `port` (absent: the
/// scheme's default) and `fragment` where it has them. What `.host` on a
/// uri reads (R-134).
pub fn uri_parts(u: &crate::uri::Uri) -> Value {
    let mut m = BTreeMap::new();
    let mut put = |k: &str, v: Option<Value>| {
        if let Some(v) = v {
            m.insert(k.to_string(), v);
        }
    };
    put("scheme", Some(Value::Str(u.scheme.clone())));
    put("user", u.user.clone().map(Value::Str));
    put("password", u.password.clone().map(Value::Str));
    put("host", u.host.clone().map(Value::Str));
    put("port", u.port.map(|p| Value::Int(i64::from(p))));
    put("path", Some(Value::Str(u.path.clone())));
    let q = u
        .query_pairs()
        .into_iter()
        .map(|(k, v)| (k, Value::Str(v)))
        .collect();
    put("query", Some(Value::Obj(q)));
    put("fragment", u.fragment.clone().map(Value::Str));
    Value::Obj(m)
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

/// What an `oci`-typed position says a reference is.
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

#[cfg(test)]
mod tests {
    use super::*;

    /// `any_scalar` looks through lists and objects at any depth and
    /// stops at the first scalar that holds; `for_each_scalar` visits
    /// every scalar in order.
    #[test]
    fn a_walk_reaches_every_scalar_inside_lists_and_objects() {
        let null = Value::Null {
            label: "t/a#p".into(),
            class: NullClass::Open,
            ty: String::new(),
        };
        let v = Value::List(vec![
            Value::Int(1),
            Value::Obj([("k".to_string(), Value::List(vec![null.clone()]))].into()),
        ]);
        assert!(v.any_scalar(&mut |x| matches!(x, Value::Null { .. })));
        assert!(!v.any_scalar(&mut |x| matches!(x, Value::Str(_))));
        let mut seen = Vec::new();
        v.for_each_scalar(&mut |x| seen.push(x.clone()));
        assert_eq!(seen, [Value::Int(1), null]);
        let mut asked = 0;
        assert!(v.any_scalar(&mut |_| {
            asked += 1;
            true
        }));
        assert_eq!(asked, 1, "the walk stops at the first that holds");
    }
}
