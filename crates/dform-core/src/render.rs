//! `dform render` (R-202): a deployment's planned documents as the objects
//! their provider sends its API, for a tool that applies them itself
//! (Argo CD's config management plugin, kustomize, `kubectl apply -f -`).
//! This is the part every provider's form shares: a resource's document
//! with every value resolved ([`Documents::wire`]), the cells it cannot
//! fill ([`Hole`]), the order a consumer applies them in ([`order`]) and
//! the YAML stream ([`yaml_stream`]). What a form adds to the document
//! (Kubernetes's `apiVersion` and `kind`) is its caller's.

use crate::ir::{self, Address, Resource};
use crate::query::Redactor;
use crate::report;
use crate::schema::Schema;
use crate::value::Value;
use anyhow::Result;
use serde_json::Value as Json;
use std::collections::{BTreeMap, BTreeSet};

/// A cell a render cannot fill: a value only an apply makes (a null, a
/// reference to one), or a secret, which a render never prints: it holds
/// no master, and what it prints is a manifest anyone may read.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct Hole {
    pub addr: Address,
    /// The cell's path in the document (`spec.template.spec.containers[0].image`).
    pub path: String,
    pub why: Why,
}

/// Why a render cannot fill a cell.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum Why {
    /// A value an apply makes, as the plan names it
    /// (`k8s.config_map web_config.metadata.name`).
    Applied(String),
    /// A value read from the cloud when the plan is made.
    Read(String),
    /// A secret, with the call that derives it when one does
    /// (`random.password("db")`).
    Secret(Option<String>),
}

impl std::fmt::Display for Hole {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let cell = format!("{} {}", report::address(&self.addr), self.path);
        match &self.why {
            Why::Applied(v) => write!(f, "{cell} = {v}, known after apply"),
            Why::Read(v) => write!(f, "{cell} = {v}, read from the cloud by a plan"),
            Why::Secret(Some(call)) => write!(f, "{cell} = {call}, a secret: a render prints none"),
            Why::Secret(None) => write!(f, "{cell} is a secret: a render prints none"),
        }
    }
}

/// A resource's document as a render has it: every value known, or the
/// cells that are not.
#[derive(Debug, Clone, PartialEq)]
pub enum Wire {
    Known(Json),
    Holes(Vec<Hole>),
}

/// How deep a reference may lead through others before it is taken as a
/// cycle (which a program's documents cannot hold).
const DEPTH: usize = 32;

/// The documents of one evaluation, by address: what a reference between
/// them reads; and what of their values is secret.
pub struct Documents<'a> {
    by_addr: BTreeMap<&'a Address, &'a Value>,
    redact: &'a Redactor,
}

