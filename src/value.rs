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

impl Value {
    pub fn as_str(&self) -> Option<&str> {
        match self {
            Value::Str(s) => Some(s),
            _ => None,
        }
    }
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
