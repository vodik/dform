//! Policy facts: `deny(msg, ctx)` and `warn(msg, ctx)` as the evaluator writes them, and as
//! an evaluation's result reads them back (`msg ctx=JSON`).

use super::value_to_json;
use crate::ast::{Atom, Term, str_term};
use crate::value::Value;
use anyhow::{Result, bail};

/// A policy fact: `deny(msg, ctx)` or `warn(msg, ctx)`.
pub(super) fn policy_fact(pred: &str, msg: &str, ctx: Value) -> Atom {
    Atom {
        pred: pred.into(),
        args: vec![str_term(msg), Term::Val(ctx)],
        record: None,
        span: Default::default(),
    }
}

pub(super) fn format_policy_fact(a: &Atom) -> Result<String> {
    if a.args.is_empty() {
        bail!("policy fact must have at least a message argument");
    }
    let msg = match &a.args[0] {
        Term::Val(Value::Str(s)) => s.clone(),
        _ => bail!("policy message must be a string"),
    };
    if a.args.len() == 1 {
        return Ok(msg);
    }
    let Term::Val(ctx) = &a.args[1] else {
        bail!("policy context must be ground");
    };
    Ok(format!("{msg} ctx={}", value_to_json_string(ctx)?))
}

fn value_to_json_string(v: &Value) -> Result<String> {
    Ok(serde_json::to_string(&value_to_json(v))?)
}