impl<'a> Documents<'a> {
    pub fn new(resources: &'a [Resource], redact: &'a Redactor) -> Documents<'a> {
        Documents {
            by_addr: resources.iter().map(|r| (&r.addr, &r.attrs)).collect(),
            redact,
        }
    }

    /// `r`'s document as its provider takes it: quantities and times in
    /// the schema's form (R-66), a reference to what another document
    /// sets resolved to that value, every other value as it is sent
    /// (`Value::wire_text`). `Err` at a value the schema's form cannot
    /// hold, as the plan refuses it.
    pub fn wire(&self, r: &Resource, schema: &Schema) -> Result<Wire> {
        let attrs = schema
            .render(&r.addr.typ, &r.attrs)
            .map_err(|(path, why)| report::Failure::located(&r.addr, format!("{path} {why}")))?;
        let mut holes = Vec::new();
        let json = self.json(&r.addr, "", &attrs, &mut holes, 0);
        Ok(match holes.is_empty() {
            true => Wire::Known(json),
            false => Wire::Holes(holes),
        })
    }

    fn json(
        &self,
        addr: &Address,
        path: &str,
        v: &Value,
        holes: &mut Vec<Hole>,
        depth: usize,
    ) -> Json {
        let mut hole = |why: Why| {
            holes.push(Hole {
                addr: addr.clone(),
                path: path.to_string(),
                why,
            });
            Json::Null
        };
        if self.redact.is_secret(v) {
            return hole(Why::Secret(self.redact.derived(v).map(str::to_string)));
        }
        // A template over a secret a provider holds is its text with a
        // placeholder where the secret goes (R-218): a secret as a whole.
        if let Value::Str(s) = v
            && crate::secrets::held::carries(s)
        {
            return hole(Why::Secret(None));
        }
        match v {
            Value::Str(s) => Json::String(s.clone()),
            Value::Int(i) => Json::from(*i),
            Value::Float(f) => {
                serde_json::Number::from_f64(f.get()).map_or(Json::Null, Json::Number)
            }
            Value::Bool(b) => Json::Bool(*b),
            Value::List(xs) => Json::Array(
                xs.iter()
                    .enumerate()
                    .map(|(i, x)| self.json(addr, &format!("{path}[{i}]"), x, holes, depth))
                    .collect(),
            ),
            Value::Obj(m) => Json::Object(
                m.iter()
                    .map(|(k, x)| {
                        let at = ir::path_join(path, k);
                        (k.clone(), self.json(addr, &at, x, holes, depth))
                    })
                    .collect(),
            ),
            Value::Ip(n) => Json::String(crate::value::u32_to_ipv4(*n)),
            Value::IpNet { addr: a, prefix } => {
                Json::String(crate::value::ipnet_to_string(*a, *prefix))
            }
            Value::Ref { typ, name, attr } => {
                let to = Address {
                    typ: typ.clone(),
                    name: name.clone(),
                };
                match self.at(&to, attr).filter(|_| depth < DEPTH) {
                    Some(x) => self.json(addr, path, x, holes, depth + 1),
                    None => hole(Why::Applied(cell(&to, attr))),
                }
            }
            Value::CloudRef { typ, name, attr } => {
                let to = Address {
                    typ: typ.clone(),
                    name: name.clone(),
                };
                hole(Why::Read(cell(&to, attr)))
            }
            Value::Null { label, .. } => hole(Why::Applied(named(label))),
            Value::Range(_)
            | Value::Quantity(_)
            | Value::Time(_)
            | Value::Uri(_)
            | Value::Oci(_)
            | Value::Semver(_) => Json::String(v.wire_text().unwrap_or_default()),
        }
    }

    /// The value the document of `addr` sets at `path`, if it is one of
    /// these documents and sets it.
    fn at(&self, addr: &Address, path: &str) -> Option<&'a Value> {
        let mut cur = *self.by_addr.get(addr)?;
        for seg in ir::path_segments(path) {
            let (key, mut rest) = ir::segment_parts(seg);
            if !key.is_empty() {
                cur = match cur {
                    Value::Obj(m) => m.get(key.as_ref())?,
                    _ => return None,
                };
            }
            while let Some(r) = rest.strip_prefix('[') {
                let (idx, tail) = r.split_once(']')?;
                cur = match cur {
                    Value::List(xs) => xs.get(idx.parse::<usize>().ok()?)?,
                    _ => return None,
                };
                rest = tail;
            }
            if !rest.is_empty() {
                return None;
            }
        }
        Some(cur)
    }
}

/// The cell `path` of `addr`, as the plan prints it:
/// `k8s.config_map web_config.metadata.name`.
fn cell(addr: &Address, path: &str) -> String {
    format!("{}.{path}", report::address(addr))
}

/// The value a null label stands for, as the plan prints it: a
/// resource's cell, else the label's own spelling (another stack's
/// output).
fn named(label: &str) -> String {
    match crate::value::null_parts(label) {
        Some((typ, name, path))
            if !name.is_empty()
                && typ != crate::transform::OUTPUT
                && typ != crate::stack::UNAPPLIED =>
        {
            cell(&Address { typ, name }, &path)
        }
        _ => ir::label(label),
    }
}

