//! Refinement types (E DR-13, F DR-13 revised): `check` on a `type` block
//! attribute, on an input, and the provider schema fact
//! `type_refine(T, Path, Constraint)`.
//!
//! The checkable table, the `Constraint` term language:
//!
//!   range(Lo, Hi)       an int in [Lo, Hi]
//!   prefix_len_le(N)    a CIDR's prefix length <= N
//!   prefix_len_ge(N)    a CIDR's prefix length >= N
//!   enum([V, ...])      one of the values
//!   regex(S)            a string S matches whole
//!   len_le(N)           a string's, list's or object's length <= N
//!   len_ge(N)           ... >= N
//!   type(int|string|bool|inet)   a `type` block's declared type
//!
//! A refinement over the attribute's own value alone that fits the table is
//! a constraint contribution to the attribute's cell (`lattice::Refinement`):
//! rank-blind, checked against the winning value at collapse. False derives
//! `deny("refinement violated", ...)`; a value carrying a null defers the
//! check (`refinement_deferred/5`) until the null resolves; on a path the
//! schema marks `sensitive` the engine never sees it: it is an Apply
//! assertion the provider checks after materializing the secret (E0306 when
//! the provider's Schema lacks `checks_refinements`). Every other refinement
//! (another attribute, a user predicate, a bound the table cannot say)
//! lowers to a `deny` rule with the refinement's span. A literal value that
//! violates a checkable refinement is a compile error.
//!
//! In a `check`, the attribute is written by its path (`db.backup_days`) or
//! its name in its block (`backup_days`); an input by its name.

use crate::ast::{
    Atom, AttrDecl, Lit, Program, RuleStmt, Span, Stmt, Term, TypeExpr, atom, str_term,
};
use crate::diag::{Diagnostic, Diagnostics};
use crate::lattice::{Constraint, Truth};
use crate::schema::Schema;
use crate::value::Value;
use anyhow::Result;
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

/// `type_refine(T, Path, C)`: a checkable refinement of every `T`'s `Path`.
pub const TYPE_REFINE: &str = "type_refine";
/// `attr_refine(T, A, Path, C)`: of one cell (an input's).
pub const ATTR_REFINE: &str = "attr_refine";
/// `refinement_deferred(T, A, Path, C, Nulls)`: a check the winning value
/// could not decide at plan time; re-checked when `Nulls` resolve.
pub const DEFERRED: &str = "refinement_deferred";
/// The deny message of a violated refinement.
pub const VIOLATED: &str = "refinement violated";

impl fmt::Display for Constraint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Constraint::IsInt => write!(f, "type(int)"),
            Constraint::IsStr => write!(f, "type(string)"),
            Constraint::IsBool => write!(f, "type(bool)"),
            Constraint::IsInet => write!(f, "type(inet)"),
            Constraint::Range(lo, hi) => write!(f, "range({lo}, {hi})"),
            Constraint::PrefixLenLe(n) => write!(f, "prefix_len_le({n})"),
            Constraint::PrefixLenGe(n) => write!(f, "prefix_len_ge({n})"),
            Constraint::LenLe(n) => write!(f, "len_le({n})"),
            Constraint::LenGe(n) => write!(f, "len_ge({n})"),
            Constraint::Regex(s) => write!(f, "regex({s:?})"),
            Constraint::OneOf(vs) => write!(
                f,
                "enum([{}])",
                vs.iter()
                    .map(crate::partition::fmt_value)
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        }
    }
}

