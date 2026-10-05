//! Values, documents and facts as the protocol's messages.
//!
//! A value maps one to one (`value`, `from_value`). A document is JSON in
//! the engine and the fake: its null and secret markers (`provider::marker`)
//! travel as `Null` messages, a float as `Float`, and a number beyond `i64`
//! or a JSON `null` as a string. The value model has no float: the engine
//! reads a `Float` as the string of its shortest round-trip decimal
//! ([`float`]), as `provider::json_to_value` reads JSON.

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
        Value::Float(f) => Kind::Float(f.get()),
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
        // A provider reads a quantity, a time or a url as its canonical
        // text; an attribute's schema renders it before it gets there
        // (`render`).
        Value::Quantity(_) | Value::Time(_) | Value::Url(_) => {
            Kind::Str(v.typed_text().unwrap_or_default())
        }
        Value::Null {
            label,
            class: c,
            ty,
        } => Kind::Null(pb::Null {
            label: label.clone(),
            class: class(*c) as i32,
            ty: ty.clone(),
            held: None,
        }),
    })
}

/// A float as the engine holds it (R-75): NaN and the infinities are no
/// value.
fn float(f: f64) -> Result<crate::value::Float> {
    crate::value::Float::new(f).ok_or_else(|| anyhow!("{f} is not a finite number"))
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
        Kind::Float(f) => Value::Float(float(*f)?),
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
            held: provider::held(j).map(|h| pb::Held {
                provider: h.provider,
                deployment: h.deployment,
                r#type: h.typ,
                remote: h.remote,
                path: h.path,
                digest: h.digest,
            }),
        }));
    }
    msg(match j {
        Json::Null => Kind::Str("null".into()),
        Json::Bool(b) => Kind::Bool(*b),
        Json::Number(n) => match (n.as_i64(), n.is_f64()) {
            (Some(i), _) => Kind::Int(i),
            (None, true) => Kind::Float(n.as_f64().unwrap_or_default()),
            (None, false) => Kind::Str(n.to_string()),
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
            NullClass::Secret => match &n.held {
                Some(h) => provider::held_json(
                    &n.label,
                    &provider::Held {
                        provider: h.provider.clone(),
                        deployment: h.deployment.clone(),
                        typ: h.r#type.clone(),
                        remote: h.remote.clone(),
                        path: h.path.clone(),
                        digest: h.digest.clone(),
                    },
                ),
                None => provider::secret_json(&n.label),
            },
            _ => provider::null_json(&n.label),
        },
        Kind::Float(f) => Json::from(f64::from(float(*f)?)),
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

/// A Schema answer's facts: those of the types the request names
/// ([`crate::schema::Schema::facts_for`]), or all of them.
pub fn schema_facts(
    schema: &crate::schema::Schema,
    req: &pb::SchemaRequest,
) -> Result<Vec<pb::Fact>> {
    match &req.types {
        Some(t) => schema
            .facts_for(&t.names.iter().cloned().collect())
            .iter()
            .map(fact)
            .collect(),
        None => schema.facts.iter().map(fact).collect(),
    }
}

pub fn fact(a: &Atom) -> Result<pb::Fact> {
    let ground = |t: &Term| {
        t.ground()
            .ok_or_else(|| anyhow!("a fact's arguments are ground, found {t:?}"))
    };
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
                       "pw": {"$secret": "t/a#pw"}, "tags": {},
                       "other": {"$secret": "stack_output/prod#pw", "held": {
                           "provider": "fakecloud", "deployment": "prod", "type": "t",
                           "remote": "a-1", "path": "pw", "digest": "hmac-sha256:00"}}});
        assert_eq!(from_doc(&doc(&d)).unwrap(), d);
    }

    /// A float crosses as `Float` and is the engine's float (R-75), back
    /// as the same JSON number; NaN is no value.
    #[test]
    fn a_float_crosses_as_a_float() {
        use pb::value::Kind;
        for j in [json!(0.1), json!(1e300), json!(-2.5)] {
            let w = doc(&j);
            let Some(Kind::Float(f)) = w.kind else {
                panic!("{j} crosses as {w:?}");
            };
            assert_eq!(from_doc(&w).unwrap(), j);
            let v = from_value(&w).unwrap();
            assert_eq!(v, Value::Float(crate::value::Float::new(f).unwrap()));
            assert_eq!(from_value(&value(&v)).unwrap(), v);
        }
        let nan = pb::Value {
            kind: Some(Kind::Float(f64::NAN)),
        };
        assert!(from_value(&nan).is_err() && from_doc(&nan).is_err());
        assert_eq!(
            doc(&json!(u64::MAX)).kind,
            Some(Kind::Str(u64::MAX.to_string()))
        );
    }
}