/// What an evaluation leaves undecided that a render must know, a line
/// each: a deny that reads a value only an apply makes (`deny "no" reads
/// db.postgres d.endpoint, known after apply`), and whether a resource
/// is made, when that does (`whether k8s.namespace ns-a is made reads
/// ..`); of the resources, those of a type `renders` says a render
/// prints.
pub fn undecided(res: &crate::engine::EvalResult, renders: impl Fn(&str) -> bool) -> Vec<String> {
    use crate::ast::Term;
    let reads = |nulls: &BTreeSet<String>| {
        let named: Vec<String> = nulls.iter().map(|n| named(n)).collect();
        format!("reads {}, known after apply", named.join(", "))
    };
    let str_arg = |a: &crate::ast::Atom, i: usize| match a.args.get(i) {
        Some(Term::Val(Value::Str(s))) => Some(s.clone()),
        _ => None,
    };
    let made = |head: &crate::ast::Atom, nulls: &BTreeSet<String>| {
        let typ = str_arg(head, 0);
        if typ.as_deref().is_some_and(|t| !renders(t)) {
            return None;
        }
        let what = match (typ, str_arg(head, 1)) {
            (Some(typ), Some(name)) => report::address(&Address { typ, name }),
            (Some(typ), None) => format!("{typ}[?]"),
            (None, _) => "a resource".to_string(),
        };
        Some(format!("whether {what} is made {}", reads(nulls)))
    };
    let mut out = Vec::new();
    for s in &res.stuck {
        match s.head.pred.as_str() {
            "deny" => {
                let msg = str_arg(&s.head, 0).unwrap_or_else(|| crate::spell::atom(&s.head));
                out.push(format!(
                    "deny {} {}",
                    crate::spell::quote(&msg),
                    reads(&s.nulls)
                ));
            }
            "want" => out.extend(made(&s.head, &s.nulls)),
            _ => {}
        }
    }
    for m in res.may_derive.iter().filter(|m| m.head.pred == "want") {
        out.extend(made(&m.head, &m.nulls));
    }
    out.sort();
    out.dedup();
    out
}

/// `resources` in the order their plan applies them, which a consumer
/// applies a stream in: rounds in address order, each taking every
/// resource whose dependencies (`deps`, and the `extra` edges a
/// provider's form knows: a CRD before the objects of its kinds) come
/// before it. What a cycle leaves follows in address order.
pub fn order<'a>(
    resources: &[&'a Resource],
    extra: &BTreeMap<Address, BTreeSet<Address>>,
) -> Vec<&'a Resource> {
    let by_addr: BTreeMap<&Address, &'a Resource> =
        resources.iter().map(|r| (&r.addr, *r)).collect();
    let deps = |r: &Resource| -> Vec<Address> {
        r.deps
            .iter()
            .chain(extra.get(&r.addr).into_iter().flatten())
            .filter(|d| **d != r.addr && by_addr.contains_key(d))
            .cloned()
            .collect()
    };
    let mut pending: Vec<&Address> = by_addr.keys().copied().collect();
    let mut done: BTreeSet<&Address> = BTreeSet::new();
    let mut out = Vec::new();
    while !pending.is_empty() {
        let before = pending.len();
        pending.retain(|a| {
            let r = by_addr[a];
            let ready = deps(r).iter().all(|d| done.contains(d));
            if ready {
                done.insert(a);
                out.push(r);
            }
            !ready
        });
        if pending.len() == before {
            out.extend(pending.iter().map(|a| by_addr[a]));
            break;
        }
    }
    out
}