/// A constraint term as a schema file or a `type_refine` fact writes it:
/// `range(1, 35)`, `enum([a, b])`, `regex("^[a-z]+$")`, or its text.
pub fn from_term(t: &Term) -> Result<Constraint, String> {
    let (name, args) = match t {
        Term::Func { name, args } => (name.as_str(), args.as_slice()),
        Term::Val(Value::Str(s)) => return parse(s),
        _ => return Err(format!("not a refinement: {t:?}")),
    };
    let int = |i: usize| match args.get(i).and_then(Term::ground) {
        Some(Value::Int(n)) => Ok(n),
        _ => Err(format!("{name}: argument {} must be an integer", i + 1)),
    };
    let prefix = |i: usize| {
        let n = int(i)?;
        u8::try_from(n)
            .ok()
            .filter(|n| *n <= 32)
            .ok_or_else(|| format!("{name}({n}): a prefix length is 0 to 32"))
    };
    let arity = |n: usize| {
        if args.len() == n {
            Ok(())
        } else {
            Err(format!(
                "{name} takes {n} argument{}",
                if n == 1 { "" } else { "s" }
            ))
        }
    };
    Ok(match name {
        "range" => {
            arity(2)?;
            let (lo, hi) = (int(0)?, int(1)?);
            if lo > hi {
                return Err(format!("range({lo}, {hi}) is empty"));
            }
            Constraint::Range(lo, hi)
        }
        "prefix_len_le" => {
            arity(1)?;
            Constraint::PrefixLenLe(prefix(0)?)
        }
        "prefix_len_ge" => {
            arity(1)?;
            Constraint::PrefixLenGe(prefix(0)?)
        }
        "len_le" => {
            arity(1)?;
            Constraint::LenLe(int(0)?)
        }
        "len_ge" => {
            arity(1)?;
            Constraint::LenGe(int(0)?)
        }
        "regex" => {
            arity(1)?;
            let Some(Value::Str(re)) = args.first().and_then(Term::ground) else {
                return Err("regex takes a string".into());
            };
            check_regex(&re)?;
            Constraint::Regex(re)
        }
        "enum" => {
            arity(1)?;
            let Some(Value::List(vs)) = args.first().and_then(Term::ground) else {
                return Err("enum takes a list of values".into());
            };
            if vs.is_empty() {
                return Err("enum([]) admits nothing".into());
            }
            Constraint::OneOf(vs.into_iter().collect())
        }
        "type" => {
            arity(1)?;
            match args.first().and_then(Term::ground) {
                Some(Value::Str(t)) if t == "int" => Constraint::IsInt,
                Some(Value::Str(t)) if t == "string" => Constraint::IsStr,
                Some(Value::Str(t)) if t == "bool" => Constraint::IsBool,
                Some(Value::Str(t)) if t == "inet" => Constraint::IsInet,
                _ => return Err("type takes int, string, bool or inet".into()),
            }
        }
        other => {
            return Err(format!(
                "unknown refinement {other} (expected range, prefix_len_le, prefix_len_ge, \
                 enum, regex, len_le, len_ge)"
            ));
        }
    })
}

/// A constraint from its text (`Constraint`'s `Display`), as a
/// `type_refine` fact carries it.
pub fn parse(text: &str) -> Result<Constraint, String> {
    let prog = crate::parser::parse_literal_text(&format!("refinement({text})"))
        .map_err(|_| format!("not a refinement: {text}"))?;
    match prog.statements.as_slice() {
        [Stmt::Fact(a)] if a.args.len() == 1 && !matches!(a.args[0], Term::Val(_)) => {
            from_term(&a.args[0])
        }
        _ => Err(format!("not a refinement: {text}")),
    }
}

fn compile(re: &str) -> Result<regex::Regex, regex::Error> {
    regex::Regex::new(&format!("^(?:{re})$"))
}

pub fn check_regex(re: &str) -> Result<(), String> {
    compile(re)
        .map(|_| ())
        .map_err(|e| format!("regex({re:?}): {e}"))
}

/// Whether `re` matches the whole of `s` (a pattern that does not compile
/// matches nothing; it was checked where it was written).
pub fn regex_matches(re: &str, s: &str) -> bool {
    compile(re).is_ok_and(|r| r.is_match(s))
}

/// The type tag a declared attribute type checks: scalars and enums.
/// Lists, objects, refs and `any` are not checked.
pub fn of_type(t: &TypeExpr) -> Option<Constraint> {
    match t {
        TypeExpr::Name(n) => match n.as_str() {
            "int" => Some(Constraint::IsInt),
            "string" => Some(Constraint::IsStr),
            "bool" => Some(Constraint::IsBool),
            "inet" => Some(Constraint::IsInet),
            _ => None,
        },
        TypeExpr::Apply(n, xs) if n == "enum" => Some(Constraint::OneOf(
            xs.iter()
                .filter_map(|x| match x {
                    TypeExpr::Name(s) | TypeExpr::Str(s) => Some(Value::Str(s.clone())),
                    _ => None,
                })
                .collect(),
        )),
        _ => None,
    }
}

