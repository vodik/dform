//! The errors an evaluation stops at, located at the rule: an undefined relation, a rule
//! that spreads its own value, a head with no value, a reference compared with a string, a
//! call that answered nothing, a field read of what has no fields; and the place a message
//! names.

use super::body::Src;
use super::builtins::{eval_func, eval_term};
use super::nulls::Rec;
use super::unify::unify_atom;
use crate::ast::{Atom, Lit, RuleStmt, Span, Term};
use crate::diag;
use crate::ir::ops;
use crate::ir::store::Window;
use crate::partition;
use crate::spell;
use crate::stuck;
use crate::value::Value;
use anyhow::{Result, bail};
use std::collections::{BTreeSet, HashMap};

/// E §2.6: a body predicate with no definition is a compile error. Defined
/// means: a fact or a rule head, a builtin, a compiler-owned or
/// provider-injected predicate, a fact given to this run, or `decl p(..)`.
pub(super) fn check_defined(
    rules: &[RuleStmt],
    facts: &[Atom],
    externs: &BTreeSet<crate::ast::Extern>,
) -> Result<()> {
    let mut defined: BTreeSet<&str> = facts.iter().map(|a| a.pred.as_str()).collect();
    defined.extend(rules.iter().map(|r| r.head.pred.as_str()));
    defined.extend(externs.iter().map(|e| e.pred.as_str()));
    let is_defined = |p: &str| {
        defined.contains(p)
            || ops::is_builtin_pred(p)
            || matches!(p, "member" | "enumerate")
            || crate::loader::is_core_pred(p)
    };
    let mut errors = Vec::new();
    // A literal several rules share (a resource's, lowered to each of its
    // attributes) is one error.
    let mut said = BTreeSet::new();
    for body in rules.iter().map(|r| &r.body) {
        for lit in body {
            let (Lit::Pos(a) | Lit::Not(a)) = lit else {
                continue;
            };
            // A function std does not have, written as a predicate
            // (`inet.contains(n, a)`): the function's error, naming what
            // replaces it (R-155: `a in n`).
            if !is_defined(&a.pred) && crate::functions::is_function_name(&a.pred) {
                errors.push(crate::functions::unknown(a.span, &a.pred));
            } else if !is_defined(&a.pred) && said.insert((a.span, a.pred.as_str())) {
                // The relation of the program it is nearest (a slip of
                // the pen), else how one is defined.
                let p = a.pred.rsplit("::").next().unwrap_or(&a.pred);
                let decl = format!("decl {p}({})", crate::transform::columns(a.args.len()));
                let named = defined.iter().filter(|d| !d.starts_with("__")).copied();
                let help = match crate::diag::nearest(&a.pred, named) {
                    Some(n) => format!(
                        "`{}` is a relation of the program; else define `{p}`, or declare one \
                         a provider feeds, `{decl}`",
                        n.rsplit("::").next().unwrap_or(n)
                    ),
                    None => format!(
                        "define `{p}` with a fact or a rule, or declare one a provider feeds, \
                         `{decl}`"
                    ),
                };
                errors.push(
                    diag::Diagnostic::error(
                        a.span,
                        format!("undefined predicate {}/{}", a.pred, a.args.len()),
                    )
                    .with_help(help),
                );
            }
        }
    }
    if !errors.is_empty() {
        return Err(diag::Diagnostics(errors).into());
    }
    Ok(())
}

/// `text (at file:line:col, origin)`, or `text` for what the compiler wrote.
pub(super) fn with_place(text: String, span: Span) -> String {
    match diag::place(span) {
        Some(at) => format!("{text} (at {at})"),
        None => text,
    }
}

/// ` (at file:line:col)` for an error message, or nothing.
pub(super) fn at_suffix(span: Span) -> String {
    diag::place(span)
        .map(|at| format!(" (at {at})"))
        .unwrap_or_default()
}

