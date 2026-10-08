//! How dform spells the program's values, terms, atoms, literals and rules
//! back, in its core form (`value`, `term`, `atom`, `lit`, `rule`) and as
//! the program wrote them (`written`); a string literal as `fmt` writes
//! it (`quote`). One renderer for every message and report.

use crate::ast::{Atom, Lit, RuleStmt, Term};
use crate::value::Value;

pub fn term(t: &Term) -> String {
    match t {
        Term::Val(v) => value(v),
        Term::Var(v) => v.clone(),
        Term::Wildcard => "_".into(),
        // `x.len` (R-155), as the program writes it.
        Term::Func { name, args } if name == crate::ir::LEN && args.len() == 1 => {
            format!("{}.len", term(&args[0]))
        }
        Term::Func { name, args } => format!(
            "{name}({})",
            args.iter().map(term).collect::<Vec<_>>().join(", ")
        ),
        Term::List(xs) => format!("[{}]", xs.iter().map(term).collect::<Vec<_>>().join(", ")),
        Term::Obj(m) => format!(
            "{{{}}}",
            m.iter()
                .map(|(k, v)| format!("{k}: {}", term(v)))
                .collect::<Vec<_>>()
                .join(", ")
        ),
        Term::ListComp { item, .. } => format!("[{} | ...]", term(item)),
    }
}

/// A string as the literal `fmt` writes for it (grammar.md "Strings"):
/// quoted, `\\` `\"` `\n` `\t` escaped, any other control character as
/// `\u{..}`, and `${` as `$${`. The plan, `query` and `why` print a string
/// value so, on one line.
pub fn quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\t' => out.push_str("\\t"),
            c if c.is_control() => out.push_str(&format!("\\u{{{:x}}}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out.replace("${", "$${")
}

/// A value with a string bare, unquoted, and anything else as
/// [`value`]: how a key's value, a label's part or a value inside a
/// message prints (`name=api`).
pub fn bare(v: &Value) -> String {
    match v {
        Value::Str(s) => s.clone(),
        v => value(v),
    }
}

pub fn value(v: &Value) -> String {
    match v {
        Value::Str(s) => quote(s),
        Value::Int(i) => i.to_string(),
        Value::Float(f) => f.to_string(),
        Value::Bool(b) => b.to_string(),
        Value::List(xs) => format!("[{}]", xs.iter().map(value).collect::<Vec<_>>().join(", ")),
        Value::Obj(m) => format!(
            "{{{}}}",
            m.iter()
                .map(|(k, v)| format!("{k}: {}", value(v)))
                .collect::<Vec<_>>()
                .join(", ")
        ),
        Value::Ip(n) => crate::value::u32_to_ipv4(*n),
        Value::IpNet { addr, prefix } => crate::value::ipnet_to_string(*addr, *prefix),
        Value::Range(r) => r.to_string(),
        Value::Ref { typ, name, attr } => format!("ref({typ}, {name}, {attr})"),
        Value::CloudRef { typ, name, attr } => format!("cloud_ref({typ}, {name}, {attr})"),
        Value::Null { label, class, .. } => format!("?{label}:{class:?}"),
        Value::Quantity(q) => q.to_string(),
        Value::Time(t) => t.to_string(),
        Value::Uri(u) => u.to_string(),
        Value::Oci(u) => u.clone(),
        Value::Semver(v) => v.to_string(),
    }
}

pub fn atom(a: &Atom) -> String {
    format!(
        "{}({})",
        a.pred,
        a.args.iter().map(term).collect::<Vec<_>>().join(", ")
    )
}

pub fn lit(l: &Lit) -> String {
    match l {
        Lit::Pos(a) => atom(a),
        Lit::Not(a) => format!("not {}", atom(a)),
        Lit::Eq(a, b) => format!("{} = {}", term(a), term(b)),
        Lit::Neq(a, b) => format!("{} != {}", term(a), term(b)),
        Lit::Gt(a, b) => format!("{} > {}", term(a), term(b)),
        Lit::Ge(a, b) => format!("{} >= {}", term(a), term(b)),
        Lit::Lt(a, b) => format!("{} < {}", term(a), term(b)),
        Lit::Le(a, b) => format!("{} <= {}", term(a), term(b)),
    }
}

/// A clause as the program writes it, for a message: membership is
/// `x in r` and `x not in r`, not the relation it lowers to.
pub fn written(l: &Lit) -> String {
    match l {
        Lit::Pos(a) | Lit::Not(a) if a.pred == "member" && a.args.len() == 2 => {
            let not = if matches!(l, Lit::Not(_)) { " not" } else { "" };
            format!("{}{not} in {}", term(&a.args[1]), term(&a.args[0]))
        }
        l => lit(l),
    }
}

pub fn rule(r: &RuleStmt) -> String {
    let body = r.body.iter().map(lit).collect::<Vec<_>>().join(", ");
    if body.is_empty() {
        atom(&r.head)
    } else {
        format!("{} :- {}", atom(&r.head), body)
    }
}

/// A short, cropped rule text for reports.
pub fn rule_short(r: &RuleStmt) -> String {
    let s = rule(r);
    if s.len() > 140 {
        format!("{}...", &s[..140])
    } else {
        s
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bare_leaves_a_string_unquoted_and_nothing_else() {
        assert_eq!(bare(&Value::Str("api".into())), "api");
        assert_eq!(value(&Value::Str("api".into())), "\"api\"");
        let list = Value::List(vec![Value::Str("a".into()), Value::Int(1)]);
        assert_eq!(bare(&list), "[\"a\", 1]");
    }
}