/// A `check` body over the value written `names`, split into what fits the
/// checkable table and the rest (which lowers to a deny). A literal fits
/// when it reads the value alone: `lo <= x <= hi` (both bounds: a range;
/// one alone does not fit), `x == v`, `x in [..]`, `x.len OP n`,
/// `x.bits OP n` (a network's prefix length), `matches(x, "re")`. The split is sound because a
/// `check` is a conjunction: `not (A, B)` is `not A` or `not B`, and a
/// literal that fits shares no variable with the rest.
pub fn split(body: &[Lit], names: &[&str]) -> (Vec<Constraint>, Vec<Lit>) {
    let is_self = |t: &Term| matches!(t, Term::Val(Value::Str(s)) if names.contains(&s.as_str()));
    let of_self = |t: &Term, f: &str| matches!(t, Term::Func { name, args } if name == f && args.len() == 1 && is_self(&args[0]));
    // `x.bits`: a network's prefix length, its field (R-134), named by
    // its text as the value is.
    let bits = |t: &Term| matches!(t, Term::Val(Value::Str(s)) if s.strip_suffix(".bits").is_some_and(|n| names.contains(&n)));
    // `x.len`: its length (R-155), lowered or as its text.
    let length = |t: &Term| {
        of_self(t, crate::ir::LEN)
            || matches!(t, Term::Val(Value::Str(s)) if s.strip_suffix(".len").is_some_and(|n| names.contains(&n)))
    };
    let int = |t: &Term| match t {
        Term::Val(Value::Int(n)) => Some(*n),
        _ => None,
    };
    #[derive(Clone, Copy, PartialEq)]
    enum Op {
        Eq,
        Le,
        Lt,
        Ge,
        Gt,
    }
    // `a OP b` as `a OP b` or `b OP' a`, with the value's side first.
    let cmp = |l: &Lit| -> Option<(Term, Op, Term)> {
        let (a, op, b) = match l {
            Lit::Eq(a, b) => (a, Op::Eq, b),
            Lit::Le(a, b) => (a, Op::Le, b),
            Lit::Lt(a, b) => (a, Op::Lt, b),
            Lit::Ge(a, b) => (a, Op::Ge, b),
            Lit::Gt(a, b) => (a, Op::Gt, b),
            _ => return None,
        };
        let mentions = |t: &Term| is_self(t) || length(t) || bits(t);
        if mentions(a) {
            Some((a.clone(), op, b.clone()))
        } else if mentions(b) {
            let flip = match op {
                Op::Eq => Op::Eq,
                Op::Le => Op::Ge,
                Op::Lt => Op::Gt,
                Op::Ge => Op::Le,
                Op::Gt => Op::Lt,
            };
            Some((b.clone(), flip, a.clone()))
        } else {
            None
        }
    };
    let mut out = Vec::new();
    let mut rest = Vec::new();
    // Bounds on the value itself, with the literal each came from.
    type Bounds<'a> = Vec<(i64, &'a Lit)>;
    let (mut lo, mut hi): (Bounds, Bounds) = Default::default();
    for l in body {
        if let Lit::Pos(a) = l
            && a.pred == "member"
            && let [list, x] = a.args.as_slice()
            && is_self(x)
            && let Some(Value::List(vs)) = list.ground()
            && !vs.is_empty()
        {
            out.push(Constraint::OneOf(vs.into_iter().collect()));
            continue;
        }
        if let Lit::Pos(a) = l
            && a.pred == "matches"
            && let [x, Term::Val(Value::Str(re))] = a.args.as_slice()
            && is_self(x)
            && check_regex(re).is_ok()
        {
            out.push(Constraint::Regex(re.clone()));
            continue;
        }
        let Some((subject, op, c)) = cmp(l) else {
            rest.push(l.clone());
            continue;
        };
        if is_self(&subject) {
            match (op, int(&c), c.ground()) {
                (Op::Eq, _, Some(v)) if nulls_free(&v) => {
                    out.push(Constraint::OneOf(BTreeSet::from([v])))
                }
                (Op::Le, Some(n), _) => hi.push((n, l)),
                (Op::Lt, Some(n), _) => hi.push((n - 1, l)),
                (Op::Ge, Some(n), _) => lo.push((n, l)),
                (Op::Gt, Some(n), _) => lo.push((n + 1, l)),
                _ => rest.push(l.clone()),
            }
            continue;
        }
        let Some(n) = int(&c) else {
            rest.push(l.clone());
            continue;
        };
        type Make = fn(i64) -> Option<Constraint>;
        let (le, ge): (Make, Make) = if length(&subject) {
            (
                |n| Some(Constraint::LenLe(n)),
                |n| Some(Constraint::LenGe(n)),
            )
        } else {
            (
                |n| u8::try_from(n).ok().map(Constraint::PrefixLenLe),
                |n| u8::try_from(n).ok().map(Constraint::PrefixLenGe),
            )
        };
        let cs = match op {
            Op::Le => vec![le(n)],
            Op::Lt => vec![le(n - 1)],
            Op::Ge => vec![ge(n)],
            Op::Gt => vec![ge(n + 1)],
            Op::Eq => vec![le(n), ge(n)],
        };
        match cs.into_iter().collect::<Option<Vec<_>>>() {
            Some(cs) => out.extend(cs),
            None => rest.push(l.clone()),
        }
    }
    match (lo.iter().map(|x| x.0).max(), hi.iter().map(|x| x.0).min()) {
        (Some(l), Some(h)) if l <= h => out.push(Constraint::Range(l, h)),
        _ => rest.extend(lo.iter().chain(&hi).map(|(_, l)| (*l).clone())),
    }
    (out, rest)
}