/// Why a head argument has no value (R-119), located at the head: a call
/// that answered nothing (`oci.with_digest(.., "16.0.4")`, a `T?`
/// function's none) reaching a cell, named with the attribute it was to
/// give; or a variable nothing bound, which the compiler must not let
/// through.
pub(super) fn head_error(head: &Atom, state: &HashMap<String, Value>) -> Option<String> {
    enum NoValue {
        /// A call that answered nothing, and whether its function is
        /// partial (R-134: else its arguments are not ones it takes).
        Call(String, bool),
        /// `__as(V, T)` (R-134): a typed position over a computed value
        /// that is not of the type.
        Typed(String, String),
        /// A spread's part that is not what its literal takes (R-199).
        Spread(String),
        Unbound(String),
    }
    fn no_value(t: &Term, state: &HashMap<String, Value>) -> Option<NoValue> {
        if eval_term(t, state).is_some() {
            return None;
        }
        match t {
            Term::Var(v) => Some(NoValue::Unbound(v.clone())),
            Term::Func { name, args } => match args.iter().find_map(|a| no_value(a, state)) {
                Some(inner) => Some(inner),
                None if name == crate::types::AS => {
                    let v = eval_term(&args[0], state)?;
                    let ty = eval_term(&args[1], state)?.as_str()?.to_string();
                    let why = crate::value::read_typed(&ty, &v).err()?;
                    Some(NoValue::Typed(ty, why))
                }
                None => {
                    let vals: Vec<Value> =
                        args.iter().filter_map(|a| eval_term(a, state)).collect();
                    if let Some(why) = spread_error(name, &vals) {
                        return Some(NoValue::Spread(why));
                    }
                    let shown: Vec<String> = vals
                        .iter()
                        .map(|v| spell::term(&Term::Val(v.clone())))
                        .collect();
                    let partial = crate::functions::get(name).is_none_or(|f| f.partial);
                    Some(NoValue::Call(
                        format!("{name}({})", shown.join(", ")),
                        partial,
                    ))
                }
            },
            Term::List(xs) => xs.iter().find_map(|x| no_value(x, state)),
            Term::Obj(m) => m.values().find_map(|x| no_value(x, state)),
            _ => None,
        }
    }
    let at = crate::diag::place(head.span).unwrap_or_else(|| head.pred.clone());
    let s = |t: &Term| match eval_term(t, state) {
        Some(Value::Str(s)) => Some(s),
        _ => None,
    };
    let cell = match (head.pred.as_str(), head.args.as_slice()) {
        ("arg", [t, a, k, _, _]) => match (s(t), s(a), s(k)) {
            (Some(typ), Some(name), Some(k)) => Some(match typ.as_str() {
                crate::modules::INPUT | crate::transform::OUTPUT => format!("{typ} {k}"),
                _ => crate::report::attribute(&crate::ir::Address { typ, name }, &k),
            }),
            _ => None,
        },
        _ => None,
    };
    let what = cell.unwrap_or_else(|| format!("`{}`", head.pred));
    head.args
        .iter()
        .find_map(|t| no_value(t, state))
        .map(|n| match n {
            NoValue::Call(call, true) => format!(
                "{at}: {call} answered nothing, so {what} has no value (the function's \
                 result is optional, `T?`): give it arguments it answers, or test it with \
                 `has` in a clause"
            ),
            NoValue::Call(call, false) => {
                format!("{at}: {call} is not defined for these arguments, so {what} has no value")
            }
            NoValue::Typed(ty, why) => format!("{at}: {what} is {}: {why}", a_type(&ty)),
            NoValue::Spread(why) => format!("{at}: {why}, so {what} has no value"),
            NoValue::Unbound(v) => format!(
                "internal error: {at}: the head of the rule for {what} leaves `{v}` unbound \
                 (a compiler bug: please report it with the program)"
            ),
        })
}

/// `an inet`, `a time`: a type with its article.
fn a_type(ty: &str) -> String {
    match ty.chars().next() {
        Some('a' | 'e' | 'i' | 'o' | 'u') => format!("an {ty}"),
        _ => format!("a {ty}"),
    }
}

