use crate::ast::{Atom, Term};
use crate::schema::Schema;
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
///
/// `assemble` of E §2.8 as F DR-11 revises it: the document excludes every
/// schema-computed path (the provider owns it; its value lives in the world's
/// `computed`) and an Optional+Computed path the program left to the provider
/// (its value is still the resource's own null). An `ignore_changes(T, A, P)`
/// path stays: a create sets it; the planner drops it from both sides of an
/// object that exists.
pub fn compile_resources(
    facts: impl IntoIterator<Item = Atom>,
    schema: &Schema,
) -> Result<Vec<Resource>> {
    let mut by_addr: BTreeMap<Address, BTreeMap<String, Value>> = BTreeMap::new();
    let mut attrs: Vec<(Address, String, Value)> = Vec::new();
    let mut ref_deps: Vec<(Address, Address)> = Vec::new();
    for f in facts {
        match (f.pred.as_str(), f.args.len()) {
            (transform::REF_DEP, 4) => {
                let addr = |i: usize| -> Result<Address> {
                    Ok(Address {
                        typ: as_str_val(&f.args[i])?.to_string(),
                        name: as_str_val(&f.args[i + 1])?.to_string(),
                    })
                };
                ref_deps.push((addr(0)?, addr(2)?));
            }
            ("want", 2) => {
                let typ = as_str_val(&f.args[0])?.to_string();
                let name = as_str_val(&f.args[1])?.to_string();
                by_addr.entry(Address { typ, name }).or_default();
            }
            ("want", _) => bail!("want/2 expected"),
            ("attr", 4) => {
                let typ = as_str_val(&f.args[0])?;
                if transform::is_pseudo_type(typ) {
                    continue;
                }
                let name = as_str_val(&f.args[1])?.to_string();
                let path = as_str_val(&f.args[2])?.to_string();
                attrs.push((
                    Address {
                        typ: typ.to_string(),
                        name,
                    },
                    path,
                    as_value(&f.args[3])?,
                ));
            }
            _ => {}
        }
    }
    for (addr, path, value) in attrs {
        let Some(root) = by_addr.get_mut(&addr) else {
            bail!(
                "attribute {path} for resource not declared by want: {}.{}",
                addr.typ,
                addr.name
            );
        };
        root.insert(path, value);
    }

    let mut extra_deps: BTreeMap<Address, BTreeSet<Address>> = BTreeMap::new();
    for (from, to) in ref_deps {
        extra_deps.entry(from).or_default().insert(to);
    }
    Ok(by_addr
        .into_iter()
        .map(|(addr, mut root)| {
            for (p, _) in schema.computed_of(&addr.typ) {
                remove_path(&mut root, &p);
            }
            for (p, _) in schema.optional_computed_of(&addr.typ) {
                let own = crate::value::null_label(&addr.typ, &addr.name, &p);
                if get_path(&root, &p)
                    .is_some_and(|v| matches!(v, Value::Null { label, .. } if *label == own))
                {
                    remove_path(&mut root, &p);
                }
            }
            let attrs = Value::Obj(root);
            let mut deps = BTreeSet::new();
            collect_deps(&attrs, &mut deps);
            deps.extend(extra_deps.remove(&addr).unwrap_or_default());
            deps.remove(&addr);
            Resource { addr, attrs, deps }
        })
        .collect())
}

fn get_path<'a>(root: &'a BTreeMap<String, Value>, path: &str) -> Option<&'a Value> {
    let mut segs = path.split('.');
    let mut cur = root.get(segs.next()?)?;
    for s in segs {
        let Value::Obj(m) = cur else { return None };
        cur = m.get(s)?;
    }
    Some(cur)
}

/// Remove a dotted path; an object left empty by it goes too.
fn remove_path(root: &mut BTreeMap<String, Value>, path: &str) {
    match path.split_once('.') {
        None => {
            root.remove(path);
        }
        Some((head, rest)) => {
            if let Some(Value::Obj(m)) = root.get_mut(head) {
                remove_path(m, rest);
                if m.is_empty() {
                    root.remove(head);
                }
            }
        }
    }
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
        // A null depends on the resource whose Apply resolves it.
        Value::Null { label, .. } => {
            if let Some((typ, name)) = crate::value::null_owner(label) {
                out.insert(Address { typ, name });
            }
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