/// A refinement as an Apply assertion (F DR-13 revised): the op is the
/// constraint's name (`len_ge`, `range`, `enum`, ...), the value its
/// arguments.
pub fn to_assertion(c: &Constraint) -> (String, serde_json::Value) {
    use serde_json::json;
    let (op, v) = match c {
        Constraint::IsInt => ("type", json!("int")),
        Constraint::IsStr => ("type", json!("string")),
        Constraint::IsBool => ("type", json!("bool")),
        Constraint::IsInet => ("type", json!("inet")),
        Constraint::Range(lo, hi) => ("range", json!([lo, hi])),
        Constraint::PrefixLenLe(n) => ("prefix_len_le", json!(n)),
        Constraint::PrefixLenGe(n) => ("prefix_len_ge", json!(n)),
        Constraint::LenLe(n) => ("len_le", json!(n)),
        Constraint::LenGe(n) => ("len_ge", json!(n)),
        Constraint::Regex(re) => ("regex", json!(re)),
        Constraint::OneOf(vs) => (
            "enum",
            serde_json::Value::Array(vs.iter().map(crate::engine::value_to_json).collect()),
        ),
    };
    (op.to_string(), v)
}

/// The refinement an Apply assertion carries, for a provider that checks
/// them in this crate (the mock).
pub fn from_assertion(op: &str, v: &serde_json::Value) -> Option<Constraint> {
    let args: Vec<Term> = match v {
        serde_json::Value::Array(xs) if op == "range" => xs
            .iter()
            .map(|x| Term::Val(crate::provider::json_to_value(x)))
            .collect(),
        serde_json::Value::Array(xs) => vec![Term::List(
            xs.iter()
                .map(|x| Term::Val(crate::provider::json_to_value(x)))
                .collect(),
        )],
        x => vec![Term::Val(crate::provider::json_to_value(x))],
    };
    from_term(&Term::Func {
        name: op.to_string(),
        args,
    })
    .ok()
}

/// The part of a `check` that lowers to a deny, checked at compile time:
/// a call to a function the evaluator does not have would have no value
/// and deny every value, and `matches(x, "re")` refines the value itself
/// with a pattern that compiles.
pub fn check_rest(rest: &[Lit], span: Span) -> Vec<Diagnostic> {
    fn calls(t: &Term, out: &mut Vec<String>) {
        match t {
            Term::Func { name, args } => {
                out.push(name.clone());
                args.iter().for_each(|a| calls(a, out));
            }
            Term::List(xs) => xs.iter().for_each(|a| calls(a, out)),
            Term::Obj(m) => m.values().for_each(|a| calls(a, out)),
            _ => {}
        }
    }
    let mut out = Vec::new();
    for l in rest {
        let mut names = Vec::new();
        match l {
            Lit::Pos(a) | Lit::Not(a) => {
                if a.pred == "matches" {
                    let msg = match a.args.get(1) {
                        Some(Term::Val(Value::Str(re))) => match check_regex(re) {
                            Err(e) => e,
                            Ok(()) => "matches(x, \"re\") refines the attribute's own value".into(),
                        },
                        _ => "matches(x, \"re\") takes the pattern as a string".into(),
                    };
                    out.push(Diagnostic::error(span, format!("in a refinement: {msg}")));
                }
                a.args.iter().for_each(|t| calls(t, &mut names));
            }
            Lit::Eq(x, y)
            | Lit::Neq(x, y)
            | Lit::Gt(x, y)
            | Lit::Ge(x, y)
            | Lit::Lt(x, y)
            | Lit::Le(x, y) => {
                calls(x, &mut names);
                calls(y, &mut names);
            }
        }
        for f in names {
            // The lowering's own (`x.len`, `a + b`) came from what the
            // program wrote.
            if !crate::functions::callable(&f) && crate::functions::get(&f).is_none() {
                let d = crate::functions::unknown(span, &f);
                out.push(Diagnostic {
                    message: format!("in a refinement: {}", d.message),
                    ..d
                });
            }
        }
    }
    out
}

/// An input's `check`, split. A secret input's refinement is checked by its
/// deny rule, whose message never prints the value (the static secret pass
/// exempts it); it has no provider to defer to.
pub fn split_input(i: &crate::ast::InputDecl) -> (Vec<Constraint>, Vec<Lit>) {
    if matches!(&i.ty, TypeExpr::Apply(n, _) if n == "secret") {
        return (Vec::new(), i.refinement.clone());
    }
    split(&i.refinement, &[i.name.as_str()])
}

fn nulls_free(v: &Value) -> bool {
    crate::lattice::nulls_in(v).is_empty()
}

/// `type_refine(T, P, C)` / `attr_refine(T, A, P, C)` as a fact.
pub fn refine_fact(typ: &str, addr: Option<&str>, path: &str, c: &Constraint, span: Span) -> Stmt {
    let mut args = vec![str_term(typ)];
    args.extend(addr.map(str_term));
    args.extend([str_term(path), str_term(&c.to_string())]);
    let pred = if addr.is_some() {
        ATTR_REFINE
    } else {
        TYPE_REFINE
    };
    Stmt::Fact(atom(pred, args, span))
}

