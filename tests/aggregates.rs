//! Aggregates (R-59): `n = agg(x)` in a body folds `x` over the body's
//! matches per group of the head's other variables; `let n = agg(x) where
//! B` folds over one group. An aggregate is bound in a body and nowhere
//! else, and what it folds is bound by the rest of the body.

mod common;
use common::{Scratch, error, facts};
use dform_core::engine;
use dform_core::parser::parse_program;

const SUBNETS: &str = r#"subnet("a", "vpc-1")
subnet("b", "vpc-1")
subnet("c", "vpc-1")
subnet("d", "vpc-2")
"#;

/// The groups are the head's other variables: one count per vpc.
#[test]
fn a_count_groups_by_the_heads_other_variables() {
    let src = format!("{SUBNETS}per_vpc(v, n) where n = count(s), subnet(s, v)\n");
    assert_eq!(
        facts(&src, "per_vpc"),
        [r#"per_vpc("vpc-1", 3)"#, r#"per_vpc("vpc-2", 1)"#]
    );
    // The literal may stand anywhere in the body.
    let src = format!("{SUBNETS}per_vpc(v, n) where subnet(s, v), n = count(s)\n");
    assert_eq!(
        facts(&src, "per_vpc"),
        [r#"per_vpc("vpc-1", 3)"#, r#"per_vpc("vpc-2", 1)"#]
    );
}

/// A head with no other variable is one group, as a `let` is.
#[test]
fn a_let_aggregate_groups_by_nothing() {
    let src = format!("{SUBNETS}let n = count(s) where subnet(s, _)\nall_subnets(x) where x = n\n");
    assert_eq!(facts(&src, "all_subnets"), ["all_subnets(4)"]);
    let src = format!("{SUBNETS}total(n) where n = count(s), subnet(s, _)\n");
    assert_eq!(facts(&src, "total"), ["total(4)"]);
}

/// A literal that reads the result is applied after the fold, and what it
/// reads of the body joins the group.
#[test]
fn a_literal_after_the_fold_reads_the_result() {
    let src = format!(
        "{SUBNETS}crowded(v) where n = count(s), subnet(s, v), n > 2\nlimit(\"vpc-1\", 5)\nlimit(\"vpc-2\", 0)\nover(v, n) where n = count(s), subnet(s, v), limit(v, m), n > m\n"
    );
    assert_eq!(facts(&src, "crowded"), [r#"crowded("vpc-1")"#]);
    assert_eq!(facts(&src, "over"), [r#"over("vpc-2", 1)"#]);
    let src = format!(
        "{SUBNETS}deny \"too many subnets in ${{v}}: ${{n}}\" where n = count(s), subnet(s, v), n > 2\n"
    );
    let deny = facts(&src, "deny");
    assert_eq!(deny.len(), 1, "{deny:?}");
    assert!(deny[0].contains("too many subnets in vpc-1: 3"), "{deny:?}");
}

/// Two aggregates over one body are joined by their group.
#[test]
fn two_aggregates_share_the_group() {
    let src = r#"size("a", "x", 3)
size("b", "x", 4)
size("c", "y", 5)
stats(g, n, t) where n = count(s), t = sum(z), size(s, g, z)
"#;
    assert_eq!(
        facts(src, "stats"),
        [r#"stats("x", 2, 7)"#, r#"stats("y", 1, 5)"#]
    );
}

#[test]
fn every_aggregate_folds() {
    let src = r#"v("a", 3, true, "p")
v("b", 1, false, "q")
v("c", 3, true, "r")
c(n) where n = count(k), v(k, _, _, _)
s(n) where n = sum(x), v(_, x, _, _)
lo(n) where n = min(x), v(_, x, _, _)
hi(n) where n = max(t), v(_, _, _, t)
some(b) where b = any(x), v(_, _, x, _)
every(b) where b = all(x), v(_, _, x, _)
every_set(b) where b = all(x), v(_, 3, x, _)
set(l) where l = collect_set(x), v(_, x, _, _)
list(l) where l = collect_list(x), v(_, x, _, _)
"#;
    assert_eq!(facts(src, "c"), ["c(3)"]);
    assert_eq!(facts(src, "s"), ["s(7)"]);
    assert_eq!(facts(src, "lo"), ["lo(1)"]);
    assert_eq!(facts(src, "hi"), [r#"hi("r")"#]);
    assert_eq!(facts(src, "some"), ["some(true)"]);
    assert_eq!(facts(src, "every"), ["every(false)"]);
    assert_eq!(facts(src, "every_set"), ["every_set(true)"]);
    assert_eq!(facts(src, "set"), ["set([1, 3])"]);
    assert_eq!(facts(src, "list"), ["list([3, 1, 3])"]);
}

/// `any` and `all` fold bools: another kind is a deny naming the group.
#[test]
fn any_over_a_non_bool_is_a_deny() {
    let program = parse_program("v(1)\nsome(b) where b = any(x), v(x)\n").unwrap();
    let (r, violations) = engine::eval(&program, &[]).unwrap();
    assert!(!r.facts.iter().any(|a| a.pred == "some"));
    assert!(
        violations
            .iter()
            .any(|v| v.contains("any() over 1, which is not a bool")),
        "{violations:?}"
    );
}

/// `collect_list` is in the order of the body's rows: the first relation
/// read in order, a list's elements in theirs; not sorted by value.
#[test]
fn collect_list_keeps_the_rows_order() {
    let src = r#"zones("c", ["z3", "z1", "z2"])
order(l) where l = collect_list(z), zones(_, zs), z in zs
pair(1, "b")
pair(2, "a")
pair(3, "c")
by(l) where l = collect_list(x), pair(_, x)
"#;
    assert_eq!(facts(src, "order"), [r#"order(["z3", "z1", "z2"])"#]);
    // `pair` in its order (by its first column), not sorted by `x`.
    assert_eq!(facts(src, "by"), [r#"by(["b", "a", "c"])"#]);
    // A comprehension lowers to `collect_list`: the same order.
    let src = r#"zones("c", ["z3", "z1", "z2"])
order(l) where l = [z | zones(_, zs), z in zs]
"#;
    assert_eq!(facts(src, "order"), [r#"order(["z3", "z1", "z2"])"#]);
}

/// An empty group derives nothing: a `let` aggregate over no match has no
/// value.
#[test]
fn an_empty_group_derives_nothing() {
    let src = "decl subnet(s, v)\nlet n = count(s) where subnet(s, _)\nseen(x) where x = n\n";
    assert!(facts(src, "seen").is_empty());
}

#[test]
fn an_aggregate_of_an_unbound_name_is_an_error() {
    let e = error("subnet(\"a\", \"v\")\np(v, n) where n = count(x), subnet(_, v)\n");
    assert!(
        e.contains("`count(x)` aggregates `x`, which the body does not bind"),
        "{e}"
    );
    let e = error("let n = sum(x) where subnet(_, _)\n");
    assert!(
        e.contains("`sum(x)` aggregates `x`, which the body does not bind"),
        "{e}"
    );
}

/// An aggregate is bound in a body: in a head, an argument or a field it
/// is an error that says so.
#[test]
fn an_aggregate_elsewhere_is_an_error() {
    for src in [
        "q(1)\np(count(x)) where q(x)\n",
        "q(1)\np(y) where q(x), y = list.sum([count(x)])\n",
        "q(1)\nresource net.vpc v { size = count(x) } where q(x)\n",
    ] {
        let e = error(src);
        assert!(
            e.contains("`count` is an aggregate: it is bound in a body, `n = count(x)`"),
            "{src}: {e}"
        );
    }
    let e = error("q(1)\np(x) where q(x), not { n = count(y), q(y) }\n");
    assert!(
        e.contains("an aggregate is bound at the top of a rule's body"),
        "{e}"
    );
}

/// `why` of an aggregate's fact shows the rows of its group.
#[test]
fn why_shows_the_groups_rows() {
    let s = Scratch::project("lang-aggregates");
    s.write(
        "p.df",
        "\nuse fake\n\nsubnet(\"a\", \"vpc-1\")\nsubnet(\"b\", \"vpc-1\")\nsubnet(\"c\", \"vpc-2\")\n\nper_vpc(v, n) where n = count(s), subnet(s, v)\ncrowded(v) where n = count(s), subnet(s, v), n > 1\n",
    );
    let r = s.run(&["why", "per_vpc(\"vpc-1\", _)", "p.df"]).success();
    assert!(
        r.stdout
            .contains("per_vpc(v, n) where n = count(s), subnet(s, v)"),
        "{}",
        r.stdout
    );
    assert!(
        r.stdout.contains("subnet(\"a\", \"vpc-1\")"),
        "{}",
        r.stdout
    );
    assert!(
        r.stdout.contains("subnet(\"b\", \"vpc-1\")"),
        "{}",
        r.stdout
    );
    assert!(!r.stdout.contains("subnet(\"c\""), "{}", r.stdout);
    // Folded in a rule of the compiler's: the group's rows all the same.
    let r = s.run(&["why", "crowded(_)", "p.df"]).success();
    assert!(
        r.stdout.contains("with v = \"vpc-1\", n = 2"),
        "{}",
        r.stdout
    );
    assert!(
        r.stdout.contains("subnet(\"b\", \"vpc-1\")"),
        "{}",
        r.stdout
    );
    assert!(!r.stdout.contains("__agg"), "{}", r.stdout);
}
