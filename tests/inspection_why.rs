//! `dform why`: the derivation tree read from the provenance circuit.

mod common;
mod inspection_common;
use common::{Scratch, repo};
use inspection_common::{dform, golden};

/// The ticket's Play line: ask why a tag exists, see the rule that added it
/// (the baseline policy pack's `arg_add ... tags`) and not the module's own
/// tags.
#[test]
fn why_a_tag_exists() {
    let out = dform(
        "examples/demo/stacks/dform.df env=prod",
        &[
            "why",
            r#"attr(net.vpc, "network.main::vpc", "tags.team", "platform")"#,
        ],
    );
    assert!(
        out.contains(r#"arg(Type, R, "tags", {team: "platform"}, "normal") :- want(Type, R)"#),
        "{out}"
    );
    assert!(out.contains("[rank normal, owner r"), "{out}");
    assert!(out.contains("... 1 other contribution (--all)"), "{out}");
    golden("why_dform_prod_tag", &out);
}

/// H-16: an address as plan prints it is a `why` and a `query` argument.
/// `T["A"]` explains the want, `T["A"].path` the attribute; any other
/// spelling of an address is refused.
#[test]
fn why_and_query_take_an_address_as_plan_prints_it() {
    let at = "examples/demo/stacks/dform.df env=prod";
    let want = dform(at, &["why", r#"net.vpc["network.main::vpc"]"#]);
    assert!(
        want.starts_with("want(\"net.vpc\", \"network.main::vpc\")\n"),
        "{want}"
    );
    let tag = dform(at, &["why", r#"net.vpc["network.main::vpc"].tags.team"#]);
    assert!(
        tag.contains(r#"arg(Type, R, "tags", {team: "platform"}, "normal") :- want(Type, R)"#),
        "{tag}"
    );
    let cidr = dform(at, &["query", r#"net.vpc["network.main::vpc"].cidr"#]);
    assert!(cidr.contains("10.20.0.0/16"), "{cidr}");
    let all = dform(at, &["query", r#"net.vpc["network.main::vpc"]"#]);
    assert!(
        all.contains("\"tags\"") && all.contains("\"cidr\""),
        "{all}"
    );
    let s = Scratch::new("why-old-address");
    common::copy_dir(&repo().join("examples/demo"), &s.dir);
    let r = s
        .run(&["why", "net.vpc/network.main::vpc", "dform", "env=prod"])
        .failure();
    assert!(r.stderr.contains("cannot parse"), "{}", r.stderr);
}

/// Every contribution of an aggregate, and bindings, and a given input.
#[test]
fn why_an_attribute_shows_every_contribution() {
    let out = dform(
        "examples/demo/stacks/dform.df env=prod",
        &["why", r#"attr(net.vpc, "network.main::vpc", "tags", X)"#],
    );
    assert!(out.contains("by Σattr: attribute aggregate"), "{out}");
    assert!(out.contains("over 2 contributions"), "{out}");
    assert!(out.contains(r#"with Env = "prod""#), "{out}");
    assert!(out.contains("input --set env=prod"), "{out}");
    assert!(out.contains("(see above)"), "{out}");
    // The pack's tag, the module's tags, the instance's input and the
    // stack input --set gives.
    assert_eq!(out.matches("[rank normal, owner").count(), 5, "{out}");
}

#[test]
fn why_prints_one_alternative_unless_all() {
    let s = Scratch::new("why-alts");
    s.write(
        "p.df",
        "edition 2027\np(1)\nq(1)\nr(x) if p(x)\nr(x) if q(x)\ns(x) if r(x), not t(x)\nt(2) if p(2)",
    );
    let why = |extra: &[&str]| {
        let mut a = vec!["dev", "--world", "w.json", "why"];
        a.extend(extra);
        a.push("p.df");
        s.run(&a).success().stdout
    };
    let one = why(&["s(1)"]);
    assert!(one.contains("... 1 more alternative (--all)"), "{one}");
    assert!(one.contains("not t(1)   (absent)"), "{one}");
    let all = why(&["s(X)", "--all"]);
    assert!(
        all.contains("alternative 1 of 2:") && all.contains("alternative 2 of 2:"),
        "{all}"
    );
    assert!(
        all.contains("fact, p.df:2:1 (p)") && all.contains("fact, p.df:3:1 (q)"),
        "{all}"
    );

    let none = s
        .run(&["dev", "--world", "w.json", "why", "s(7)", "p.df"])
        .failure();
    assert!(
        none.stderr.contains("no fact matches s(7)"),
        "{}",
        none.stderr
    );
}

/// A variable matches every fact; each gets its own tree.
#[test]
fn why_with_a_variable_prints_each_match() {
    let s = Scratch::new("why-vars");
    s.write("p.df", "edition 2027\np(1)\np(2)\nq(x) if p(x)");
    let out = s
        .run(&["dev", "--world", "w.json", "why", "q(N)", "p.df"])
        .success()
        .stdout;
    assert!(
        out.starts_with("q(1)\n") && out.contains("\n\nq(2)\n"),
        "{out}"
    );
}

/// The never-prints claim holds for why: the tree over a secret prints its
/// label, also where a rule forwards it into another predicate.
#[test]
fn why_never_prints_a_labeled_secret() {
    let s = Scratch::new("why-secret");
    s.write(
        "p.df",
        r#"edition 2027
resource leaky.vault v { password = "VAULT-SECRET-DO-NOT-PRINT" }
           copy(p) if attr(leaky.vault, "v", "password", p)"#,
    );
    let schema = repo().join("tests/fixtures/providers/leaky/schema.df");
    let out = s
        .run(&[
            "dev",
            "--provider",
            schema.to_str().unwrap(),
            "--world",
            "w.json",
            "why",
            "copy(X)",
            "p.df",
        ])
        .success()
        .stdout;
    assert!(!out.contains("VAULT-SECRET"), "{out}");
    assert!(
        out.contains("(sensitive leaky.vault[\"v\"].password)"),
        "{out}"
    );
}

/// The deformation the planner hands back for the policy pass is the
/// plan's, not the world's: `why` labels it `plan`, and a refreshed world
/// fact keeps `world (refresh)`.
#[test]
fn why_labels_planner_facts_as_the_plan() {
    let s = Scratch::new("why-plan-leaf");
    s.write(
        "p.df",
        "edition 2027\nresource net.vpc main { cidr = \"10.0.0.0/16\" }\n",
    );
    s.run(&["dev", "--world", "w.json", "apply", "p.df"])
        .success();
    s.write(
        "p.df",
        "edition 2027\nlifecycle(net.vpc, \"main\", \"prevent_destroy\")\nseen(a) if identity(net.vpc, a, _)\n",
    );
    let why = |q: &str| {
        s.run(&["dev", "--world", "w.json", "why", q, "p.df"])
            .success()
            .stdout
    };
    let out = why("deny(M)");
    let line = out
        .lines()
        .find(|l| l.contains("─ deformation(\"delete\""))
        .unwrap_or_else(|| panic!("{out}"));
    assert!(line.ends_with("   plan"), "{out}");
    assert!(!out.contains("world (refresh)"), "{out}");
    let out = why("seen(A)");
    assert!(out.contains("   world (refresh)"), "{out}");
}

/// Facts injected at an apply tick (a boundary, a resume) are labelled
/// with it, and the resume stop is a deny derived from them.
#[test]
fn why_labels_facts_injected_at_a_tick() {
    use dform::ast::{Atom, Lit, Term};
    use dform::value::Value;
    let program =
        dform::zset::with_policy_rules(dform::parser::parse_program("edition 2027\n").unwrap())
            .unwrap();
    let s = |x: &str| Term::Val(Value::Str(x.into()));
    let fact = |pred: &str, args: Vec<Term>| Atom {
        pred: pred.into(),
        args,
        record: None,
        span: Default::default(),
    };
    let facts = [
        fact(
            "deformation",
            vec![s("remaining"), s("net.subnet"), s("a"), s("d1")],
        ),
        fact("world_digest", vec![s("net.subnet"), s("a"), s("d2")]),
    ];
    let (res, denies) = dform::engine::eval_at(&program, &facts, Some(3)).unwrap();
    assert_eq!(
        denies,
        ["the world changed under a remaining action: net.subnet[\"a\"]"]
    );
    let schema = dform::schema::Schema::default();
    let redact = dform::query::Redactor::new(&res.facts, &schema);
    let printer = dform::why::Printer {
        circuit: &res.circuit,
        redact: &redact,
        all: false,
    };
    let dform::query::Query::Body { body, .. } = dform::query::parse("deny(M)").unwrap() else {
        unreachable!()
    };
    let [Lit::Pos(pat)] = body.as_slice() else {
        unreachable!()
    };
    let (deny, _) = dform::why::find(pat, &res.facts).unwrap().remove(0);
    let id = res
        .circuit
        .fact_id(&dform::engine::circuit_fact(&deny))
        .unwrap();
    let out = printer.tree(id, None);
    assert_eq!(out.matches("   plan (tick 3)\n").count(), 2, "{out}");
}
