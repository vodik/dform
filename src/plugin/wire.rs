//! Values, documents and facts as the protocol's messages.
//!
//! A value maps one to one (`value`, `from_value`). A document is JSON in
//! the engine and the fake: its null and secret markers (`provider::marker`)
//! travel as `Null` messages, and a number that is not an `i64` or a JSON
//! `null` as a string, as `externs::from_json` reads JSON (the value model
//! has neither).

use super::pb;
use crate::ast::{Atom, Term};
use crate::provider;
use crate::value::{NullClass, Value};
use anyhow::{Result, anyhow, bail};
use serde_json::Value as Json;

pub fn class(c: NullClass) -> pb::NullClass {
    match c {
        NullClass::Fresh => pb::NullClass::Fresh,
        NullClass::Open => pb::NullClass::Open,
        NullClass::Secret => pb::NullClass::Secret,
    }
}

fn from_class(c: i32) -> Result<NullClass> {
    Ok(match pb::NullClass::try_from(c) {
        Ok(pb::NullClass::Fresh) => NullClass::Fresh,
        Ok(pb::NullClass::Open) => NullClass::Open,
        Ok(pb::NullClass::Secret) => NullClass::Secret,
        _ => bail!("a null without a class ({c})"),
    })
}

fn msg(k: pb::value::Kind) -> pb::Value {
    pb::Value { kind: Some(k) }
}

fn pb_ref(typ: &str, name: &str, attr: &str) -> pb::Ref {
    pb::Ref {
        r#type: typ.to_string(),
        name: name.to_string(),
        attr: attr.to_string(),
    }
}

pub fn value(v: &Value) -> pb::Value {
    use pb::value::Kind;
    msg(match v {
        Value::Str(s) => Kind::Str(s.clone()),
        Value::Int(i) => Kind::Int(*i),
        Value::Bool(b) => Kind::Bool(*b),
        Value::List(xs) => Kind::List(pb::List {
            items: xs.iter().map(value).collect(),
        }),
        Value::Obj(m) => Kind::Obj(pb::Obj {
            fields: m.iter().map(|(k, x)| (k.clone(), value(x))).collect(),
        }),
        Value::Ip(n) => Kind::Ip(*n),
        Value::IpNet { addr, prefix } => Kind::IpNet(pb::IpNet {
            addr: *addr,
            prefix: u32::from(*prefix),
        }),
        Value::IpRange { start, end } => Kind::IpRange(pb::IpRange {
            start: *start,
            end: *end,
        }),
        Value::Ref { typ, name, attr } => Kind::Ref(pb_ref(typ, name, attr)),
        Value::CloudRef { typ, name, attr } => Kind::CloudRef(pb_ref(typ, name, attr)),
        Value::Null {
            label,
            class: c,
            ty,
        } => Kind::Null(pb::Null {
            label: label.clone(),
            class: class(*c) as i32,
            ty: ty.clone(),
        }),
    })
}

pub fn from_value(v: &pb::Value) -> Result<Value> {
    use pb::value::Kind;
    let Some(k) = &v.kind else {
        bail!("an empty value");
    };
    Ok(match k {
        Kind::Str(s) => Value::Str(s.clone()),
        Kind::Int(i) => Value::Int(*i),
        Kind::Bool(b) => Value::Bool(*b),
        Kind::List(l) => Value::List(l.items.iter().map(from_value).collect::<Result<_>>()?),
        Kind::Obj(o) => Value::Obj(
            o.fields
                .iter()
                .map(|(k, x)| Ok((k.clone(), from_value(x)?)))
                .collect::<Result<_>>()?,
        ),
        Kind::Ip(n) => Value::Ip(*n),
        Kind::IpNet(n) => Value::IpNet {
            addr: n.addr,
            prefix: u8::try_from(n.prefix)
                .ok()
                .filter(|p| *p <= 32)
                .ok_or_else(|| anyhow!("an ip_net with prefix {}", n.prefix))?,
        },
        Kind::IpRange(r) => Value::IpRange {
            start: r.start,
            end: r.end,
        },
        Kind::Ref(r) => Value::Ref {
            typ: r.r#type.clone(),
            name: r.name.clone(),
            attr: r.attr.clone(),
        },
        Kind::CloudRef(r) => Value::CloudRef {
            typ: r.r#type.clone(),
            name: r.name.clone(),
            attr: r.attr.clone(),
        },
        Kind::Null(n) => Value::Null {
            label: n.label.clone(),
            class: from_class(n.class)?,
            ty: n.ty.clone(),
        },
    })
}

