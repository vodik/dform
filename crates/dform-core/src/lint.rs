//! A small lint pass over the lowered program: warn about an
//! `input(...)` key, from `--set` or a fact, that no rule body reads, and
//! about a stack key input that defaults to production.
//!
//! Its F13 half is a compile error now: an instance that sets a key its
//! module does not declare as an input names the key (`modules`).
//!
//! And, over an evaluation, the collision lint of a keyed stack
//! ([`key_collisions`]): a resource whose name-like attribute's value does
//! not depend on any key input ([`from_key`]) is named the same by every
//! deployment of the stack. `dform stack rekey` lists the other side
//! ([`key_named`]): the resources whose names do depend on the key, and so
//! change when it does.

use crate::ast::{Atom, Lit, Program, RuleStmt, Stmt, Term};
use crate::circuit::{self, Circuit, Leaf, NodeId, View};
use crate::engine::EvalResult;
use crate::ir::Address;
use crate::schema::Schema;
use crate::transform;
use crate::value::Value;
use std::collections::BTreeSet;

/// A name-like attribute of a resource in an evaluation: `path` (dotted,
/// maybe below the attribute `fact` holds) names the object in the cloud.
#[derive(Debug, Clone)]
pub struct Named {
    pub addr: Address,
    pub path: String,
    pub value: Value,
    /// The `attr(T, A, P, V)` fact the value is in.
    pub fact: circuit::Fact,
}

impl Named {
    fn text(&self) -> String {
        format!(
            "{} = {}",
            self.addr.attr(&self.path),
            crate::partition::fmt_value(&self.value)
        )
    }
}

/// Every name-like attribute (`Schema::name_like_paths`) of every resource
/// the evaluation wants, whose value is known (a null is minted by the
/// cloud, not written).
pub fn name_like(facts: &BTreeSet<Atom>, schema: &Schema) -> Vec<Named> {
    let str_of = |t: &Term| match t {
        Term::Val(Value::Str(s)) => Some(s.clone()),
        _ => None,
    };
    let wanted: BTreeSet<(String, String)> = facts
        .iter()
        .filter(|a| a.pred == "want" && a.args.len() == 2)
        .filter_map(|a| Some((str_of(&a.args[0])?, str_of(&a.args[1])?)))
        .collect();
    let mut out = Vec::new();
    for a in facts
        .iter()
        .filter(|a| a.pred == "attr" && a.args.len() == 4)
    {
        let (Some(t), Some(n), Some(p), Term::Val(v)) = (
            str_of(&a.args[0]),
            str_of(&a.args[1]),
            str_of(&a.args[2]),
            &a.args[3],
        ) else {
            continue;
        };
        if !wanted.contains(&(t.clone(), n.clone())) {
            continue;
        }
        for path in schema.name_like_paths(&t) {
            // The attribute is the path's first segment; below it, walk
            // the object.
            let value = if path == p {
                Some(v.clone())
            } else if let Some(rest) = path.strip_prefix(&format!("{p}.")) {
                rest.split('.').try_fold(v.clone(), |v, seg| match v {
                    Value::Obj(m) => m.get(seg).cloned(),
                    _ => None,
                })
            } else {
                None
            };
            match value {
                None | Some(Value::Null { .. }) => {}
                Some(value) => out.push(Named {
                    addr: Address {
                        typ: t.clone(),
                        name: n.clone(),
                    },
                    path,
                    value,
                    fact: crate::engine::circuit_fact(a),
                }),
            }
        }
    }
    out
}

