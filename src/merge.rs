use crate::ast::{Atom, Term};
use crate::value::Value;
use anyhow::{bail, Result};
use std::collections::BTreeMap;

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum MergeOp {
    Scalar,
    MapMerge,
    Set,
    Bag,
}

#[derive(Debug, Default, Clone)]
pub struct MergeRules {
    pub global: BTreeMap<String, MergeOp>,
    pub by_type: BTreeMap<(String, String), MergeOp>,
}

pub fn parse_merge_rules<'a>(facts: impl Iterator<Item = &'a Atom>) -> Result<MergeRules> {
    let mut rules = MergeRules::default();
    for a in facts.filter(|a| a.pred == "merge_rule") {
        match a.args.len() {
            2 => {
                let path = as_str_val(&a.args[0])?.to_string();
                let op = parse_merge_op(as_str_val(&a.args[1])?)?;
                rules.global.insert(path, op);
            }
            3 => {
                let typ = as_str_val(&a.args[0])?.to_string();
                let path = as_str_val(&a.args[1])?.to_string();
                let op = parse_merge_op(as_str_val(&a.args[2])?)?;
                rules.by_type.insert((typ, path), op);
            }
            n => bail!("merge_rule expects arity 2 or 3, got {n}"),
        }
    }
    Ok(rules)
}

pub fn merge_value(a: Value, b: Value, rules: &MergeRules, typ: &str, keypath: &str) -> Result<Value> {
    if a == b {
        return Ok(a);
    }

    let op = merge_op_for(rules, typ, keypath, &a, &b);
    match op {
        MergeOp::Scalar => bail!("conflicting values at '{keypath}': {a:?} vs {b:?}"),
        MergeOp::MapMerge => {
            let (Value::Obj(mut am), Value::Obj(bm)) = (a, b) else {
                bail!("merge_rule map_merge requires objects at '{keypath}'")
            };
            for (k, bv) in bm {
                match am.remove(&k) {
                    None => {
                        am.insert(k, bv);
                    }
                    Some(av) => {
                        let child_path = if keypath.is_empty() {
                            k.clone()
                        } else {
                            format!("{keypath}.{k}")
                        };
                        am.insert(k, merge_value(av, bv, rules, typ, &child_path)?);
                    }
                }
            }
            Ok(Value::Obj(am))
        }
        MergeOp::Set => {
            let (Value::List(mut ax), Value::List(bx)) = (a, b) else {
                bail!("merge_rule set requires lists at '{keypath}'")
            };
            ax.extend(bx);
            ax.sort();
            ax.dedup();
            Ok(Value::List(ax))
        }
        MergeOp::Bag => {
            let (Value::List(mut ax), Value::List(bx)) = (a, b) else {
                bail!("merge_rule bag requires lists at '{keypath}'")
            };
            ax.extend(bx);
            ax.sort();
            Ok(Value::List(ax))
        }
    }
}

pub fn merge_op_for(rules: &MergeRules, typ: &str, keypath: &str, a: &Value, b: &Value) -> MergeOp {
    if let Some(op) = rules.by_type.get(&(typ.to_string(), keypath.to_string())) {
        return *op;
    }
    if let Some(op) = rules.global.get(keypath) {
        return *op;
    }
    match (a, b) {
        (Value::Obj(_), Value::Obj(_)) => MergeOp::MapMerge,
        (Value::List(_), Value::List(_)) => MergeOp::Set,
        _ => MergeOp::Scalar,
    }
}

pub fn parse_merge_op(s: &str) -> Result<MergeOp> {
    match s {
        "scalar" => Ok(MergeOp::Scalar),
        "map_merge" => Ok(MergeOp::MapMerge),
        "set" => Ok(MergeOp::Set),
        "bag" => Ok(MergeOp::Bag),
        other => bail!("unknown merge op '{other}'"),
    }
}

fn as_str_val(t: &Term) -> Result<&str> {
    let Term::Val(Value::Str(s)) = t else {
        bail!("expected string")
    };
    Ok(s)
}