/// A document: markers as nulls (a `$null` one of class open: the class is
/// the schema's, and the receiver reads it from there).
pub fn doc(j: &Json) -> pb::Value {
    use pb::value::Kind;
    if let Some((key, label)) = provider::marker(j) {
        return msg(Kind::Null(pb::Null {
            label: label.to_string(),
            class: if key == provider::SECRET_KEY {
                pb::NullClass::Secret
            } else {
                pb::NullClass::Open
            } as i32,
            ty: String::new(),
        }));
    }
    msg(match j {
        Json::Null => Kind::Str("null".into()),
        Json::Bool(b) => Kind::Bool(*b),
        Json::Number(n) => match n.as_i64() {
            Some(i) => Kind::Int(i),
            None => Kind::Str(n.to_string()),
        },
        Json::String(s) => Kind::Str(s.clone()),
        Json::Array(xs) => Kind::List(pb::List {
            items: xs.iter().map(doc).collect(),
        }),
        Json::Object(m) => Kind::Obj(pb::Obj {
            fields: m.iter().map(|(k, x)| (k.clone(), doc(x))).collect(),
        }),
    })
}

/// A document back: a null as its marker, a secret one as `$secret`.
pub fn from_doc(v: &pb::Value) -> Result<Json> {
    use pb::value::Kind;
    let Some(k) = &v.kind else {
        bail!("an empty value");
    };
    Ok(match k {
        Kind::Str(s) => Json::String(s.clone()),
        Kind::Int(i) => Json::from(*i),
        Kind::Bool(b) => Json::Bool(*b),
        Kind::List(l) => Json::Array(l.items.iter().map(from_doc).collect::<Result<_>>()?),
        Kind::Obj(o) => Json::Object(
            o.fields
                .iter()
                .map(|(k, x)| Ok((k.clone(), from_doc(x)?)))
                .collect::<Result<_>>()?,
        ),
        Kind::Null(n) => match from_class(n.class)? {
            NullClass::Secret => provider::secret_json(&n.label),
            _ => provider::null_json(&n.label),
        },
        other => bail!("a document holds JSON values, not {other:?}"),
    })
}

/// An optional document field; absent is an empty object.
pub fn from_doc_or_empty(v: Option<&pb::Value>) -> Result<Json> {
    match v {
        Some(v) => from_doc(v),
        None => Ok(Json::Object(Default::default())),
    }
}

pub fn fact(a: &Atom) -> Result<pb::Fact> {
    fn ground(t: &Term) -> Result<Value> {
        match t {
            Term::Val(v) => Ok(v.clone()),
            Term::List(xs) => Ok(Value::List(xs.iter().map(ground).collect::<Result<_>>()?)),
            Term::Obj(m) => Ok(Value::Obj(
                m.iter()
                    .map(|(k, x)| Ok((k.clone(), ground(x)?)))
                    .collect::<Result<_>>()?,
            )),
            other => bail!("a fact's arguments are ground, found {other:?}"),
        }
    }
    Ok(pb::Fact {
        pred: a.pred.clone(),
        args: a
            .args
            .iter()
            .map(|t| ground(t).map(|v| value(&v)))
            .collect::<Result<_>>()?,
    })
}

pub fn from_fact(f: &pb::Fact) -> Result<Atom> {
    Ok(Atom {
        pred: f.pred.clone(),
        args: f
            .args
            .iter()
            .map(|v| from_value(v).map(Term::Val))
            .collect::<Result<_>>()?,
        record: None,
        span: Default::default(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn values_round_trip() {
        let v = Value::Obj(
            [
                ("s".to_string(), Value::Str("x".into())),
                ("n".to_string(), Value::Int(-3)),
                (
                    "net".to_string(),
                    Value::IpNet {
                        addr: 167772160,
                        prefix: 16,
                    },
                ),
                ("r".to_string(), Value::IpRange { start: 1, end: 9 }),
                (
                    "null".to_string(),
                    Value::Null {
                        label: "t/a#id".into(),
                        class: NullClass::Fresh,
                        ty: "string".into(),
                    },
                ),
                (
                    "ref".to_string(),
                    Value::List(vec![Value::Ref {
                        typ: "t".into(),
                        name: "a".into(),
                        attr: "id".into(),
                    }]),
                ),
            ]
            .into_iter()
            .collect(),
        );
        assert_eq!(from_value(&value(&v)).unwrap(), v);
    }

    #[test]
    fn documents_round_trip_with_their_markers() {
        let d = json!({"a": [1, true, "x"], "id": {"$null": "t/a#id"},
                       "pw": {"$secret": "t/a#pw"}, "tags": {}});
        assert_eq!(from_doc(&doc(&d)).unwrap(), d);
    }
}