/// Does the value of the name-like attribute `n` depend on one of the
/// stack's inputs `keys` (`attr("input", "", k, _)`)? Value dependence: a
/// key input's value flows into the value term of a rule that derived it,
/// through bindings, interpolation, calls, lookups or refs. A read that
/// only gates a rule, or feeds another field, does not count.
///
/// Within a rule the flow is static: from the variables of the head's
/// term, across `=` and every positive literal sharing one (a lookup's
/// key selects its value). Between rules it follows the circuit: each such
/// literal's matched fact, at the columns the flow reaches, and each ref's
/// attribute.
pub fn from_key(res: &EvalResult, n: &Named, keys: &[String]) -> bool {
    let Some(start) = res.circuit.fact_id(&n.fact) else {
        return false;
    };
    let below: Vec<String> = match n.fact.args.get(2) {
        Some(Value::Str(p)) => n
            .path
            .strip_prefix(&format!("{p}."))
            .map(|rest| rest.split('.').map(str::to_string).collect())
            .unwrap_or_default(),
        _ => Vec::new(),
    };
    let mut flow = Flow {
        res,
        keys,
        seen: BTreeSet::new(),
    };
    flow.fact(start, &BTreeSet::from([3]), &below)
}

struct Flow<'a> {
    res: &'a EvalResult,
    keys: &'a [String],
    seen: BTreeSet<(NodeId, BTreeSet<usize>)>,
}

impl Flow<'_> {
    fn is_key(&self, f: &circuit::Fact) -> bool {
        f.pred == "attr"
            && matches!(f.args.as_slice(),
                [Value::Str(t), Value::Str(scope), Value::Str(k), _]
                    if t == crate::modules::INPUT && scope.is_empty() && self.keys.contains(k))
    }

    /// Do columns `cols` of the fact at `id` depend on a key? `below`: the
    /// path inside the value column the question is about.
    fn fact(&mut self, id: NodeId, cols: &BTreeSet<usize>, below: &[String]) -> bool {
        if !self.seen.insert((id, cols.clone())) {
            return false;
        }
        let View::Fact { fact, alts, .. } = self.res.circuit.view(id) else {
            return false;
        };
        if self.is_key(fact) && cols.contains(&3) {
            return true;
        }
        // A ref's value is its attribute's.
        let mut refs = Vec::new();
        for &c in cols {
            if let Some(v) = fact.args.get(c) {
                refs_in(v, &mut refs);
            }
        }
        for (t, a, p) in refs {
            let top = p.split('.').next().unwrap_or(&p);
            let target = self.res.facts.iter().find(|f| {
                f.pred == "attr"
                    && matches!(f.args.as_slice(),
                        [Term::Val(Value::Str(ft)), Term::Val(Value::Str(fa)), Term::Val(Value::Str(fp)), _]
                            if *ft == t && *fa == a && fp == top)
            });
            if let Some(id) =
                target.and_then(|f| self.res.circuit.fact_id(&crate::engine::circuit_fact(f)))
                && self.fact(id, &BTreeSet::from([3]), &[])
            {
                return true;
            }
        }
        alts.iter().any(|&alt| self.firing(alt, cols, below))
    }

    fn firing(&mut self, alt: NodeId, cols: &BTreeSet<usize>, below: &[String]) -> bool {
        let View::Times { children, bindings } = self.res.circuit.view(alt) else {
            return false;
        };
        let rule = children
            .iter()
            .find_map(|&c| match self.res.circuit.view(c) {
                View::Leaf(Leaf::Rule { id }) => Some(id.clone()),
                _ => None,
            });
        let Some(rule) = rule else {
            // A fact the program states, or one given: no key flows in.
            return false;
        };
        let facts: Vec<NodeId> = children
            .iter()
            .copied()
            .filter(|&c| matches!(self.res.circuit.view(c), View::Fact { .. }))
            .collect();
        if rule == crate::engine::ATTR_SIGMA {
            // attr(T, A, P, V) over its contributions arg(T, A, P, V, R).
            return facts.iter().any(|&f| self.fact(f, cols, below));
        }
        let Some(r) = rule
            .strip_prefix('r')
            .and_then(|i| i.parse::<usize>().ok())
            .and_then(|i| self.res.rules.get(i))
        else {
            // Another aggregate: every contribution, every column.
            return facts.iter().any(|&f| self.fact(f, cols, &[]));
        };
        let terms: Vec<&Term> = cols
            .iter()
            .filter_map(|&c| r.head.args.get(c))
            .map(|t| narrow(t, below))
            .collect();
        let sources = flows_into(r, &terms);
        let bound = |t: &Term| match t {
            Term::Val(v) => Some(v.clone()),
            Term::Var(x) => bindings
                .iter()
                .find(|(k, _)| k == x)
                .map(|(_, v)| v.clone()),
            _ => None,
        };
        for (a, cols) in sources {
            let want: Vec<Option<Value>> = a.args.iter().map(bound).collect();
            for &f in &facts {
                let View::Fact { fact, .. } = self.res.circuit.view(f) else {
                    continue;
                };
                let matches = fact.pred == a.pred
                    && fact.args.len() == want.len()
                    && fact
                        .args
                        .iter()
                        .zip(&want)
                        .all(|(v, w)| w.as_ref().is_none_or(|w| w == v));
                if matches && self.fact(f, &cols, &[]) {
                    return true;
                }
            }
        }
        false
    }
}

