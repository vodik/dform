use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(tag = "t", content = "v")]
pub enum Value {
    Str(String),
    Int(i64),
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