/// Every top-level `type T { ... }` block as its refinements: a
/// `type_refine` fact per checkable refinement and declared scalar type,
/// and a deny rule per attribute whose `check` does not fit the table. A
/// type block elsewhere stays pending (and is rejected there).
pub fn lower_types(program: &Program) -> Result<Program> {
    let mut out = Vec::new();
    let mut diags = Vec::new();
    for s in &program.statements {
        let Stmt::Pending(p) = s else {
            out.push(s.clone());
            continue;
        };
        let crate::ast::PendingKind::TypeDecl { name, attrs } = &p.kind;
        let mut leaves = Vec::new();
        flatten(attrs, "", &mut leaves);
        let paths: BTreeSet<String> = leaves.iter().map(|(p, _)| p.clone()).collect();
        // Each attribute's declared type: a value type's value is read as
        // one where the rest reads it (R-134: `net.bits` of an `inet`).
        let types: BTreeMap<String, crate::types::Ty> = leaves
            .iter()
            .filter_map(|(p, a)| Some((p.clone(), crate::types::of_expr(a.ty.as_ref()?))))
            .collect();
        let mut n = 0;
        for (path, a) in leaves {
            if let Some(f) = a.flags.first() {
                diags.push(
                    Diagnostic::error(a.span, format!("a type block attribute flag ({f}) is not yet supported"))
                        .with_note("flags come from the provider's schema; its semantics land with phase 6 \"Refinement types, doc annotations, L15 inet\""),
                );
                continue;
            }
            if let Some(c) = a.ty.as_ref().and_then(of_type) {
                out.push(refine_fact(name, None, &path, &c, a.span));
            }
            let names = [path.as_str(), a.path.as_str()];
            let (cs, rest) = split(&a.refinement, &names);
            for c in cs {
                out.push(refine_fact(name, None, &path, &c, a.span));
            }
            let bad = check_rest(&rest, a.span);
            if !bad.is_empty() {
                diags.extend(bad);
                continue;
            }
            if !rest.is_empty() {
                n += 1;
                out.extend(deny_rules(
                    name, &path, &names, &rest, &paths, &types, n, a.span,
                ));
            }
        }
    }
    if diags.is_empty() {
        Ok(Program {
            statements: out,
            stack: program.stack.clone(),
        })
    } else {
        Err(Diagnostics(diags).into())
    }
}

/// Leaves of a type block, with their full dotted paths.
fn flatten<'a>(attrs: &'a [AttrDecl], prefix: &str, out: &mut Vec<(String, &'a AttrDecl)>) {
    for a in attrs {
        let path = if prefix.is_empty() {
            a.path.clone()
        } else {
            format!("{prefix}.{}", a.path)
        };
        if a.children.is_empty() {
            out.push((path, a));
        } else {
            flatten(&a.children, &path, out);
        }
    }
}

/// `attr(T, A, P, V)` for a path of `typ`, read into `v`: a resource's
/// attribute is its top-level segment (E §2.5 normalization), the rest a
/// field walk; a pseudo-type's path is its whole key.
fn read_attr(typ: &str, addr: &Term, path: &str, v: &Term, n: usize, span: Span) -> Vec<Lit> {
    if crate::transform::is_pseudo_type(typ) {
        return vec![Lit::Pos(atom(
            "attr",
            vec![str_term(typ), addr.clone(), str_term(path), v.clone()],
            span,
        ))];
    }
    let Some((top, rest)) = path.split_once('.') else {
        return vec![Lit::Pos(atom(
            "attr",
            vec![str_term(typ), addr.clone(), str_term(path), v.clone()],
            span,
        ))];
    };
    let whole = Term::Var(format!("__RefineTop{n}"));
    vec![
        Lit::Pos(atom(
            "attr",
            vec![str_term(typ), addr.clone(), str_term(top), whole.clone()],
            span,
        )),
        Lit::Eq(
            v.clone(),
            Term::Func {
                name: "__path".into(),
                args: vec![whole, str_term(rest)],
            },
        ),
    ]
}

