//! A small lint pass over the lowered program: warn about an
//! `input(...)` key, from `--set` or a fact, that no rule body reads.
//!
//! Its F13 half is a compile error now: an instance that sets a key its
//! module does not declare as an input names the key (`modules`).
//!
//! And, over an evaluation, the collision lint of a keyed stack
//! ([`key_collisions`]): a resource whose name-like attribute has no
//! provenance path from any key input is named the same by every
//! deployment of the stack. `dform stack rekey` lists the other side
//! ([`key_named`]): the resources whose names do depend on the key, and so
//! change when it does.

use crate::ast::{Atom, Lit, Program, Stmt, Term};
use crate::circuit::{self, Circuit, View};
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
            "{}.{} {} = {}",
            self.addr.typ,
            self.addr.name,
            self.path,
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

/// Does `fact`'s provenance reach one of the stack's inputs `keys`
/// (`attr("input", "", k, _)`)? Every derivation is followed, the reads
/// that only gate a rule included.
pub fn from_key(circuit: &Circuit, fact: &circuit::Fact, keys: &[String]) -> bool {
    let is_key = |f: &circuit::Fact| {
        f.pred == "attr"
            && matches!(f.args.as_slice(),
                [Value::Str(t), Value::Str(scope), Value::Str(k), _]
                    if t == crate::modules::INPUT && scope.is_empty() && keys.contains(k))
    };
    let Some(start) = circuit.fact_id(fact) else {
        return false;
    };
    let mut seen = BTreeSet::new();
    let mut todo = vec![start];
    while let Some(id) = todo.pop() {
        if !seen.insert(id) {
            continue;
        }
        match circuit.view(id) {
            View::Fact { fact, alts, .. } => {
                if is_key(fact) {
                    return true;
                }
                todo.extend_from_slice(alts);
            }
            View::Times { children, .. } => todo.extend_from_slice(children),
            View::Leaf(_) | View::Dead => {}
        }
    }
    false
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

/// The collision lint of a keyed stack: one message per name-like
/// attribute with no provenance path from any key input, at the place it
/// is written. `stack` is the deployment's name.
pub fn key_collisions(
    facts: &BTreeSet<Atom>,
    circuit: &Circuit,
    schema: &Schema,
    keys: &[String],
    stack: &str,
) -> Vec<String> {
    name_like(facts, schema)
        .into_iter()
        .filter(|n| !from_key(circuit, &n.fact, keys))
        .map(|n| {
            let at = owner(circuit, &n.fact)
                .map(|at| format!("{at}: "))
                .unwrap_or_default();
            format!(
                "{at}{} does not depend on the stack's key ({}): every deployment of \
                 {stack} gives it this name, and they collide; derive it from the key \
                 (\"...{{{}}}\"), or say `isolated = true` on the stack when each key value \
                 deploys into its own account",
                n.text(),
                keys.join(", "),
                keys[0],
            )
        })
        .collect()
}

/// The name-like attributes that depend on a key input: what a new key
/// value renames, usually a replace. One line per attribute.
pub fn key_named(
    facts: &BTreeSet<Atom>,
    circuit: &Circuit,
    schema: &Schema,
    keys: &[String],
) -> Vec<String> {
    name_like(facts, schema)
        .into_iter()
        .filter(|n| from_key(circuit, &n.fact, keys))
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
    let Ok(lowered) = transform::lower(program) else {
        return Vec::new();
    };
    lint_lowered(&lowered.program, cli_keys)
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
            Stmt::Constraint(c) => read.extend(c.body.iter().filter_map(read_key)),
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
    /// `vpc_net`), `examples/adopt_demo.df` derives zero resources and the
    /// evaluated fact set has no `want`/`adopt` for `net.vpc`. This test
    /// fails if that bug is reintroduced.
    #[test]
    fn adopt_demo_derives_a_vpc() {
        let root = workspace_root();
        let file = root.join("examples/adopt_demo.df");
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
