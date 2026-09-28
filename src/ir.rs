use crate::ast::{Atom, Term};
use crate::transform;
use crate::value::Value;
use anyhow::{Result, bail};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Address {
    pub typ: String,
    pub name: String,
}

#[derive(Debug, Clone)]
pub struct Resource {
    pub addr: Address,
    pub attrs: Value, // must be Obj
    pub deps: BTreeSet<Address>,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Adopt {
    pub addr: Address,
    pub remote: String,
}

/// One resource per `want(T, A)`, its document assembled from the collapsed
/// `attr(T, A, P, V)` facts (every path is top-level after normalization).
pub fn compile_resources(facts: impl IntoIterator<Item = Atom>) -> Result<Vec<Resource>> {
    let mut by_addr: BTreeMap<Address, BTreeMap<String, Value>> = BTreeMap::new();
    let mut attrs: Vec<(Address, String, Value)> = Vec::new();
    for f in facts {
        match (f.pred.as_str(), f.args.len()) {
            ("want", 2) => {
                let typ = as_str_val(&f.args[0])?.to_string();
                let name = as_str_val(&f.args[1])?.to_string();
                by_addr.entry(Address { typ, name }).or_default();
            }
            ("want", _) => bail!("want/2 expected"),
            ("attr", 4) => {
                let typ = as_str_val(&f.args[0])?;
                if typ == transform::SETTINGS || typ == transform::OUTPUT {
                    continue;
                }
                let name = as_str_val(&f.args[1])?.to_string();
                let path = as_str_val(&f.args[2])?.to_string();
                attrs.push((Address { typ: typ.to_string(), name }, path, as_value(&f.args[3])?));
            }
            _ => {}
        }
    }
    for (addr, path, value) in attrs {
        let Some(root) = by_addr.get_mut(&addr) else {
            bail!("attribute {path} for resource not declared by want: {}.{}", addr.typ, addr.name);
        };
        root.insert(path, value);
    }

    Ok(by_addr
        .into_iter()
        .map(|(addr, root)| {
            let attrs = Value::Obj(root);
            let mut deps = BTreeSet::new();
            collect_deps(&attrs, &mut deps);
            Resource { addr, attrs, deps }
        })
        .collect())
}

pub fn compile_adopts<'a>(facts: impl Iterator<Item = &'a Atom>) -> Result<Vec<Adopt>> {
    let mut out = Vec::new();
    for a in facts.filter(|a| a.pred == "adopt") {
        if a.args.len() != 3 {
            bail!("adopt/3 expected");
        }
        let typ = as_str_val(&a.args[0])?.to_string();
        let name = as_str_val(&a.args[1])?.to_string();
        let remote = as_str_val(&a.args[2])?.to_string();
        out.push(Adopt {
            addr: Address { typ, name },
            remote,
        });
    }
    Ok(out)
}

fn as_str_val(t: &Term) -> Result<&str> {
    let Term::Val(Value::Str(s)) = t else {
        bail!("expected string literal")
    };
    Ok(s)
}

fn as_value(t: &Term) -> Result<Value> {
    match t {
        Term::Val(v) => Ok(v.clone()),
        _ => bail!("expected ground value"),
    }
}

fn collect_deps(v: &Value, out: &mut BTreeSet<Address>) {
    match v {
        Value::Ref { typ, name, .. } => {
            out.insert(Address {
                typ: typ.clone(),
                name: name.clone(),
            });
        }
        Value::CloudRef { .. } => {}
        Value::List(xs) => {
            for x in xs {
                collect_deps(x, out);
            }
        }
        Value::Obj(m) => {
            for x in m.values() {
                collect_deps(x, out);
            }
        }
        _ => {}
    }
}
