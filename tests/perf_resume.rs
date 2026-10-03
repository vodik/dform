//! The policy pass (E §2.8) continues an evaluation with the deformation
//! facts instead of repeating it (`engine::Resumable`): its result must be
//! the fresh evaluation's with those facts given.

mod common;
use common::repo;
use dform::ast::{Atom, Term};
use dform::value::Value;

fn atom(pred: &str, args: Vec<Term>) -> Atom {
    Atom {
        pred: pred.into(),
        args,
        record: None,
        span: Default::default(),
    }
}

fn s(x: &str) -> Term {
    Term::Val(Value::Str(x.into()))
}

fn check(file: &str, provider: &str, env: Option<&str>, append: &str) {
    let schema = dform::schema::load_provider(provider).unwrap();
    let mut program = dform::loader::load_program(&[repo().join(file)]).unwrap();
    program
        .statements
        .extend(dform::parser::parse_program(append).unwrap().statements);
    let program = dform::zset::with_policy_rules(program).unwrap();
    let mut extra: Vec<Atom> = env
        .map(|e| atom("input", vec![s("env"), s(e)]))
        .into_iter()
        .collect();
    extra.extend(schema.facts.clone());
    let (first, _, resumable) =
        dform::engine::eval_resumable(&program, &extra, dform::zset::POLICY_INPUTS).unwrap();
    // A deformation of every kind the lifecycle rules read, for every
    // resource, and prevent_destroy on each: every policy rule fires.
    let mut more = vec![];
    for (k, w) in first.facts.iter().filter(|a| a.pred == "want").enumerate() {
        let kind = ["delete", "replace", "pending", "create"][k % 4];
        let [Term::Val(Value::Str(t)), Term::Val(Value::Str(n))] = w.args.as_slice() else {
            panic!("{w:?}");
        };
        let r = Term::Val(Value::Ref {
            typ: t.clone(),
            name: n.clone(),
            attr: String::new(),
        });
        more.push(atom("deformation", vec![s(kind), r.clone(), s("before")]));
        more.push(atom("world_digest", vec![r.clone(), s("now")]));
        more.push(atom("lifecycle", vec![r, s("prevent_destroy")]));
    }
    assert!(!more.is_empty(), "{file}: no resources");
    let (resumed, v1) = resumable.with(&more).unwrap();
    let mut all = extra.clone();
    all.extend(more);
    let (fresh, v2) = dform::engine::eval(&program, &all).unwrap();
    assert!(v2.len() > first.facts.iter().filter(|a| a.pred == "deny").count());
    assert_eq!(v1, v2, "{file}: violations");
    assert_eq!(resumed.facts, fresh.facts, "{file}: facts");
    assert_eq!(resumed.warnings, fresh.warnings, "{file}: warnings");
    assert_eq!(resumed.stuck, fresh.stuck, "{file}: stuck");
    let facts = fresh.circuit.facts();
    assert_eq!(resumed.circuit.facts(), facts, "{file}: circuit facts");
    for f in &facts {
        assert_eq!(
            resumed.circuit.why(f),
            fresh.circuit.why(f),
            "{file}: why {f:?}"
        );
    }
}

#[test]
fn the_policy_pass_resumed_is_the_policy_pass() {
    check("examples/demo/stacks/dform.df", "fake", Some("prod"), "");
    check("examples/pngu/stacks/pngu.df", "gke", Some("prod"), "");
    check("examples/gke/stacks/gke_two_phase.df", "gke", None, "");
    // stuck/4 counts the instances of the rules that read the deformation
    // (this one is stuck on the cluster's zones once a deformation names it).
    check(
        "examples/gke/stacks/gke_two_phase.df",
        "gke",
        None,
        "deny \"strict\" { rule: r } where stuck(r, _, _, _)\n         deny \"zone\" { z: z } where {\n           deformation(_, google.container_cluster[_], _)\n           arg(\"google.container_cluster\", \"pngu\", \"zones\", zs)\n           member(zs, z)\n         }",
    );
}
