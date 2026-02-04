use crate::ast::{Atom, Term};
use crate::merge;
use crate::value::Value;
use anyhow::{anyhow, bail, Context, Result};
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

pub fn compile_resources(facts: impl IntoIterator<Item = Atom>) -> Result<Vec<Resource>> {
    let facts: Vec<Atom> = facts.into_iter().collect();

    let mut want: BTreeSet<Address> = BTreeSet::new();
    let mut assigns: Vec<(Address, String, Value)> = Vec::new();
    let mut adds: Vec<(Address, String, Value)> = Vec::new();

    let merge_rules = merge::parse_merge_rules(facts.iter())?;

    for f in facts {
        let pred = f.pred.as_str();
        match pred {
            "want" => {
                if f.args.len() != 2 {
                    bail!("want/2 expected");
                }
                let typ = as_str_val(&f.args[0])?.to_string();
                let name = as_str_val(&f.args[1])?.to_string();
                want.insert(Address { typ, name });
            }
            "arg" => {
                if f.args.len() != 4 {
                    bail!("arg/4 expected");
                }
                let typ = as_str_val(&f.args[0])?.to_string();
                let name = as_str_val(&f.args[1])?.to_string();
                let key = as_str_val(&f.args[2])?.to_string();
                let value = as_value(&f.args[3])?;
                assigns.push((Address { typ, name }, key, value));
            }
            "arg_add" => {
                if f.args.len() != 4 {
                    bail!("arg_add/4 expected");
                }
                let typ = as_str_val(&f.args[0])?.to_string();
                let name = as_str_val(&f.args[1])?.to_string();
                let key = as_str_val(&f.args[2])?.to_string();
                let value = as_value(&f.args[3])?;
                adds.push((Address { typ, name }, key, value));
            }
            "merge_rule" => {}
            _ => {}
        }
    }

    // Build attribute objects.
    let mut out = Vec::new();

    let mut by_addr: BTreeMap<Address, BTreeMap<String, Value>> = BTreeMap::new();
    for a in &want {
        by_addr.insert(a.clone(), BTreeMap::new());
    }

    // Apply scalar assignments first.
    for (addr, key, value) in assigns {
        if !want.contains(&addr) {
            bail!("arg for resource not declared by want: {}.{}", addr.typ, addr.name);
        }
        let root = by_addr.get_mut(&addr).unwrap();
        insert_keypath(root, &key, value)
            .with_context(|| format!("resource {}.{}", addr.typ, addr.name))?;
    }

    // Apply merge contributions.
    for (addr, key, value) in adds {
        if !want.contains(&addr) {
            bail!("arg_add for resource not declared by want: {}.{}", addr.typ, addr.name);
        }
        let root = by_addr.get_mut(&addr).unwrap();
        merge_keypath(root, &addr.typ, &key, value, &merge_rules)
            .with_context(|| format!("resource {}.{}", addr.typ, addr.name))?;
    }

    for (addr, root) in by_addr {
        let attrs = Value::Obj(root);
        let mut deps = BTreeSet::new();
        collect_deps(&attrs, &mut deps);
        out.push(Resource { addr, attrs, deps });
    }

    Ok(out)
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

fn insert_keypath(root: &mut BTreeMap<String, Value>, key: &str, value: Value) -> Result<()> {
    let mut parts = key.split('.').peekable();
    let first = parts
        .next()
        .ok_or_else(|| anyhow!("empty keypath"))?
        .to_string();
    if parts.peek().is_none() {
        // leaf
        if let Some(existing) = root.get(&first) {
            if existing != &value {
                bail!("conflicting value for key '{first}'");
            }
        } else {
            root.insert(first, value);
        }
        return Ok(());
    }

    let entry = root.entry(first).or_insert_with(|| Value::Obj(BTreeMap::new()));
    let Value::Obj(map) = entry else {
        bail!("keypath conflict: expected object")
    };
    let rest: String = parts.collect::<Vec<&str>>().join(".");
    insert_keypath(map, &rest, value)
}

fn merge_keypath(
    root: &mut BTreeMap<String, Value>,
    typ: &str,
    key: &str,
    value: Value,
    rules: &merge::MergeRules,
) -> Result<()> {
    let mut parts = key.split('.').peekable();
    let first = parts
        .next()
        .ok_or_else(|| anyhow!("empty keypath"))?
        .to_string();
    if parts.peek().is_none() {
        // leaf
        match root.get_mut(&first) {
            None => {
                root.insert(first, value);
            }
            Some(existing) => {
                let merged = merge::merge_value(existing.clone(), value, rules, typ, key)
                    .with_context(|| format!("merge key '{key}'"))?;
                *existing = merged;
            }
        }
        return Ok(());
    }

    let entry = root.entry(first).or_insert_with(|| Value::Obj(BTreeMap::new()));
    let Value::Obj(map) = entry else {
        bail!("keypath conflict: expected object")
    };
    let rest: String = parts.collect::<Vec<&str>>().join(".");
    merge_keypath(map, typ, &rest, value, rules)
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
            for (_, x) in m {
                collect_deps(x, out);
            }
        }
        _ => {}
    }
}
