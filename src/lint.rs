//! A small lint pass over the lowered program (F13's aftermath): warn about
//! an `input(...)`/`param(...)` key that no rule body reads, and about a
//! `--set` input that no rule body reads either.
//!
//! F13 was exactly this shape: `use network main { vpc_cidr = VpcCidr }`
//! produced `param("vpc_cidr", ...)`, but `modules/network.df` reads
//! `param(vpc_net, VpcNet)` -- a different key -- so the module silently
//! derived nothing and the plan came out empty. This pass would have caught
//! it: `vpc_cidr` is produced but never read.
//!
//! `param` is scoped per component instance after lowering (`param(Scope,
//! Key, Value)`); `input` is a flat, unscoped `input(Key, Value)`. Both keys
//! must be ground (a literal string) to be checked; a computed key is out of
//! scope for this pass.

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
    let mut produced_input: BTreeSet<String> = cli_keys.iter().cloned().collect();
    let mut produced_param: BTreeSet<(String, String)> = BTreeSet::new();
    let mut read_input: BTreeSet<String> = BTreeSet::new();
    let mut read_param: BTreeSet<(String, String)> = BTreeSet::new();

    for stmt in &program.statements {
        match stmt {
            Stmt::Fact(a) => record_produced(a, &mut produced_input, &mut produced_param),
            Stmt::Rule(r) => {
                record_produced(&r.head, &mut produced_input, &mut produced_param);
                for lit in &r.body {
                    record_read(lit, &mut read_input, &mut read_param);
                }
            }
            Stmt::Constraint(c) => {
                for lit in &c.body {
                    record_read(lit, &mut read_input, &mut read_param);
                }
            }
            _ => {}
        }
    }

    let mut warnings = Vec::new();
    for key in &produced_input {
        if !read_input.contains(key) {
            warnings.push(format!(
                "input(\"{key}\", _) is set but no rule body reads it"
            ));
        }
    }
    for (scope, key) in &produced_param {
        if !read_param.contains(&(scope.clone(), key.clone())) {
            warnings.push(format!(
                "param(\"{key}\", _) in scope '{scope}' is set but no rule body reads it"
            ));
        }
    }
    warnings
}

fn record_produced(
    atom: &Atom,
    produced_input: &mut BTreeSet<String>,
    produced_param: &mut BTreeSet<(String, String)>,
) {
    match key_of(atom) {
        Some(Key::Input(k)) => {
            produced_input.insert(k);
        }
        Some(Key::Param(scope, k)) => {
            produced_param.insert((scope, k));
        }
        None => {}
    }
}

fn record_read(
    lit: &Lit,
    read_input: &mut BTreeSet<String>,
    read_param: &mut BTreeSet<(String, String)>,
) {
    let atom = match lit {
        Lit::Pos(a) | Lit::Not(a) => a,
        _ => return,
    };
    match key_of(atom) {
        Some(Key::Input(k)) => {
            read_input.insert(k);
        }
        Some(Key::Param(scope, k)) => {
            read_param.insert((scope, k));
        }
        None => {}
    }
}

enum Key {
    Input(String),
    Param(String, String),
}

/// After full lowering, `input` is `input(Key, Value)` (arity 2, unscoped)
/// and `param` is `param(Scope, Key, Value)` (arity 3, scoped per component
/// instance) -- see `transform::rewrite_atom`.
fn key_of(atom: &Atom) -> Option<Key> {
    match atom.pred.as_str() {
        "input" if atom.args.len() == 2 => literal_str(&atom.args[0]).map(Key::Input),
        "param" if atom.args.len() == 3 => {
            let scope = literal_str(&atom.args[0])?;
            let key = literal_str(&atom.args[1])?;
            Some(Key::Param(scope, key))
        }
        _ => None,
    }
}

fn literal_str(t: &Term) -> Option<String> {
    match t {
        Term::Val(Value::Str(s)) => Some(s.clone()),
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
    fn warns_about_unread_param_key() {
        // Mirrors F13 exactly: an `instance` block sets `vpc_cidr`, the module
        // it instantiates reads `vpc_net`.
        let src = r#"
            module widget {
              resource net.vpc vpc {
                cidr = VpcNet
              } :-
                param(vpc_net, VpcNet).
            }.

            instance widget main {
              vpc_cidr = "10.0.0.0/16"
            }.
        "#;
        let program = crate::parser::parse_program(src).expect("parse");
        let warnings = lint(&program, &[]);
        assert!(
            warnings.iter().any(|w| w.contains("vpc_cidr")),
            "expected a warning naming the unread 'vpc_cidr' param, got: {warnings:?}"
        );
    }

    #[test]
    fn no_warning_when_everything_is_read() {
        let src = r#"
            module widget {
              resource net.vpc vpc {
                cidr = VpcNet
              } :-
                param(vpc_net, VpcNet).
            }.

            instance widget main {
              vpc_net = "10.0.0.0/16"
            }.
        "#;
        let program = crate::parser::parse_program(src).expect("parse");
        let warnings = lint(&program, &[]);
        assert!(warnings.is_empty(), "unexpected warnings: {warnings:?}");
    }

    #[test]
    fn warns_about_unread_cli_input() {
        let src = r#"
            env(staging).
        "#;
        let program = crate::parser::parse_program(src).expect("parse");
        let warnings = lint(&program, &["region".to_string()]);
        assert!(
            warnings.iter().any(|w| w.contains("region")),
            "expected a warning naming the unread '--set region=...' input, got: {warnings:?}"
        );
    }
}