/// `attr(T, X, P, "s")` matched nothing under `s`: when the cell holds a
/// reference, the error a comparison of a reference with a string is,
/// never a join that silently does not hold.
pub(super) fn reference_at_string_cell(
    atom: &Atom,
    cell: &ops::Key,
    s: &HashMap<String, Value>,
    src: &Src,
    w: Window,
    rec: &Rec,
) -> Result<()> {
    let probe: Option<Vec<Value>> = cell
        .iter()
        .map(|&c| match &atom.args[c] {
            Term::Val(v) => Some(v.clone()),
            Term::Var(x) => s.get(x).cloned(),
            _ => None,
        })
        .collect();
    let Some(probe) = probe.filter(|p| !p.iter().any(stuck::has_null)) else {
        return Ok(());
    };
    let rel = ops::Rel::of(atom);
    let mut open = atom.clone();
    open.args[3] = Term::Wildcard;
    for t in src.store.candidates(&rel, cell, Some(&probe), false, w) {
        let fact = src.store.get(t);
        let Some(Term::Val(v @ Value::Ref { .. })) = fact.args.get(3) else {
            continue;
        };
        if unify_atom(&open, fact, s, rec)?.is_none() {
            continue;
        }
        let (Term::Var(x), Term::Val(Value::Str(path)), b) =
            (&atom.args[1], &atom.args[2], &atom.args[3])
        else {
            continue;
        };
        let read = format!("{}.{path}", crate::syntax::resolve::source_name(x));
        let Term::Val(bv) = b else { continue };
        if let Some(e) = ref_and_string_written(&read, v, "==", &spell::term(b), bv, rec) {
            return Err(e);
        }
    }
    Ok(())
}

/// A reference compared with a string, `a OP b` (`==`, `!=`, `in` an
/// element of `b`): never equal, so an error at the rule naming both,
/// never a literal that silently does not hold. The compiler says so
/// where it knows the types; this is where it did not (a document's
/// field, a reference of no known type).
pub(super) fn ref_and_string(
    a: &Term,
    av: &Value,
    op: &str,
    b: &Term,
    bv: &Value,
    rec: &Rec,
) -> Option<anyhow::Error> {
    fn shown(t: &Term) -> String {
        match t {
            Term::Var(x) => crate::syntax::resolve::source_name(x),
            Term::List(xs) => format!("[{}]", xs.iter().map(shown).collect::<Vec<_>>().join(", ")),
            t => spell::term(t),
        }
    }
    ref_and_string_written(&shown(a), av, op, &shown(b), bv, rec)
}

/// [`ref_and_string`] with both sides as the program wrote them.
fn ref_and_string_written(
    a: &str,
    av: &Value,
    op: &str,
    b: &str,
    bv: &Value,
    rec: &Rec,
) -> Option<anyhow::Error> {
    let ((typ, name), text) = match (av, bv) {
        (Value::Ref { typ, name, attr }, Value::Str(s))
        | (Value::Str(s), Value::Ref { typ, name, attr })
            if attr.is_empty() =>
        {
            ((typ, name), s)
        }
        _ => return None,
    };
    let reference = crate::report::address(&crate::ir::Address {
        typ: typ.clone(),
        name: name.clone(),
    });
    let help = match text.is_empty() {
        true => "`has r` tests whether a reference is set".to_string(),
        false => format!(
            "compare with the resource: `{}`, or its name in scope",
            crate::ir::Address {
                typ: typ.clone(),
                name: text.clone(),
            }
        ),
    };
    let d = diag::Diagnostic::error(
        rec.head.span,
        format!(
            "`{a} {op} {b}` compares a reference, {reference}, with the string {text:?}: a \
             reference is never a string"
        ),
    )
    .with_help(help);
    Some(diag::Diagnostics(vec![d]).into())
}

