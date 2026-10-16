//! Values, documents and facts as the protocol's messages.
//!
//! A value maps one to one (`From<&Value> for pb::Value`, `TryFrom<&pb::Value>
//! for Value`), and a fact and a plan's change so (`TryFrom` both ways, a
//! change `From` to the wire). A document is JSON in
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
use std::collections::BTreeMap;

impl From<NullClass> for pb::NullClass {
    fn from(c: NullClass) -> pb::NullClass {
        match c {
            NullClass::Fresh => pb::NullClass::Fresh,
            NullClass::Open => pb::NullClass::Open,
            NullClass::Secret => pb::NullClass::Secret,
        }
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

impl From<&Value> for pb::Value {
    fn from(v: &Value) -> pb::Value {
        use pb::value::Kind;
        msg(match v {
            Value::Str(s) => Kind::Str(s.clone()),
            Value::Int(i) => Kind::Int(*i),
            Value::Float(f) => Kind::Float(f.get()),
            Value::Bool(b) => Kind::Bool(*b),
            Value::List(xs) => Kind::List(pb::List {
                items: xs.iter().map(pb::Value::from).collect(),
            }),
            Value::Obj(m) => Kind::Obj(pb::Obj {
                fields: m.iter().map(|(k, x)| (k.clone(), x.into())).collect(),
            }),
            Value::Ip(n) => Kind::Ip(*n),
            Value::IpNet { addr, prefix } => Kind::IpNet(pb::IpNet {
                addr: *addr,
                prefix: u32::from(*prefix),
            }),
            // A range of addresses is the wire's own; another range is its
            // canonical text, which the provider's schema reads (R-180).
            Value::Range(r) => match (&r.start, &r.end) {
                (Value::Ip(start), Value::Ip(end)) if r.inclusive => Kind::IpRange(pb::IpRange {
                    start: *start,
                    end: *end,
                }),
                _ => Kind::Str(r.to_string()),
            },
            Value::Ref { typ, name, attr } => Kind::Ref(pb_ref(typ, name, attr)),
            Value::CloudRef { typ, name, attr } => Kind::CloudRef(pb_ref(typ, name, attr)),
            // A provider reads a quantity, a time or a uri as its canonical
            // text, a uri's host in its A-labels (R-134); an attribute's
            // schema renders it before it gets there (`render`).
            Value::Quantity(_)
            | Value::Time(_)
            | Value::Uri(_)
            | Value::Oci(_)
            | Value::Semver(_) => Kind::Str(v.wire_text().unwrap_or_default()),
            Value::Null {
                label,
                class: c,
                ty,
            } => Kind::Null(pb::Null {
                label: label.clone(),
                class: pb::NullClass::from(*c) as i32,
                ty: ty.clone(),
                held: None,
            }),
        })
    }
}

/// A float as the engine holds it (R-75): NaN and the infinities are no
/// value.
fn float(f: f64) -> Result<crate::value::Float> {
    crate::value::Float::new(f).ok_or_else(|| anyhow!("{f} is not a finite number"))
}

impl TryFrom<&pb::Value> for Value {
    type Error = anyhow::Error;

    fn try_from(v: &pb::Value) -> Result<Value> {
        use pb::value::Kind;
        let Some(k) = &v.kind else {
            bail!("an empty value");
        };
        Ok(match k {
            Kind::Str(s) => Value::Str(s.clone()),
            Kind::Int(i) => Value::Int(*i),
            Kind::Bool(b) => Value::Bool(*b),
            Kind::List(l) => {
                Value::List(l.items.iter().map(Value::try_from).collect::<Result<_>>()?)
            }
            Kind::Obj(o) => Value::Obj(
                o.fields
                    .iter()
                    .map(|(k, x)| Ok((k.clone(), Value::try_from(x)?)))
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
            Kind::IpRange(r) => crate::range::Range {
                start: Value::Ip(r.start),
                end: Value::Ip(r.end),
                inclusive: true,
            }
            .into(),
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

/// A plan's change as the protocol's message: each side a document.
impl From<&provider::Change> for pb::Change {
    fn from(c: &provider::Change) -> pb::Change {
        pb::Change {
            path: c.path.clone(),
            before: c.before.as_ref().map(doc),
            after: c.after.as_ref().map(doc),
            sensitive: c.sensitive,
        }
    }
}

/// A plan's change back from its message.
impl TryFrom<&pb::Change> for provider::Change {
    type Error = anyhow::Error;

    fn try_from(c: &pb::Change) -> Result<provider::Change> {
        let side = |v: &Option<pb::Value>| v.as_ref().map(from_doc).transpose();
        Ok(provider::Change {
            path: c.path.clone(),
            before: side(&c.before)?,
            after: side(&c.after)?,
            sensitive: c.sensitive,
            note: None,
        })
    }
}

/// A provider's Plan as the protocol's answer.
impl From<&provider::Planned> for pb::PlanResponse {
    fn from(p: &provider::Planned) -> pb::PlanResponse {
        pb::PlanResponse {
            changes: p.changes.iter().map(pb::Change::from).collect(),
            requires_replace: p.replaces,
        }
    }
}

/// A provider's Plan back from its answer.
impl TryFrom<&pb::PlanResponse> for provider::Planned {
    type Error = anyhow::Error;

    fn try_from(r: &pb::PlanResponse) -> Result<provider::Planned> {
        Ok(provider::Planned {
            changes: r
                .changes
                .iter()
                .map(provider::Change::try_from)
                .collect::<Result<_>>()?,
            replaces: r.requires_replace,
        })
    }
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
            .map(pb::Fact::try_from)
            .collect(),
        None => schema.facts.iter().map(pb::Fact::try_from).collect(),
    }
}

impl TryFrom<&Atom> for pb::Fact {
    type Error = anyhow::Error;

    /// A fact as the protocol carries it: its arguments ground.
    fn try_from(a: &Atom) -> Result<pb::Fact> {
        let ground = |t: &Term| {
            t.ground()
                .ok_or_else(|| anyhow!("a fact's arguments are ground, found {t:?}"))
        };
        Ok(pb::Fact {
            pred: a.pred.clone(),
            args: a
                .args
                .iter()
                .map(|t| ground(t).map(|v| pb::Value::from(&v)))
                .collect::<Result<_>>()?,
        })
    }
}

impl TryFrom<&pb::Fact> for Atom {
    type Error = anyhow::Error;

    fn try_from(f: &pb::Fact) -> Result<Atom> {
        Ok(Atom {
            pred: f.pred.clone(),
            args: f
                .args
                .iter()
                .map(|v| Value::try_from(v).map(Term::Val))
                .collect::<Result<_>>()?,
            record: None,
            span: Default::default(),
        })
    }
}

/// A provider's types under another name (R-115): `use ovh as ca` serves
/// `ovh.instance` as `ca.instance`. Applied at the link only
/// ([`super::link::Link::rename`]), so the provider never learns of the
/// name: a call's types go out as `Rename::new("ca", "ovh")` renames
/// them, and its answer comes back through the [`Rename::inverse`]. A
/// type is renamed where the protocol carries one: a request's and an
/// answer's type, a Schema answer's facts, externs and examples, a typed
/// Query's type column, an extern's name, and a null's label (`T/N#P`)
/// and its holder's type. A value a program gives (a string that happens
/// to start with `ovh.`) never is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rename {
    from: String,
    to: String,
}

impl Rename {
    /// `from.X` is `to.X`.
    pub fn new(from: impl Into<String>, to: impl Into<String>) -> Rename {
        Rename {
            from: from.into(),
            to: to.into(),
        }
    }

    pub fn inverse(&self) -> Rename {
        Rename::new(self.to.clone(), self.from.clone())
    }

    /// A type or an extern's name: `from.X` as `to.X`, any other as it is.
    pub fn name(&self, s: &str) -> String {
        match s.strip_prefix(&self.from).and_then(|r| r.strip_prefix('.')) {
            Some(rest) => format!("{}.{rest}", self.to),
            None => s.to_string(),
        }
    }

    /// An attribute's type as a schema writes it (`ref(from.X)`,
    /// `list(ref(from.X))`): each type in it renamed.
    pub fn text(&self, s: &str) -> String {
        let mut out = String::new();
        let mut rest = s;
        let at_word = |out: &String| {
            out.chars()
                .last()
                .is_none_or(|c| !(c.is_alphanumeric() || matches!(c, '_' | '.' | '-')))
        };
        while let Some(i) = rest.find(&format!("{}.", self.from)) {
            out.push_str(&rest[..i]);
            if at_word(&out) {
                out.push_str(&self.to);
            } else {
                out.push_str(&self.from);
            }
            rest = &rest[i + self.from.len()..];
        }
        out.push_str(rest);
        out
    }

    /// A value: each null's label (`T/N#P`, `pred#P`) and holder's type,
    /// and each reference's type.
    pub fn value(&self, v: &mut pb::Value) {
        use pb::value::Kind;
        match &mut v.kind {
            Some(Kind::Null(n)) => {
                n.label = self.name(&n.label);
                if let Some(h) = &mut n.held {
                    h.r#type = self.name(&h.r#type);
                }
            }
            Some(Kind::Ref(r) | Kind::CloudRef(r)) => r.r#type = self.name(&r.r#type),
            Some(Kind::List(l)) => l.items.iter_mut().for_each(|x| self.value(x)),
            Some(Kind::Obj(o)) => o.fields.values_mut().for_each(|x| self.value(x)),
            _ => {}
        }
    }

    fn some(&self, v: &mut Option<pb::Value>) {
        if let Some(v) = v {
            self.value(v);
        }
    }

    /// Whether a Query of `pred` binds or answers a type in its first
    /// column: the inventory and `provider.created`.
    pub fn typed(pred: &str) -> bool {
        pred == super::providers::CREATED
            || super::providers::INVENTORY.iter().any(|(p, _)| *p == pred)
    }

    /// A call, its types renamed.
    pub fn call(&self, mut c: super::backend::Call) -> super::backend::Call {
        use super::backend::Call;
        match &mut c {
            Call::Schema(r) => {
                if let Some(t) = &mut r.types {
                    t.names = t.names.iter().map(|n| self.name(n)).collect();
                }
            }
            Call::Query(r) => {
                if Self::typed(&r.pred)
                    && let Some(pb::Value {
                        kind: Some(pb::value::Kind::Str(t)),
                    }) = r.inputs.first_mut()
                {
                    *t = self.name(t);
                }
                r.pred = self.name(&r.pred);
                r.inputs.iter_mut().for_each(|v| self.value(v));
            }
            Call::Read(r) => r.r#type = self.name(&r.r#type),
            Call::Plan(r) => {
                r.r#type = self.name(&r.r#type);
                self.some(&mut r.prior);
                self.some(&mut r.desired);
            }
            Call::Apply(r) => {
                r.r#type = self.name(&r.r#type);
                self.some(&mut r.config);
                r.assertions
                    .iter_mut()
                    .for_each(|a| self.some(&mut a.value));
            }
            Call::Import(r) => r.r#type = self.name(&r.r#type),
            Call::Reveal(r) => {
                if let Some(h) = &mut r.held {
                    h.r#type = self.name(&h.r#type);
                }
            }
            Call::Health(r) => r
                .objects
                .iter_mut()
                .for_each(|o| o.r#type = self.name(&o.r#type)),
            Call::Handshake(_) | Call::Configure(_) => {}
        }
        c
    }

    /// An answer, its types renamed; `typed`: the answer to a Query whose
    /// first column is a type ([`Rename::typed`]).
    pub fn reply(&self, mut r: super::backend::Reply, typed: bool) -> super::backend::Reply {
        use super::backend::Reply;
        match &mut r {
            Reply::Schema(s) => self.schema(s),
            Reply::Query(rows) => {
                for row in rows {
                    if typed
                        && let Some(pb::Value {
                            kind: Some(pb::value::Kind::Str(t)),
                        }) = row.values.first_mut()
                    {
                        *t = self.name(t);
                    }
                    row.values.iter_mut().for_each(|v| self.value(v));
                }
            }
            Reply::Read(x) => {
                self.some(&mut x.attrs);
                self.some(&mut x.computed);
            }
            Reply::Plan(x) => {
                for c in &mut x.changes {
                    self.some(&mut c.before);
                    self.some(&mut c.after);
                }
            }
            Reply::Apply(x) => {
                self.some(&mut x.attrs);
                self.some(&mut x.computed);
            }
            Reply::Import(x) => {
                x.r#type = self.name(&x.r#type);
                self.some(&mut x.attrs);
                self.some(&mut x.computed);
            }
            Reply::Handshake(_) | Reply::Configure(_) | Reply::Reveal(_) | Reply::Health(_) => {}
        }
        r
    }

    /// A Schema answer: each fact's type (its first column: the type, or
    /// an extern's name), an attribute's type (`ref(T)`), what a
    /// `type_alias` names; each extern's name and each example's type.
    pub fn schema(&self, s: &mut pb::SchemaResponse) {
        use pb::value::Kind;
        for f in &mut s.facts {
            let pred = f.pred.clone();
            for (i, a) in f.args.iter_mut().enumerate() {
                let Some(Kind::Str(t)) = &mut a.kind else {
                    continue;
                };
                *t = match (pred.as_str(), i) {
                    (_, 0) | ("type_alias", 1) => self.name(t),
                    ("type_attr", 2) => self.text(t),
                    _ => continue,
                };
            }
        }
        for e in &mut s.externs {
            e.pred = self.name(&e.pred);
        }
        for e in &mut s.examples {
            e.r#type = self.name(&e.r#type);
        }
    }
}

/// A provider's attribute names where the program's differ: a reference
/// is named for the resource (`vpc` for the provider's `vpc_id`,
/// `schema::reference_name`). Applied at the link only, as [`Rename`] is,
/// so the rest of dform knows the program's names and the provider its
/// own: a call goes out through `Names::new(&schema.wire)` and its answer
/// comes back through the [`Names::inverse`]. An attribute is renamed
/// where the protocol carries one: a document's keys, at any depth, a
/// change's path and values, an assertion's and a kept path, an inventory
/// row's path and value, and a null's label (`T/N#P`) and its holder's
/// path.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Names {
    /// type -> path -> the path it is renamed to; the two differ in their
    /// last key only.
    paths: BTreeMap<String, BTreeMap<String, String>>,
}

impl Names {
    /// `(type, path) -> path` renames, the program's to the provider's
    /// (`schema::Schema::wire`).
    pub fn new(wire: &BTreeMap<(String, String), String>) -> Names {
        let mut paths: BTreeMap<String, BTreeMap<String, String>> = BTreeMap::new();
        for ((t, from), to) in wire {
            paths
                .entry(t.clone())
                .or_default()
                .insert(from.clone(), to.clone());
        }
        Names { paths }
    }

    pub fn inverse(&self) -> Names {
        let paths = self
            .paths
            .iter()
            .map(|(t, m)| {
                (
                    t.clone(),
                    m.iter().map(|(a, b)| (b.clone(), a.clone())).collect(),
                )
            })
            .collect();
        Names { paths }
    }

    pub fn is_empty(&self) -> bool {
        self.paths.is_empty()
    }

    /// The key the last segment of `path` (keys only, no indices) of
    /// `typ` becomes, when it is renamed.
    fn key(&self, typ: &str, path: &str) -> Option<String> {
        let to = self.paths.get(typ)?.get(path)?;
        crate::address::path_keys(to).pop()
    }

    /// A path as the protocol prints it (`rules[0].subnet_id`), each
    /// segment renamed where its keys so far name a renamed attribute.
    pub fn path(&self, typ: &str, path: &str) -> String {
        if self.is_empty() || path.is_empty() {
            return path.to_string();
        }
        let mut at = String::new();
        let segs: Vec<String> = crate::address::path_segments(path)
            .into_iter()
            .map(|seg| {
                let (key, index) = crate::address::segment_parts(seg);
                at = crate::address::path_join(&at, &key);
                match self.key(typ, &at) {
                    Some(k) => format!("{}{index}", crate::address::path_key(&k)),
                    None => seg.to_string(),
                }
            })
            .collect();
        segs.join(".")
    }

    /// A path's keys, its indices dropped: where a value at it is.
    fn keys(path: &str) -> String {
        crate::address::path_keys(path)
            .iter()
            .fold(String::new(), |at, k| {
                let k = crate::address::segment_parts(k).0;
                crate::address::path_join(&at, &k)
            })
    }

    /// A null's label `T/N#P`, its path renamed by its type.
    fn label(&self, label: &str) -> String {
        match crate::value::null_parts(label) {
            Some((t, n, p)) => {
                let renamed = self.path(&t, &p);
                match renamed == p {
                    true => label.to_string(),
                    false => crate::value::null_label(&t, &n, &renamed),
                }
            }
            None => label.to_string(),
        }
    }

    /// A value at `at` (keys only) of a `typ` document: each object key
    /// whose path is renamed, and each null's label and holder's path.
    pub fn value(&self, typ: &str, at: &str, v: &mut pb::Value) {
        use pb::value::Kind;
        if self.is_empty() {
            return;
        }
        match &mut v.kind {
            Some(Kind::Null(n)) => {
                n.label = self.label(&n.label);
                if let Some(h) = &mut n.held {
                    h.path = self.path(&h.r#type, &h.path);
                }
            }
            Some(Kind::List(l)) => l.items.iter_mut().for_each(|x| self.value(typ, at, x)),
            Some(Kind::Obj(o)) => {
                o.fields = std::mem::take(&mut o.fields)
                    .into_iter()
                    .map(|(k, mut x)| {
                        let p = crate::address::path_join(at, &k);
                        self.value(typ, &p, &mut x);
                        (self.key(typ, &p).unwrap_or(k), x)
                    })
                    .collect();
            }
            _ => {}
        }
    }

    fn some(&self, typ: &str, at: &str, v: &mut Option<pb::Value>) {
        if let Some(v) = v {
            self.value(typ, at, v);
        }
    }

    /// What a call's answer is renamed by: the type it reads, plans or
    /// applies, or the inventory relation it asks for (its rows name
    /// their types).
    pub fn answered_by(c: &super::backend::Call) -> Option<String> {
        use super::backend::Call;
        match c {
            Call::Read(r) => Some(r.r#type.clone()),
            Call::Plan(r) => Some(r.r#type.clone()),
            Call::Apply(r) => Some(r.r#type.clone()),
            Call::Query(q) if Self::inventory(&q.pred) => Some(q.pred.clone()),
            _ => None,
        }
    }

    /// Whether `pred` is an inventory relation, `(Type, Name, Path,
    /// Value)`.
    fn inventory(pred: &str) -> bool {
        super::providers::INVENTORY
            .iter()
            .any(|(p, n)| *p == pred && *n == 4)
    }

    /// A call, its attribute names renamed.
    pub fn call(&self, mut c: super::backend::Call) -> super::backend::Call {
        use super::backend::Call;
        if self.is_empty() {
            return c;
        }
        match &mut c {
            Call::Plan(r) => {
                self.some(&r.r#type, "", &mut r.prior);
                self.some(&r.r#type, "", &mut r.desired);
            }
            Call::Apply(r) => {
                let typ = r.r#type.clone();
                self.some(&typ, "", &mut r.config);
                for a in &mut r.assertions {
                    a.path = self.path(&typ, &a.path);
                    self.some(&typ, &Self::keys(&a.path), &mut a.value);
                }
                for k in &mut r.keep {
                    *k = self.path(&typ, k);
                }
            }
            Call::Reveal(r) => {
                if let Some(h) = &mut r.held {
                    h.path = self.path(&h.r#type, &h.path);
                }
            }
            Call::Query(r) => r.inputs.iter_mut().for_each(|v| self.value("", "", v)),
            _ => {}
        }
        c
    }

    /// An answer, its attribute names renamed; `typ` what its call named
    /// ([`Names::answered_by`]).
    pub fn reply(&self, mut r: super::backend::Reply, typ: Option<&str>) -> super::backend::Reply {
        use super::backend::Reply;
        if self.is_empty() {
            return r;
        }
        let typ = typ.unwrap_or_default();
        match &mut r {
            Reply::Read(x) => {
                self.some(typ, "", &mut x.attrs);
                self.some(typ, "", &mut x.computed);
            }
            Reply::Apply(x) => {
                self.some(typ, "", &mut x.attrs);
                self.some(typ, "", &mut x.computed);
            }
            Reply::Import(x) => {
                let typ = x.r#type.clone();
                self.some(&typ, "", &mut x.attrs);
                self.some(&typ, "", &mut x.computed);
            }
            Reply::Plan(x) => {
                for c in &mut x.changes {
                    let at = Self::keys(&c.path);
                    self.some(typ, &at, &mut c.before);
                    self.some(typ, &at, &mut c.after);
                    c.path = self.path(typ, &c.path);
                }
            }
            Reply::Query(rows) if Self::inventory(typ) => {
                for row in rows {
                    self.row(&mut row.values);
                }
            }
            _ => {}
        }
        r
    }

    /// An inventory row, `(Type, Name, Path, Value)` (`cloud_attr`,
    /// `cloud_computed`), its path and value renamed by its type.
    fn row(&self, values: &mut [pb::Value]) {
        use pb::value::Kind;
        if let [
            pb::Value {
                kind: Some(Kind::Str(t)),
            },
            _,
            pb::Value {
                kind: Some(Kind::Str(p)),
            },
            v,
        ] = values
        {
            let at = Self::keys(p);
            self.value(t, &at, v);
            *p = self.path(t, p);
        }
    }
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
                (
                    "r".to_string(),
                    crate::range::Range::new(Value::Ip(1), Value::Ip(9), true)
                        .unwrap()
                        .into(),
                ),
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
        assert_eq!(Value::try_from(&pb::Value::from(&v)).unwrap(), v);
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
            let v = Value::try_from(&w).unwrap();
            assert_eq!(v, Value::Float(crate::value::Float::new(f).unwrap()));
            assert_eq!(Value::try_from(&pb::Value::from(&v)).unwrap(), v);
        }
        let nan = pb::Value {
            kind: Some(Kind::Float(f64::NAN)),
        };
        assert!(Value::try_from(&nan).is_err() && from_doc(&nan).is_err());
        assert_eq!(
            doc(&json!(u64::MAX)).kind,
            Some(Kind::Str(u64::MAX.to_string()))
        );
    }

    /// A type is renamed where the protocol carries one, and a string a
    /// program gives is not (R-115).
    #[test]
    fn a_rename_touches_types_only() {
        use super::super::backend::{Call, Reply};
        let out = Rename::new("ca", "ovh");
        assert_eq!(out.name("ca.instance"), "ovh.instance");
        assert_eq!(out.name("cat.instance"), "cat.instance");
        assert_eq!(
            out.name("ca.instance/x#password"),
            "ovh.instance/x#password"
        );
        assert_eq!(out.text("list(ref(ca.network))"), "list(ref(ovh.network))");
        assert_eq!(out.text("ref(orca.network)"), "ref(orca.network)");
        let req = pb::ApplyRequest {
            r#type: "ca.instance".into(),
            config: Some(doc(
                &json!({"host": "ca.example.com", "peer": {"$secret": "eu.vpc/p#id"}}),
            )),
            ..Default::default()
        };
        let Call::Apply(sent) = out.call(Call::Apply(req)) else {
            unreachable!()
        };
        assert_eq!(sent.r#type, "ovh.instance");
        let config = from_doc(sent.config.as_ref().unwrap()).unwrap();
        assert_eq!(config["host"], "ca.example.com");
        assert_eq!(config["peer"], json!({"$secret": "eu.vpc/p#id"}));
        let back = out.inverse();
        let rows = vec![pb::Row {
            values: vec![
                pb::Value::from(&Value::Str("ovh.instance".into())),
                pb::Value::from(&Value::Str("ovh.x".into())),
            ],
        }];
        let Reply::Query(rows) = back.reply(Reply::Query(rows), true) else {
            unreachable!()
        };
        assert_eq!(
            Value::try_from(&rows[0].values[0]).unwrap(),
            Value::Str("ca.instance".into())
        );
        assert_eq!(
            Value::try_from(&rows[0].values[1]).unwrap(),
            Value::Str("ovh.x".into())
        );
    }

    /// The provider is sent its own names of the attributes the program
    /// names for the resource, wherever the protocol carries one, and
    /// its answers come back under the program's; a key elsewhere that
    /// happens to be one is not renamed.
    #[test]
    fn names_rename_attributes_where_the_protocol_carries_them() {
        use super::super::backend::{Call, Reply};
        let wire: BTreeMap<(String, String), String> = [
            (("t.s".into(), "vpc".into()), "vpc_id".into()),
            (
                ("t.s".into(), "rules.subnet".into()),
                "rules.subnet_id".into(),
            ),
        ]
        .into();
        let out = Names::new(&wire);
        let req = pb::ApplyRequest {
            r#type: "t.s".into(),
            config: Some(doc(&json!({
                "vpc": "vpc-1",
                "tags": {"vpc": "x"},
                "rules": [{"subnet": "s-1", "port": 80}],
                "peer": {"$secret": "t.s/b#vpc"},
            }))),
            assertions: vec![pb::Assertion {
                path: "vpc".into(),
                ..Default::default()
            }],
            keep: vec!["rules[0].subnet".into()],
            ..Default::default()
        };
        let Call::Apply(sent) = out.call(Call::Apply(req)) else {
            unreachable!()
        };
        assert_eq!(
            from_doc(sent.config.as_ref().unwrap()).unwrap(),
            json!({
                "vpc_id": "vpc-1",
                "tags": {"vpc": "x"},
                "rules": [{"subnet_id": "s-1", "port": 80}],
                "peer": {"$secret": "t.s/b#vpc_id"},
            })
        );
        assert_eq!(sent.assertions[0].path, "vpc_id");
        assert_eq!(sent.keep, ["rules[0].subnet_id"]);
        let back = out.inverse();
        let plan = pb::PlanResponse {
            changes: vec![pb::Change {
                path: "rules[0].subnet_id".into(),
                ..Default::default()
            }],
            requires_replace: false,
        };
        let Reply::Plan(plan) = back.reply(Reply::Plan(plan), Some("t.s")) else {
            unreachable!()
        };
        assert_eq!(plan.changes[0].path, "rules[0].subnet");
        let read = pb::ReadResponse {
            found: true,
            attrs: Some(doc(&json!({"vpc_id": "vpc-1"}))),
            computed: None,
        };
        let Reply::Read(read) = back.reply(Reply::Read(read), Some("t.s")) else {
            unreachable!()
        };
        assert_eq!(
            from_doc(read.attrs.as_ref().unwrap()).unwrap(),
            json!({"vpc": "vpc-1"})
        );
        // Another type's document is its own.
        let other = pb::ReadResponse {
            found: true,
            attrs: Some(doc(&json!({"vpc_id": "vpc-1"}))),
            computed: None,
        };
        let Reply::Read(other) = back.reply(Reply::Read(other), Some("t.u")) else {
            unreachable!()
        };
        assert_eq!(
            from_doc(other.attrs.as_ref().unwrap()).unwrap(),
            json!({"vpc_id": "vpc-1"})
        );
    }
}
