use crate::ast::{Atom, Term};
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

pub fn compile_resources(facts: impl IntoIterator<Item = Atom>) -> Result<Vec<Resource>> {
    let mut want: BTreeSet<Address> = BTreeSet::new();
    let mut args: Vec<(Address, String, Value)> = Vec::new();

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
                args.push((Address { typ, name }, key, value));
            }
            _ => {}
        }
    }

    // Build attribute objects.
    let mut by_addr: BTreeMap<Address, BTreeMap<String, Value>> = BTreeMap::new();
    for a in &want {
        by_addr.insert(a.clone(), BTreeMap::new());
    }
    for (addr, key, value) in args {
        if !want.contains(&addr) {
            bail!("arg for resource not declared by want: {}.{}", addr.typ, addr.name);
        }
        let m = by_addr.get_mut(&addr).unwrap();
        if let Some(existing) = m.get(&key) {
            if existing != &value {
                bail!(
                    "conflicting arg for {}.{} key '{key}'",
                    addr.typ,
                    addr.name
                );
            }
        } else {
            m.insert(key, value);
        }
    }

    let mut out = Vec::new();
    for (addr, flat) in by_addr {
        let mut root: BTreeMap<String, Value> = BTreeMap::new();
        for (k, v) in flat {
            insert_keypath(&mut root, &k, v)
                .with_context(|| format!("resource {}.{}", addr.typ, addr.name))?;
        }
        let attrs = Value::Obj(root);
        let mut deps = BTreeSet::new();
        collect_deps(&attrs, &mut deps);
        out.push(Resource { addr, attrs, deps });
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

fn collect_deps(v: &Value, out: &mut BTreeSet<Address>) {
    match v {
        Value::Ref { typ, name, .. } => {
            out.insert(Address {
                typ: typ.clone(),
                name: name.clone(),
            });
        }
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