/// `objects` as a YAML stream, each after a `---` line, its keys `lead`
/// first in that order (a Kubernetes object's `apiVersion`, `kind`,
/// `metadata`), then the rest as they are.
pub fn yaml_stream(objects: &[Json], lead: &[&str]) -> Result<String> {
    let mut out = String::new();
    for o in objects {
        let doc = match o {
            Json::Object(m) => {
                let mut map = serde_yaml::Mapping::new();
                let keys = lead
                    .iter()
                    .filter(|k| m.contains_key(**k))
                    .copied()
                    .chain(m.keys().map(String::as_str).filter(|k| !lead.contains(k)));
                for k in keys {
                    map.insert(k.into(), serde_yaml::to_value(&m[k])?);
                }
                serde_yaml::Value::Mapping(map)
            }
            other => serde_yaml::to_value(other)?,
        };
        out.push_str("---\n");
        out.push_str(&serde_yaml::to_string(&doc)?);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn resource(typ: &str, name: &str, attrs: Value, deps: &[(&str, &str)]) -> Resource {
        Resource {
            addr: Address {
                typ: typ.into(),
                name: name.into(),
            },
            attrs,
            deps: deps
                .iter()
                .map(|(t, n)| Address {
                    typ: (*t).into(),
                    name: (*n).into(),
                })
                .collect(),
        }
    }

    fn obj(pairs: &[(&str, Value)]) -> Value {
        Value::Obj(
            pairs
                .iter()
                .map(|(k, v)| (k.to_string(), v.clone()))
                .collect(),
        )
    }

    /// A reference to what another document sets is that value; one to
    /// what only an apply makes is a hole, named as the plan names it,
    /// at the cell's path.
    #[test]
    fn a_reference_resolves_or_is_a_hole() {
        let ns = resource(
            "k8s.namespace",
            "shop",
            obj(&[("metadata", obj(&[("name", Value::Str("shop".into()))]))]),
            &[],
        );
        let reference = |typ: &str, name: &str, attr: &str| Value::Ref {
            typ: typ.into(),
            name: name.into(),
            attr: attr.into(),
        };
        let cm = resource(
            "k8s.config_map",
            "cfg",
            obj(&[
                (
                    "metadata",
                    obj(&[(
                        "namespace",
                        reference("k8s.namespace", "shop", "metadata.name"),
                    )]),
                ),
                (
                    "data",
                    obj(&[(
                        "uid",
                        Value::List(vec![reference("k8s.namespace", "shop", "metadata.uid")]),
                    )]),
                ),
            ]),
            &[("k8s.namespace", "shop")],
        );
        let all = [ns, cm];
        let redact = Redactor::default();
        let docs = Documents::new(&all, &redact);
        let schema = Schema::default();
        assert_eq!(
            docs.wire(&all[0], &schema).unwrap(),
            Wire::Known(json!({"metadata": {"name": "shop"}}))
        );
        let Wire::Holes(holes) = docs.wire(&all[1], &schema).unwrap() else {
            panic!("the uid is known after apply");
        };
        assert_eq!(
            holes.iter().map(|h| h.to_string()).collect::<Vec<_>>(),
            ["k8s.config_map cfg data.uid[0] = k8s.namespace shop.metadata.uid, known after apply"]
        );
    }

    /// A template over a secret a provider holds is a secret: a hole, said
    /// as one written whole is, never its placeholder.
    #[test]
    fn a_template_over_a_held_secret_is_a_hole() {
        let token = crate::secrets::held::placeholder("vault.token/t#value");
        let secret = resource(
            "k8s.secret",
            "join",
            obj(&[(
                "stringData",
                obj(&[("token", Value::Str(format!("token: {token}")))]),
            )]),
            &[],
        );
        let all = [secret];
        let redact = Redactor::default();
        let docs = Documents::new(&all, &redact);
        let Wire::Holes(holes) = docs.wire(&all[0], &Schema::default()).unwrap() else {
            panic!("the token is a secret");
        };
        assert_eq!(
            holes.iter().map(|h| h.to_string()).collect::<Vec<_>>(),
            ["k8s.secret join stringData.token is a secret: a render prints none"]
        );
    }

    /// The plan's order: rounds in address order, each taking what has
    /// its dependencies before it; an extra edge orders as a dependency
    /// does.
    #[test]
    fn dependencies_come_first() {
        let none = Value::Obj(Default::default());
        let a = resource(
            "k8s.deployment",
            "web",
            none.clone(),
            &[("k8s.namespace", "shop")],
        );
        let b = resource("k8s.namespace", "shop", none.clone(), &[]);
        let c = resource("k8s.traefik.middleware", "m", none.clone(), &[]);
        let d = resource("k8s.custom_resource_definition", "mw", none, &[]);
        let extra = BTreeMap::from([(c.addr.clone(), BTreeSet::from([d.addr.clone()]))]);
        let names: Vec<&str> = order(&[&a, &b, &c, &d], &extra)
            .iter()
            .map(|r| r.addr.name.as_str())
            .collect();
        assert_eq!(names, ["mw", "shop", "m", "web"]);
    }

    /// A Kubernetes object's YAML leads with the keys it is read by.
    #[test]
    fn a_stream_leads_with_the_kind() {
        let o = json!({"data": {"a": "1"}, "metadata": {"name": "x"}, "kind": "ConfigMap", "apiVersion": "v1"});
        assert_eq!(
            yaml_stream(&[o.clone(), o], &["apiVersion", "kind", "metadata"]).unwrap(),
            "---\napiVersion: v1\nkind: ConfigMap\nmetadata:\n  name: x\ndata:\n  a: '1'\n\
             ---\napiVersion: v1\nkind: ConfigMap\nmetadata:\n  name: x\ndata:\n  a: '1'\n"
        );
    }
}