/// A call that answered nothing (R-134): `Ok` when the function is
/// partial (`T?`), whose none is an answer, so the literal holding the
/// call fails; else an error at the rule, the arguments not ones the
/// function takes (a bad unit, layout, template or port).
pub(super) fn unanswered(name: &str, args: &[Value], rec: &Rec) -> Result<()> {
    if let Some(why) = spread_error(name, args) {
        bail!("{why}{}", at_suffix(rec.head.span));
    }
    if crate::functions::get(name).is_none_or(|f| f.partial) {
        return Ok(());
    }
    let args: Vec<String> = args.iter().map(spell::value).collect();
    bail!(
        "{name}({}) is not defined for these arguments{}",
        args.join(", "),
        at_suffix(rec.head.span)
    )
}

/// The help for a contribution that spreads the attribute it writes
/// (R-199), `set r.spec = { ..r.spec, x: 1 }`: a value cannot be made from
/// itself, and the fix is a write of each key it adds, `set r.spec.x = 1`.
pub(super) fn self_spread(graph: &partition::Graph, edges: &[partition::Edge]) -> Option<String> {
    edges
        .iter()
        .filter_map(|e| graph.rules.get(e.rule?))
        .find_map(|r| {
            let ("arg", [Term::Val(typ), Term::Val(name), Term::Val(path), merge, ..]) =
                (r.head.pred.as_str(), r.head.args.as_slice())
            else {
                return None;
            };
            let Term::Func {
                name: f,
                args: parts,
            } = merge
            else {
                return None;
            };
            if f != crate::functions::MERGE {
                return None;
            }
            let read = r.body.iter().find_map(|l| match l {
                Lit::Pos(a) if a.pred == "attr" => match a.args.as_slice() {
                    [Term::Val(t), Term::Val(n), Term::Val(p), Term::Var(v)]
                        if (t, n, p) == (typ, name, path) =>
                    {
                        Some(v)
                    }
                    _ => None,
                },
                _ => None,
            })?;
            let at = parts
                .iter()
                .position(|p| matches!(p, Term::Var(v) if v == read))?;
            let a = crate::ir::Address {
                typ: typ.as_str()?.to_string(),
                name: name.as_str()?.to_string(),
            };
            let path = path.as_str()?;
            let sets: Vec<String> = parts[at + 1..]
                .iter()
                .filter_map(|p| match p {
                    Term::Obj(m) => Some(m),
                    _ => None,
                })
                .flatten()
                .map(|(k, v)| {
                    let v = match v {
                        Term::Val(v) => spell::value(v),
                        _ => "..".to_string(),
                    };
                    format!(
                        "`set {} = {v}`",
                        crate::report::reference(&a, &format!("{path}.{k}"))
                    )
                })
                .collect();
            let fix = match sets.as_slice() {
                [] => "write the fields it adds, each its own `set`".to_string(),
                _ => format!("write what it adds, {}", sets.join(", ")),
            };
            Some(format!(
                "\n  help: {} spreads its own value, which it has only once it is given: {fix}",
                crate::report::attribute(&a, path)
            ))
        })
}

/// Why a spread answered nothing (R-199): a part that is not what its
/// literal takes, checked once it has a value, `{ ..x }` of a list or `[..x]`
/// of an object or a dense range; `None` for any other call.
fn spread_error(name: &str, args: &[Value]) -> Option<String> {
    use crate::functions::{CONCAT, MERGE};
    let object = match name {
        MERGE => true,
        CONCAT => false,
        _ => return None,
    };
    let bad = args.iter().find(|v| match (object, v) {
        (_, Value::Null { .. }) | (true, Value::Obj(_)) | (false, Value::List(_)) => false,
        (false, Value::Range(r)) => r.members().is_err(),
        _ => true,
    })?;
    if let Value::Range(r) = bad {
        return Some(format!("a spread `..` in a list: {}", r.members().err()?));
    }
    let (into, takes) = match object {
        true => ("an object", "an object's fields"),
        false => ("a list", "a list's elements"),
    };
    Some(format!(
        "a spread `..` in {into} gives {} `{}`, and {into} takes {takes}",
        crate::value::article(crate::value::type_name(bad)),
        spell::value(bad)
    ))
}

