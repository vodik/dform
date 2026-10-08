//! How dform spells the program's values, terms, atoms, literals and rules
//! back, in its core form (`value`, `term`, `atom`, `lit`, `rule`) and as
//! the program wrote them (`written`); a string literal as `fmt` writes
//! it (`quote`); a value as JSON (`value_to_json`). One renderer for every
//! message and report.

use crate::ast::{Atom, Lit, RuleStmt, Term};
use crate::query::Redactor;
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
pub(crate) fn rule_short(r: &RuleStmt) -> String {
    let s = rule(r);
    if s.len() > 140 {
        format!("{}...", &s[..140])
    } else {
        s
    }
}

/// The program's atoms, terms and literals as it wrote them, for a
/// message about what it does: a variable by its source name, an
/// interpolated string with its holes, an attribute read by its address, a
/// copy's guard as the copy, a value as [`Redactor::surface`] spells it
/// (a secret redacted).
pub struct Written<'a> {
    redact: &'a Redactor,
}

impl<'a> Written<'a> {
    pub fn new(redact: &'a Redactor) -> Written<'a> {
        Written { redact }
    }

    /// An attribute by its address and path; an input's cell as the program
    /// names the input, `input one.namespace` of the copy `one` (R-120).
    pub fn attribute(typ: String, name: String, p: &str) -> String {
        match typ == crate::modules::INPUT {
            true if name.is_empty() => format!("input {p}"),
            true => format!("input {name}.{p}"),
            false => crate::report::attribute(&crate::ir::Address { typ, name }, p),
        }
    }

    pub fn pred(&self, pred: &str) -> String {
        pred.to_string()
    }

    /// A body atom as the program would write it: a copy's guard as the
    /// copy, an attribute read by its address, a row with its variables by
    /// the source's names.
    pub fn atom(&self, a: &Atom) -> String {
        if let Some(scope) = a.pred.strip_suffix("::__instance")
            && let [Term::Val(Value::Str(c))] = a.args.as_slice()
        {
            return format!("resource {c} {}", scope.replace("::", "."));
        }
        if let (
            "attr",
            [
                Term::Val(Value::Str(t)),
                Term::Val(Value::Str(n)),
                Term::Val(Value::Str(p)),
                v,
            ],
        ) = (a.pred.as_str(), a.args.as_slice())
        {
            let addr = Written::attribute(t.clone(), n.clone(), p);
            return match v {
                Term::Val(v) => format!("{addr} = {}", self.redact.surface(v)),
                _ => addr,
            };
        }
        let args: Vec<String> = a.args.iter().map(|t| self.term(t)).collect();
        format!("{}({})", self.pred(&a.pred), args.join(", "))
    }

    pub fn term(&self, t: &Term) -> String {
        match t {
            Term::Val(v) => self.redact.surface(v),
            Term::Var(v) => crate::syntax::resolve::source_name(v),
            Term::Wildcard => "_".into(),
            // An interpolated string as written.
            Term::Func { name, args } if name == crate::ir::FORMAT => match args.split_first() {
                Some((Term::Val(Value::Str(f)), rest)) => {
                    let mut out = String::from("\"");
                    let mut parts = f.split("%s");
                    out.push_str(parts.next().unwrap_or(""));
                    for (p, a) in parts.zip(rest) {
                        out.push_str(&format!("${{{}}}", self.term(a)));
                        out.push_str(p);
                    }
                    out.push('"');
                    out
                }
                _ => term(t),
            },
            Term::Func { name, args } => format!(
                "{name}({})",
                args.iter()
                    .map(|a| self.term(a))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            Term::List(xs) => format!(
                "[{}]",
                xs.iter()
                    .map(|a| self.term(a))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            t => term(t),
        }
    }

    pub fn lit(&self, l: &Lit) -> String {
        let bin = |a: &Term, op: &str, b: &Term| format!("{} {op} {}", self.term(a), self.term(b));
        match l {
            Lit::Pos(a) => self.atom(a),
            Lit::Not(a) => format!("not {}", self.atom(a)),
            Lit::Eq(a, b) => bin(a, "==", b),
            Lit::Neq(a, b) => bin(a, "!=", b),
            Lit::Gt(a, b) => bin(a, ">", b),
            Lit::Ge(a, b) => bin(a, ">=", b),
            Lit::Lt(a, b) => bin(a, "<", b),
            Lit::Le(a, b) => bin(a, "<=", b),
        }
    }

    /// `x = v, y = w`, by the source's names.
    pub fn bindings(&self, env: &std::collections::BTreeMap<String, Value>) -> String {
        env.iter()
            .filter(|(k, _)| !k.starts_with("__"))
            .map(|(k, v)| {
                format!(
                    "{} = {}",
                    crate::syntax::resolve::source_name(k),
                    self.redact.surface(v)
                )
            })
            .collect::<Vec<_>>()
            .join(", ")
    }
}

/// A value as JSON: a string, number, bool, list or object as itself, every
/// other value as its text (`--json`, a policy's context, a document).
pub fn value_to_json(v: &Value) -> serde_json::Value {
    match v {
        Value::Str(s) => serde_json::Value::String(s.clone()),
        Value::Int(i) => serde_json::Value::Number((*i).into()),
        Value::Float(f) => serde_json::Number::from_f64(f.get())
            .map_or(serde_json::Value::Null, serde_json::Value::Number),
        Value::Bool(b) => serde_json::Value::Bool(*b),
        Value::List(xs) => serde_json::Value::Array(xs.iter().map(value_to_json).collect()),
        Value::Obj(m) => serde_json::Value::Object(
            m.iter()
                .map(|(k, v)| (k.clone(), value_to_json(v)))
                .collect(),
        ),
        Value::Ip(n) => serde_json::Value::String(crate::value::u32_to_ipv4(*n)),
        Value::IpNet { addr, prefix } => {
            serde_json::Value::String(crate::value::ipnet_to_string(*addr, *prefix))
        }
        Value::Range(r) => serde_json::Value::String(r.to_string()),
        Value::Ref { typ, name, attr } => {
            serde_json::Value::String(format!("ref({typ},{name},{attr})"))
        }
        Value::CloudRef { typ, name, attr } => {
            serde_json::Value::String(format!("cloud_ref({typ},{name},{attr})"))
        }
        Value::Null { label, .. } => serde_json::Value::String(format!("?{label}")),
        Value::Quantity(q) => serde_json::Value::String(q.to_string()),
        Value::Time(t) => serde_json::Value::String(t.to_string()),
        Value::Uri(u) => serde_json::Value::String(u.to_string()),
        Value::Oci(u) => serde_json::Value::String(u.clone()),
        Value::Semver(v) => serde_json::Value::String(v.to_string()),
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