/// The part of `t` at `path`, as far as `t` spells it out.
fn narrow<'t>(t: &'t Term, path: &[String]) -> &'t Term {
    match (t, path.split_first()) {
        (Term::Obj(m), Some((k, rest))) => m.get(k).map_or(t, |v| narrow(v, rest)),
        _ => t,
    }
}

/// A positive body literal, and the columns of it a flow reaches.
type Source<'r> = (&'r Atom, BTreeSet<usize>);

/// What flows into `terms` in rule `r`: the positive body literals, each
/// with the columns the flow reaches.
fn flows_into<'r>(r: &'r RuleStmt, terms: &[&'r Term]) -> Vec<Source<'r>> {
    let mut vars = BTreeSet::new();
    for t in terms {
        term_vars(t, &mut vars);
    }
    loop {
        let before = vars.len();
        for l in &r.body {
            let mut here = BTreeSet::new();
            match l {
                Lit::Eq(a, b) => {
                    term_vars(a, &mut here);
                    term_vars(b, &mut here);
                }
                Lit::Pos(a) => a.args.iter().for_each(|t| term_vars(t, &mut here)),
                _ => continue,
            }
            if !here.is_disjoint(&vars) {
                vars.extend(here);
            }
        }
        if vars.len() == before {
            break;
        }
    }
    r.body
        .iter()
        .filter_map(|l| match l {
            Lit::Pos(a) => Some(a),
            _ => None,
        })
        .filter_map(|a| {
            let cols: BTreeSet<usize> = a
                .args
                .iter()
                .enumerate()
                .filter(|(_, t)| {
                    let mut here = BTreeSet::new();
                    term_vars(t, &mut here);
                    !here.is_disjoint(&vars)
                })
                .map(|(i, _)| i)
                .collect();
            (!cols.is_empty()).then_some((a, cols))
        })
        .collect()
}

fn term_vars<'t>(t: &'t Term, out: &mut BTreeSet<&'t str>) {
    match t {
        Term::Var(v) => {
            out.insert(v);
        }
        Term::Func { args, .. } | Term::List(args) => args.iter().for_each(|a| term_vars(a, out)),
        Term::Obj(m) => m.values().for_each(|a| term_vars(a, out)),
        Term::Val(_) | Term::Wildcard | Term::ListComp { .. } => {}
    }
}

/// The `(type, address, path)` of every ref in `v`.
fn refs_in(v: &Value, out: &mut Vec<(String, String, String)>) {
    match v {
        Value::Ref { typ, name, attr } => out.push((typ.clone(), name.clone(), attr.clone())),
        Value::List(xs) => xs.iter().for_each(|x| refs_in(x, out)),
        Value::Obj(m) => m.values().for_each(|x| refs_in(x, out)),
        _ => {}
    }
}

