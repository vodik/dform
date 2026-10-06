//! Every diagnostic names where: syntax errors, lowering errors, undefined
//! predicates, negative cycles, and `why`'s owners (ticket "Spans through
//! lowering to every diagnostic").

mod common;
use common::Scratch;

fn plan(src: &str) -> common::Run {
    let s = Scratch::new("diag");
    s.write("p.df", src);
    s.run(&["dev", "--world", "w.json", "plan", "p.df"])
}

/// A syntax error prints through ariadne: the location, the source line
/// and what was expected; every error in the file, then a count.
#[test]
fn syntax_errors_print_with_their_source_line() {
    let r = plan("\np(\"a\") where q(]\nr(\"b\") where ,\nuse fake\n").failure();
    assert!(
        r.stderr.contains("p.df:2:16: expected a term, found `]`"),
        "{}",
        r.stderr
    );
    assert!(
        r.stderr.contains("p.df:3:14: expected a term, found `,`"),
        "{}",
        r.stderr
    );
    assert!(r.stderr.contains("p(\"a\") where q(]"), "{}", r.stderr);
    assert!(r.stderr.ends_with("2 errors\n"), "{}", r.stderr);
}

#[test]
fn an_undefined_predicate_names_its_literal() {
    let r = plan("\nenv(\"prod\")\nq(x) where envv(x)\nuse fake\n").failure();
    assert!(
        r.stderr.contains("p.df:3:12: undefined predicate envv/1"),
        "{}",
        r.stderr
    );
    assert!(
        r.stderr.contains("in rule: q(X) :- envv(X)"),
        "{}",
        r.stderr
    );
}

/// A call to a function the evaluator does not have would have no value
/// and fail its literal quietly; it is a compile error at the call, in a
/// rule body and in a resource field alike. An aggregate is bound in a
/// body (`n = count(x)`, R-59) and nowhere else.
#[test]
fn an_unknown_function_names_its_call() {
    let r = plan("\nenv(\"prod\")\nq(y) where env(x), y = lowr(x)\nuse fake\n").failure();
    assert!(
        r.stderr.contains("p.df:3:24: unknown function lowr"),
        "{}",
        r.stderr
    );
    assert!(
        r.stderr.contains("the functions are bytes, cloud_ref,"),
        "{}",
        r.stderr
    );
    let r = plan("\nresource net.a x {\n  name = uper(\"x\")\n}\nuse fake\n").failure();
    assert!(
        r.stderr.contains("p.df:3:10: unknown function uper"),
        "{}",
        r.stderr
    );
    let r = plan("\nenv(\"prod\")\nq(count(x)) where env(x)\nuse fake\n").failure();
    assert!(
        r.stderr
            .contains("p.df:3:3: `count` is an aggregate: it is bound in a body, `n = count(x)`"),
        "{}",
        r.stderr
    );
    plan("\nenv(\"prod\")\nq(n) where env(x), n = count(x)\nr(y) where env(x), y = str.upper(x)\nuse fake\n")
        .success();
}

#[test]
fn a_negative_cycle_names_each_rule() {
    let r = plan(
        "\nresource net.vpc x {\n  peer = p\n} where p = y.name\nresource net.subnet y {\n  name = q\n} where q = x.peer\nuse fake\n",
    )
    .failure();
    assert!(r.stderr.contains("not stratifiable"), "{}", r.stderr);
    assert!(r.stderr.contains("(at p.df:3:3)"), "{}", r.stderr);
    assert!(r.stderr.contains("(at p.df:6:3)"), "{}", r.stderr);
}

#[test]
fn a_lowering_error_names_its_statement() {
    let r = plan("\ninstance nope main\nuse fake\n").failure();
    assert!(
        r.stderr
            .contains("p.df:2:1: no component `nope`: there is no"),
        "{}",
        r.stderr
    );
}

/// `why` names each contribution's statement, where it is written, and the
/// module activation or copy it was lowered out of: the same frame for both.
#[test]
fn why_names_the_activation_and_the_copy() {
    let s = Scratch::new("diag-why");
    s.write(
        "tags.df",
        "\n\n\n\n\n\narg(t, a, \"tags\", { team: \"x\" }) where want(t, a)\n",
    );
    s.write(
        "p.df",
        "\ncomponent m {\n  resource net.vpc vpc { cidr = \"10.0.0.0/16\" }\n}\ninstance m main\nuse tags\nuse fake\n",
    );
    let out = s
        .run(&[
            "dev",
            "--world",
            "w.json",
            "why",
            "attr(net.vpc, A, P, V)",
            "--all",
            "p.df",
        ])
        .success()
        .stdout;
    assert!(
        out.contains("└─ \"10.0.0.0/16\"   p.df:3   (instance m main)\n"),
        "{out}"
    );
    assert!(
        out.contains(
            "tags.df:7  arg(t, a, \"tags\", { team: \"x\" }) where want(t, a)   (use tags)\n"
        ),
        "{out}"
    );
}

/// R-4: a content read of a computed path in a block is a note at the
/// read, once per block; a read the block's name needs is the point of the
/// wait, and has none; a whole value is a reference and waits for nothing.
#[test]
fn a_content_read_of_a_computed_path_is_a_note() {
    let s = Scratch::new("diag-computed-read");
    let f = common::repo().join("tests/fixtures/notes/computed_read.df");
    let r = s
        .run(&["dev", "--world", "w.json", "plan", f.to_str().unwrap()])
        .success();
    assert_eq!(r.stderr.matches("note: ").count(), 1, "{}", r.stderr);
    assert!(
        r.stderr.contains(
            "computed_read.df:8:17: reads `db.endpoint` now, a computed value: this block waits \
             for the tick that creates `db`; a field written `= db.endpoint` would be an edge and \
             apply with it"
        ),
        "{}",
        r.stderr
    );
}
