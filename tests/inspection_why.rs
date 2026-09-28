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
        "dform.df",
        &[
            "--set",
            "env=prod",
            "why",
            r#"attr(net.vpc, "network.main::vpc", "tags.team", "platform")"#,
        ],
    );
    assert!(
        out.contains(
            r#"arg(Type, Name, "tags", {team: "platform"}, "normal") :- want(Type, Name)"#
        ),
        "{out}"
    );
    assert!(out.contains("[rank normal, owner r"), "{out}");
    assert!(out.contains("... 1 other contribution (--all)"), "{out}");
    golden("why_dform_prod_tag", &out);
}

/// Every contribution of an aggregate, and bindings, and a given input.
#[test]
fn why_an_attribute_shows_every_contribution() {
    let out = dform(
        "dform.df",
        &[
            "--set",
            "env=prod",
            "why",
            r#"attr(net.vpc, "network.main::vpc", tags, X)"#,
        ],
    );
    assert!(out.contains("by Σattr: attribute aggregate"), "{out}");
    assert!(out.contains("over 2 contributions"), "{out}");
    assert!(out.contains(r#"with Env = "prod""#), "{out}");
    assert!(out.contains("input --set env=prod"), "{out}");
    assert!(out.contains("(see above)"), "{out}");
    // The pack's tag, the module's tags, and the instance's input.
    assert_eq!(out.matches("[rank normal, owner").count(), 4, "{out}");
}

#[test]
fn why_prints_one_alternative_unless_all() {
    let s = Scratch::new("why-alts");
    s.write(
        "p.df",
        "edition 2026.\np(1). q(1). r(X) :- p(X). r(X) :- q(X). s(X) :- r(X), not t(X). t(2) :- p(2).",
    );
    let why = |extra: &[&str]| {
        let mut a = vec!["--file", "p.df", "--world", "w.json", "why"];
        a.extend(extra);
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
        all.contains("fact, p.df:2:1 (p)") && all.contains("fact, p.df:2:7 (q)"),
        "{all}"
    );

    let none = s
        .run(&["--file", "p.df", "--world", "w.json", "why", "s(7)"])
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
    s.write("p.df", "edition 2026.\np(1). p(2). q(X) :- p(X).");
    let out = s
        .run(&["--file", "p.df", "--world", "w.json", "why", "q(N)"])
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
        r#"edition 2026.
resource leaky.vault v { password = "VAULT-SECRET-DO-NOT-PRINT" }.
           copy(P) :- attr(leaky.vault, v, password, P)."#,
    );
    let schema = repo().join("providers/leaky/schema.df");
    let out = s
        .run(&[
            "--file",
            "p.df",
            "--provider",
            schema.to_str().unwrap(),
            "--world",
            "w.json",
            "why",
            "copy(X)",
        ])
        .success()
        .stdout;
    assert!(!out.contains("VAULT-SECRET"), "{out}");
    assert!(out.contains("(sensitive leaky.vault/v#password)"), "{out}");
}