/// Where the value of an `attr` fact is written: the place of the rule or
/// fact behind its first contribution.
fn owner(circuit: &Circuit, fact: &circuit::Fact) -> Option<String> {
    let mut id = circuit.fact_id(fact)?;
    // attr <- Σ <- arg <- the rule firing (or the fact) that wrote it.
    for _ in 0..8 {
        match circuit.view(id) {
            View::Fact { alts, .. } => id = *alts.first()?,
            View::Times { children, .. } => {
                for &c in children {
                    match circuit.view(c) {
                        View::Leaf(circuit::Leaf::Rule { id: r }) if !r.starts_with('Σ') => {
                            return circuit.rule_at(r).map(str::to_string);
                        }
                        // `file:line:col (arg)`: the place.
                        View::Leaf(circuit::Leaf::Base { span }) => {
                            return Some(span.split(" (").next().unwrap_or(span).to_string());
                        }
                        _ => {}
                    }
                }
                id = *children
                    .iter()
                    .find(|&&c| matches!(circuit.view(c), View::Fact { .. }))?;
            }
            View::Leaf(_) | View::Dead => return None,
        }
    }
    None
}

/// One finding of the collision lint: its message, and the `attr` fact it
/// is about.
#[derive(Debug, Clone)]
pub struct Collision {
    pub text: String,
    pub fact: circuit::Fact,
}

/// The collision lint of a keyed stack: one finding per name-like
/// attribute whose value does not depend on any key input, at the place it
/// is written. `stack` is the deployment's name.
pub fn key_collisions(
    res: &EvalResult,
    schema: &Schema,
    keys: &[String],
    stack: &str,
) -> Vec<Collision> {
    name_like(&res.facts, schema)
        .into_iter()
        .filter(|n| !from_key(res, n, keys))
        .map(|n| {
            let at = owner(&res.circuit, &n.fact)
                .map(|at| format!("{at}: "))
                .unwrap_or_default();
            let text = format!(
                "{at}{} does not depend on the stack's key ({}): every deployment of \
                 {stack} gives it this name, and they collide; derive it from the key \
                 (\"...${{{}}}\"), or say `isolated = true` on the stack when each key value \
                 deploys into its own account",
                n.text(),
                keys.join(", "),
                keys[0],
            );
            Collision { text, fact: n.fact }
        })
        .collect()
}

/// The name-like attributes that depend on a key input: what a new key
/// value renames, usually a replace. One line per attribute.
pub fn key_named(res: &EvalResult, schema: &Schema, keys: &[String]) -> Vec<String> {
    name_like(&res.facts, schema)
        .into_iter()
        .filter(|n| from_key(res, n, keys))
        .map(|n| n.text())
        .collect()
}

/// Run the lint over `program` (which is lowered internally) plus the set of
/// `--set`/`--data` keys supplied on the command line. Returns one message
/// per unread key; empty means clean.
///
/// Lowering errors are swallowed here (not this pass's job to report; the
/// real evaluation will surface them) -- an empty list is returned instead.
pub fn lint(program: &Program, cli_keys: &[String]) -> Vec<String> {
    let mut out = production_defaults(program);
    let Ok(lowered) = transform::lower(program) else {
        return out;
    };
    out.extend(lint_lowered(&lowered.program, cli_keys));
    out
}

/// A key input whose default is `"prod"` or `"production"`: a plan or
/// apply that names no value of the key is of the production deployment.
fn production_defaults(program: &Program) -> Vec<String> {
    let mut out = Vec::new();
    for s in &program.statements {
        let Stmt::Input(i) = s else { continue };
        let Some(Term::Val(Value::Str(v))) = &i.default else {
            continue;
        };
        let production = ["prod", "production"]
            .iter()
            .any(|p| v.eq_ignore_ascii_case(p));
        if !production || !i.key {
            continue;
        }
        let at = crate::diag::at(i.span)
            .map(|a| format!("{a}: "))
            .unwrap_or_default();
        // A stack is named after its file.
        let stack = crate::diag::location(i.span)
            .map(|(f, _, _)| crate::state::stack_name(std::path::Path::new(&f)))
            .unwrap_or_else(|| "the stack".into());
        out.push(format!(
            "{at}key {k} of stack {stack} defaults to \"{v}\": a plan or apply that names no \
             {k} is of {stack}[{k}={v}]; default to another value, or give none",
            k = i.name
        ));
    }
    out
}