/// `check R` that does not fit the table, on `typ`'s `path`: the value and
/// every other attribute of the block `R` names are read, and
/// `deny("refinement violated", {...}) :- reads, not ok(A, V)` with
/// `ok(A, V) :- reads, R`.
#[allow(clippy::too_many_arguments)]
fn deny_rules(
    typ: &str,
    path: &str,
    names: &[&str],
    rest: &[Lit],
    paths: &BTreeSet<String>,
    types: &BTreeMap<String, crate::types::Ty>,
    n: usize,
    span: Span,
) -> Vec<Stmt> {
    let addr = Term::Var("__RefineAddr".into());
    let v = Term::Var("__RefineValue".into());
    let typed = |p: &str, t: &Term| match types.get(p) {
        Some(ty) => crate::types::at_run_time(ty, t.clone()),
        None => t.clone(),
    };
    let mut reads = read_attr(typ, &addr, path, &v, 0, span);
    let mut subst: BTreeMap<String, Term> = names
        .iter()
        .map(|s| (s.to_string(), typed(path, &v)))
        .collect();
    // Another attribute of the block, named by its path.
    let mut named: BTreeSet<String> = BTreeSet::new();
    for l in rest {
        lit_strs(l, &mut named);
    }
    // `wide.bits` names the attribute `wide` (R-134: a field of it).
    let others: BTreeSet<String> = named
        .into_iter()
        .map(|s| match s.split_once('.') {
            Some((h, _)) if !paths.contains(&s) && paths.contains(h) => h.to_string(),
            _ => s,
        })
        .filter(|s| paths.contains(s) && !subst.contains_key(s))
        .collect();
    let others: Vec<String> = others.into_iter().collect();
    for (i, other) in others.iter().enumerate() {
        let w = Term::Var(format!("__RefineOther{i}"));
        reads.extend(read_attr(typ, &addr, other, &w, i + 1, span));
        subst.insert(other.clone(), typed(other, &w));
    }
    let body: Vec<Lit> = rest.iter().map(|l| subst_lit(l, &subst)).collect();
    // The attribute (and each other one named) printed as written, not as
    // a string.
    let named: BTreeMap<String, Term> = subst
        .keys()
        .map(|k| (k.clone(), Term::Var(k.clone())))
        .collect();
    let text = rest
        .iter()
        .map(|l| crate::partition::fmt_written(&subst_lit(l, &named)))
        .collect::<Vec<_>>()
        .join(", ");
    let ok = atom(
        &format!("__refine_{}_{n}", typ.replace('.', "_")),
        vec![addr.clone(), v.clone()],
        span,
    );
    let ctx = BTreeMap::from([
        ("type".to_string(), str_term(typ)),
        ("addr".to_string(), addr.clone()),
        ("path".to_string(), str_term(path)),
        ("constraint".to_string(), str_term(&text)),
        ("value".to_string(), v.clone()),
        (
            "reason".to_string(),
            Term::Func {
                name: crate::ir::FORMAT.into(),
                args: vec![str_term(&format!("%s does not satisfy {text}")), v.clone()],
            },
        ),
        (
            "at".to_string(),
            str_term(&crate::diag::place(span).unwrap_or_default()),
        ),
    ]);
    let mut ok_body = reads.clone();
    ok_body.extend(body);
    let mut deny_body = reads;
    deny_body.push(Lit::Not(ok.clone()));
    vec![
        Stmt::Rule(RuleStmt {
            head: ok,
            body: ok_body,
        }),
        Stmt::Rule(RuleStmt {
            head: atom("deny", vec![str_term(VIOLATED), Term::Obj(ctx)], span),
            body: deny_body,
        }),
    ]
}

fn lit_strs(l: &Lit, out: &mut BTreeSet<String>) {
    fn term(t: &Term, out: &mut BTreeSet<String>) {
        match t {
            Term::Val(Value::Str(s)) => {
                out.insert(s.clone());
            }
            Term::Func { args, .. } | Term::List(args) => args.iter().for_each(|a| term(a, out)),
            Term::Obj(m) => m.values().for_each(|a| term(a, out)),
            _ => {}
        }
    }
    match l {
        Lit::Pos(a) | Lit::Not(a) => a.args.iter().for_each(|t| term(t, out)),
        Lit::Eq(a, b)
        | Lit::Neq(a, b)
        | Lit::Gt(a, b)
        | Lit::Ge(a, b)
        | Lit::Lt(a, b)
        | Lit::Le(a, b) => {
            term(a, out);
            term(b, out);
        }
    }
}

fn subst_term(t: &Term, s: &BTreeMap<String, Term>) -> Term {
    match t {
        Term::Val(Value::Str(x)) => match s.get(x) {
            Some(v) => v.clone(),
            // A field of the value, `net.bits` (R-134): read off it, or,
            // where the names print as written, as written.
            None => match x.split_once('.').and_then(|(h, f)| Some((s.get(h)?, h, f))) {
                Some((Term::Var(v), h, _)) if v == h => Term::Var(x.clone()),
                Some((v, _, f)) => Term::Func {
                    name: "__path".into(),
                    args: vec![v.clone(), Term::Val(Value::Str(f.to_string()))],
                },
                None => t.clone(),
            },
        },
        Term::Func { name, args } => Term::Func {
            name: name.clone(),
            args: args.iter().map(|a| subst_term(a, s)).collect(),
        },
        Term::List(xs) => Term::List(xs.iter().map(|a| subst_term(a, s)).collect()),
        Term::Obj(m) => Term::Obj(
            m.iter()
                .map(|(k, a)| (k.clone(), subst_term(a, s)))
                .collect(),
        ),
        other => other.clone(),
    }
}

fn subst_lit(l: &Lit, s: &BTreeMap<String, Term>) -> Lit {
    l.clone().map_terms(|t| subst_term(&t, s))
}

/// One checkable refinement a program or schema states.
#[derive(Debug, Clone)]
pub struct Stated {
    pub typ: String,
    /// `None`: every address of `typ` (`type_refine`).
    pub addr: Option<Value>,
    pub path: String,
    pub constraint: Constraint,
    /// Where it is written; `None` for a provider schema's.
    pub span: Option<Span>,
}

