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

    /// The canonical text of a quantity, a time or a url (`1536Mi`,
    /// `2026-10-02T09:00:00+02:00[Europe/Paris]`, `https://h/p`): what `str()`,
    /// interpolation and the plan print where no schema renders it.
    pub fn typed_text(&self) -> Option<String> {
        match self {
            Value::Quantity(q) => Some(q.to_string()),
            Value::Time(t) => Some(t.to_string()),
            Value::Url(u) => Some(u.clone()),
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