fn lint_lowered(program: &Program, cli_keys: &[String]) -> Vec<String> {
    let mut produced: BTreeSet<String> = cli_keys.iter().cloned().collect();
    let mut read: BTreeSet<String> = BTreeSet::new();
    for stmt in &program.statements {
        match stmt {
            Stmt::Fact(a) => produced.extend(input_key(a)),
            Stmt::Rule(r) => {
                produced.extend(input_key(&r.head));
                read.extend(r.body.iter().filter_map(read_key));
            }
            _ => {}
        }
    }
    produced
        .difference(&read)
        .map(|key| format!("input(\"{key}\", _) is set but no rule body reads it"))
        .collect()
}

fn read_key(lit: &Lit) -> Option<String> {
    match lit {
        Lit::Pos(a) | Lit::Not(a) => input_key(a),
        _ => None,
    }
}

/// The key of `input(Key, Value)` when it is a literal.
fn input_key(atom: &Atom) -> Option<String> {
    match (atom.pred.as_str(), atom.args.as_slice()) {
        ("input", [Term::Val(Value::Str(k)), _]) => Some(k.clone()),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ir;
    use crate::loader;
    use std::path::PathBuf;

    fn workspace_root() -> PathBuf {
        PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/../.."))
    }

    /// Reverted-fix check: with the F13 bug back (`vpc_cidr` instead of
    /// `vpc_net`), `examples/adopt/stacks/adopt_demo.df` derives zero resources and the
    /// evaluated fact set has no `want`/`adopt` for `net.vpc`. This test
    /// fails if that bug is reintroduced.
    #[test]
    fn adopt_demo_derives_a_vpc() {
        let root = workspace_root();
        let file = root.join("examples/adopt/stacks/adopt_demo.df");
        let program = loader::load_program(&[file]).expect("load adopt_demo.df");

        let set_env_prod = Atom {
            pred: "input".to_string(),
            args: vec![
                Term::Val(Value::Str("env".to_string())),
                Term::Val(Value::Str("prod".to_string())),
            ],
            record: None,
            span: Default::default(),
        };
        let (res, warnings) =
            crate::engine::eval(&program, &[set_env_prod]).expect("eval adopt_demo.df");
        assert!(
            warnings.is_empty(),
            "unexpected engine warnings: {warnings:?}"
        );

        let wants: Vec<_> = res
            .facts
            .iter()
            .filter(|a| a.pred == "want" || a.pred == "adopt")
            .collect();
        assert!(
            !wants.is_empty(),
            "expected at least one want/adopt fact, got none -- facts: {:?}",
            res.facts
        );

        // The specific bug (F13): the VPC itself must be among the derived
        // resources, not just some unrelated want/adopt fact.
        let resources = ir::compile_resources(res.facts.iter().cloned(), &Default::default())
            .expect("compile resources");
        assert!(
            resources.iter().any(|r| r.addr.typ == "net.vpc"),
            "expected a net.vpc resource to be derived, got: {:?}",
            resources
                .iter()
                .map(|r| (&r.addr.typ, &r.addr.name))
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn warns_about_unread_cli_input() {
        let src = r#"
            env("staging")
        "#;
        let program = crate::parser::parse_program(src).expect("parse");
        let warnings = lint(&program, &["region".to_string()]);
        assert!(
            warnings.iter().any(|w| w.contains("region")),
            "expected a warning naming the unread '--set region=...' input, got: {warnings:?}"
        );
    }
}