impl Stated {
    /// A `type_refine/3` or `attr_refine/4` fact.
    pub fn of(a: &Atom) -> Option<Result<Stated, String>> {
        let v = |t: &Term| match t {
            Term::Val(v) => Some(v.clone()),
            _ => None,
        };
        let (typ, addr, path, c) = match (a.pred.as_str(), a.args.as_slice()) {
            (TYPE_REFINE, [t, p, c]) => (v(t)?, None, v(p)?, c),
            (ATTR_REFINE, [t, x, p, c]) => (v(t)?, Some(v(x)?), v(p)?, c),
            _ => return None,
        };
        let (Value::Str(typ), Value::Str(path)) = (typ, path) else {
            return Some(Err(format!("{}: type and path must be symbols", a.pred)));
        };
        Some(from_term(c).map(|constraint| Stated {
            typ,
            addr,
            path: path.trim_start_matches('.').to_string(),
            constraint,
            span: (!a.span.is_none()).then_some(a.span),
        }))
    }

    /// Whether this refinement applies to the attribute group `(typ, addr,
    /// path)`: at the path or below it.
    pub fn applies(&self, typ: &str, addr: &Value, path: &str) -> bool {
        self.typ == typ
            && self.addr.as_ref().is_none_or(|a| a == addr)
            && (self.path == path
                || self
                    .path
                    .strip_prefix(path)
                    .is_some_and(|r| r.starts_with('.')))
    }

    fn describe(&self) -> String {
        match &self.addr {
            Some(a) => format!(
                "{ATTR_REFINE}({}, {}, {}, {})",
                self.typ,
                crate::partition::fmt_value(a),
                self.path,
                self.constraint
            ),
            None => format!(
                "{TYPE_REFINE}({}, {}, {})",
                self.typ, self.path, self.constraint
            ),
        }
    }
}

/// Every checkable refinement the lowered program states, then the
/// schema's.
pub fn stated(program: &Program, schema: &Schema) -> Result<Vec<Stated>> {
    let mut out = Vec::new();
    let mut diags = Vec::new();
    for s in &program.statements {
        let Stmt::Fact(a) = s else { continue };
        match Stated::of(a) {
            Some(Ok(r)) => out.push(r),
            Some(Err(e)) => diags.push(Diagnostic::error(a.span, e)),
            None => {}
        }
    }
    for a in &schema.facts {
        if let Some(r) = Stated::of(a) {
            out.push(r.map_err(|e| anyhow::anyhow!("schema: {e}"))?);
        }
    }
    if diags.is_empty() {
        Ok(out)
    } else {
        Err(Diagnostics(diags).into())
    }
}

