//! A small lint pass over the lowered program: warn about an
//! `input(...)` key, from `--set` or a fact, that no rule body reads.
//!
//! Its F13 half is a compile error now: an instance that sets a key its
//! module does not declare as an input names the key (`modules`).

use crate::ast::{Atom, Lit, Program, Stmt, Term};
use crate::transform;
use crate::value::Value;
use std::collections::BTreeSet;

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
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
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