/// A side of a comparison with no value: a call in it that answered
/// nothing fails the literal, or is an error ([`unanswered`]); `None`
/// when no call did.
pub(super) fn side_unanswered(
    t: &Term,
    state: &HashMap<String, Value>,
    rec: &Rec,
) -> Option<Result<()>> {
    if let Err(e) = not_an_object(t, state, rec) {
        return Some(Err(e));
    }
    let (name, args) = failed_builtin(t, state)?;
    Some(unanswered(&name, &args, rec))
}

/// A walk to a path the value does not have (`has x.f`, `x.f.g`, `some c
/// in x.f`): no value, so the literal holding it does not hold. A walk
/// that reaches a value with no fields is an error ([`not_an_object`]).
pub(super) fn missing_walk(t: &Term, state: &HashMap<String, Value>, rec: &Rec) -> Result<bool> {
    not_an_object(t, state, rec)?;
    Ok(failed_builtin(t, state).is_some_and(|(name, _)| name == "__path"))
}

/// A field read of a value that is not an object (R-185: `w.spec` with
/// `w` an address string, a number, a list): an error at the rule naming
/// the read and the value, never a literal that does not hold, so a deny
/// over it cannot pass without checking.
pub(super) fn not_an_object(t: &Term, state: &HashMap<String, Value>, rec: &Rec) -> Result<()> {
    let Term::Func { name, args } = t else {
        return Ok(());
    };
    for a in args {
        not_an_object(a, state, rec)?;
    }
    let ([whole, path], "__path") = (args.as_slice(), name.as_str()) else {
        return Ok(());
    };
    let (Some(v), Some(Value::Str(path))) = (eval_term(whole, state), eval_term(path, state))
    else {
        return Ok(());
    };
    let Some((walked, at)) = crate::functions::not_an_object(&v, &path) else {
        return Ok(());
    };
    let base = match whole {
        Term::Var(x) => crate::syntax::resolve::source_name(x),
        t => spell::term(t),
    };
    let keys = crate::ir::path_keys(&path);
    let reached = std::iter::once(base.clone())
        .chain(walked.iter().cloned())
        .collect::<Vec<_>>()
        .join(".");
    let field = &keys[walked.len()];
    let read = format!("{base}.{}", keys.join("."));
    let (what, help) = match &at {
        Value::Ref { typ, name, attr } if attr.is_empty() => (
            format!(
                "a reference, {}",
                crate::report::address(&crate::ir::Address {
                    typ: typ.clone(),
                    name: name.clone(),
                })
            ),
            format!(
                "a reference's attributes are read where its type is known: bind it, \
                 `{reached} in {typ}`, or type the column it is read from"
            ),
        ),
        Value::Str(s) if walked.is_empty() => (
            format!("the string {s:?}"),
            format!(
                "a resource's attributes are read through a reference: bind `{reached}` to \
                 its resource, `{reached} in T`, or type the column it is read from"
            ),
        ),
        Value::Str(s) => (
            format!("the string {s:?}"),
            format!("read `{reached}` itself"),
        ),
        v => (
            format!("`{}`", spell::value(v)),
            format!("read `{reached}` itself"),
        ),
    };
    let d = diag::Diagnostic::error(
        rec.head.span,
        format!("`{read}`: `{reached}` is {what}, which has no field `{field}`"),
    )
    .with_help(help);
    Err(diag::Diagnostics(vec![d]).into())
}

/// The innermost function application in `t` whose arguments are all ground
/// but which has no value: a builtin applied to the wrong kind of value
/// (`"10" + 1`, `int("abc")`). `None` when the term is merely unbound.
pub(super) fn failed_builtin(
    t: &Term,
    state: &HashMap<String, Value>,
) -> Option<(String, Vec<Value>)> {
    let Term::Func { name, args } = t else {
        return None;
    };
    if let Some(inner) = args.iter().find_map(|a| failed_builtin(a, state)) {
        return Some(inner);
    }
    let vals: Option<Vec<Value>> = args.iter().map(|a| eval_term(a, state)).collect();
    let vals = vals?;
    eval_func(name, args, state)
        .is_none()
        .then(|| (name.clone(), vals))
}