/// The compile-time checks: a literal contribution that violates a
/// checkable refinement (both spans), and E0306, a refinement on a path the
/// schema marks `sensitive` whose provider does not check refinements.
pub fn check(program: &Program, schema: &Schema) -> Result<()> {
    let refs = stated(program, schema)?;
    let mut diags = Vec::new();
    let refined = |d: Diagnostic, r: &Stated| match r.span {
        Some(at) => d.with_label(at, format!("refined here: {}", r.constraint)),
        None => d.with_note(format!("the provider schema states {}", r.describe())),
    };
    // One E0306 per refinement written (a `check` and its declared type are
    // one site).
    let mut e0306 = BTreeSet::new();
    for r in &refs {
        if !schema.is_sensitive(&r.typ, &r.path) || schema.checks_refinements.contains(&r.typ) {
            continue;
        }
        let site = r.span.map(|s| (s.file, s.start, s.end));
        if !e0306.insert((r.typ.clone(), r.path.clone(), site)) {
            continue;
        }
        let provider = schema
            .provider_of
            .get(&r.typ)
            .map(String::as_str)
            .unwrap_or("(none)");
        let msg = format!(
            "E0306: a refinement on sensitive path {} .{} cannot be checked by the engine, \
             and provider {provider} does not check refinements",
            r.typ, r.path,
        );
        let d = match r.span {
            Some(at) => Diagnostic::error(at, msg),
            None => Diagnostic::error(Span::default(), msg)
                .with_note(format!("the provider schema states {}", r.describe())),
        };
        diags.push(d.with_help(format!(
            "dform never reads {} .{} to check it: provider {provider} checks it once its \
             schema declares `checks_refinements`; else leave the check out",
            r.typ, r.path,
        )));
    }
    for s in &program.statements {
        let head = match s {
            Stmt::Fact(a) => a,
            Stmt::Rule(r) => &r.head,
            _ => continue,
        };
        if head.pred != "arg" || head.args.len() != 5 {
            continue;
        }
        let (Some(Value::Str(typ)), Some(Value::Str(path)), Some(value)) = (
            head.args[0].ground(),
            head.args[2].ground(),
            head.args[3].ground(),
        ) else {
            continue;
        };
        let addr = head.args[1].ground();
        for r in &refs {
            // A variable address meets only the per-type refinements.
            let applies = match (&r.addr, &addr) {
                (Some(_), None) => false,
                (_, a) => r.applies(
                    &typ,
                    a.as_ref().unwrap_or(&Value::Str(String::new())),
                    &path,
                ),
            };
            if !applies || schema.is_sensitive(&r.typ, &r.path) {
                continue;
            }
            let rest = r
                .path
                .strip_prefix(path.as_str())
                .map(|p| p.trim_start_matches('.'))
                .unwrap_or("");
            let Some(v) = crate::lattice::value_at(&value, rest) else {
                continue;
            };
            if r.constraint.check(v) == Truth::False {
                let d = Diagnostic::error(
                    head.span,
                    format!(
                        "{} violates the refinement {} of {typ} .{}",
                        crate::partition::fmt_value(v),
                        r.constraint,
                        r.path
                    ),
                );
                diags.push(refined(d, r));
            }
        }
    }
    if diags.is_empty() {
        Ok(())
    } else {
        Err(Diagnostics(diags).into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lits(src: &str) -> Vec<Lit> {
        // A `check` names the attribute by its text.
        let p = crate::parser::parse_literal_text(&format!("x(1) where {src}")).unwrap();
        match p.statements.as_slice() {
            [Stmt::Rule(r)] => r.body.clone(),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn the_term_language_round_trips_through_its_text() {
        for text in [
            "range(1, 35)",
            "prefix_len_le(24)",
            "prefix_len_ge(16)",
            "enum([\"a\", \"b\"])",
            "regex(\"^[a-z]+$\")",
            "len_le(63)",
            "len_ge(16)",
            "type(int)",
        ] {
            let c = parse(text).unwrap();
            assert_eq!(c.to_string(), text);
        }
        assert!(parse("range(5, 1)").is_err());
        assert!(parse("prefix_len_le(40)").is_err());
        assert!(parse("regex(\"(\")").is_err());
        assert!(parse("between(1, 2)").is_err());
    }

    #[test]
    fn the_checks() {
        use Constraint::*;
        let net = |s: &str| {
            let (addr, prefix) = crate::value::parse_ipnet(s).unwrap();
            Value::IpNet { addr, prefix }
        };
        assert_eq!(Range(1, 35).check(&Value::Int(35)), Truth::True);
        assert_eq!(Range(1, 35).check(&Value::Int(40)), Truth::False);
        assert_eq!(PrefixLenLe(24).check(&net("10.0.0.0/24")), Truth::True);
        assert_eq!(
            PrefixLenLe(24).check(&Value::Str("10.0.0.0/26".into())),
            Truth::False
        );
        assert_eq!(PrefixLenGe(16).check(&Value::Str("x".into())), Truth::False);
        assert_eq!(LenGe(3).check(&Value::Str("abc".into())), Truth::True);
        assert_eq!(
            LenLe(1).check(&Value::List(vec![Value::Int(1); 2])),
            Truth::False
        );
        assert_eq!(
            Regex("[a-z]+".into()).check(&Value::Str("ab1".into())),
            Truth::False
        );
        assert_eq!(
            Regex("[a-z]+".into()).check(&Value::Str("ab".into())),
            Truth::True
        );
        let null = Value::Null {
            label: "t/a#p".into(),
            class: crate::value::NullClass::Open,
            ty: "string".into(),
        };
        assert_eq!(Range(1, 2).check(&null), Truth::Unknown);
    }

    #[test]
    fn a_where_splits_into_the_table_and_the_rest() {
        let (cs, rest) = split(&lits("1 <= days, days <= 35"), &["days"]);
        assert_eq!(cs, vec![Constraint::Range(1, 35)]);
        assert!(rest.is_empty());
        let (cs, rest) = split(&lits("cidr.bits == 28"), &["cidr"]);
        assert_eq!(
            cs,
            vec![Constraint::PrefixLenLe(28), Constraint::PrefixLenGe(28)]
        );
        assert!(rest.is_empty());
        let (cs, _) = split(&lits("pw.len >= 16, matches(pw, \"[a-z]+\")"), &["pw"]);
        assert_eq!(
            cs,
            vec![Constraint::LenGe(16), Constraint::Regex("[a-z]+".into())]
        );
        let (cs, _) = split(&lits("env in [dev, prod]"), &["env"]);
        assert_eq!(cs.len(), 1);
        // One bound alone, another attribute, a user predicate: the rest.
        let (cs, rest) = split(&lits("n <= 5"), &["n"]);
        assert!(cs.is_empty());
        assert_eq!(rest.len(), 1);
        let (cs, rest) = split(&lits("pw.len >= 3, max <= min, ok(pw)"), &["pw"]);
        assert_eq!(cs, vec![Constraint::LenGe(3)]);
        assert_eq!(rest.len(), 2);
    }
}
