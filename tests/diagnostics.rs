//! Every diagnostic names where: syntax errors, lowering errors, undefined
//! predicates, negative cycles, and `why`'s owners (ticket "Spans through
//! lowering to every diagnostic").

mod common;
use common::Scratch;

fn plan(src: &str) -> common::Run {
    let s = Scratch::new("diag");
    s.write("p.df", src);
    s.run(&["--file", "p.df", "--world", "w.json", "plan"])
}

/// A syntax error prints through ariadne: the location, the source line
/// and what was expected; every error in the file, then a count.
#[test]
fn syntax_errors_print_with_their_source_line() {
    let r = plan("edition 2026.\np(a) :- q(.\nr(b) :- .\n").failure();
    assert!(
        r.stderr.contains("p.df:2:11: expected a term"),
        "{}",
        r.stderr
    );
    assert!(
        r.stderr.contains("p.df:3:9: expected a term"),
        "{}",
        r.stderr
    );
    assert!(r.stderr.contains("p(a) :- q(."), "{}", r.stderr);
    assert!(r.stderr.ends_with("2 errors\n"), "{}", r.stderr);
}

#[test]
fn an_undefined_predicate_names_its_literal() {
    let r = plan("edition 2026.\nenv(prod).\nq(X) :- envv(X).\n").failure();
    assert!(
        r.stderr.contains("p.df:3:9: undefined predicate envv/1"),
        "{}",
        r.stderr
    );
    assert!(
        r.stderr.contains("in rule: q(X) :- envv(X)"),
        "{}",
        r.stderr
    );
}

#[test]
fn a_negative_cycle_names_each_rule() {
    let r = plan(
        "edition 2026.\n\
         resource net.a x { peer = P } :- arg(net.b, y, .name, P).\n\
         resource net.b y { name = Q } :- arg(net.a, x, .peer, Q).\n",
    )
    .failure();
    assert!(r.stderr.contains("not stratifiable"), "{}", r.stderr);
    assert!(r.stderr.contains("(at p.df:2:20)"), "{}", r.stderr);
    assert!(r.stderr.contains("(at p.df:3:20)"), "{}", r.stderr);
}

#[test]
fn a_lowering_error_names_its_statement() {
    let r = plan("edition 2026.\ninstance nope main {}.\n").failure();
    assert!(
        r.stderr
            .contains("p.df:2:1: instance nope main names an unknown module 'nope'"),
        "{}",
        r.stderr
    );
}

/// `why` names each contribution's owner by rule id, where it is written,
/// and the policy pack or module instance it was lowered out of.
#[test]
fn why_names_the_pack_and_the_module_instance() {
    let s = Scratch::new("diag-why");
    s.write(
        "p.df",
        "edition 2026.\n\
         module m {\n  resource net.vpc vpc { cidr = \"10.0.0.0/16\" }.\n}.\n\
         instance m main {}.\n\
         policy tags {\n  contributes arg to _ at .tags.\n  arg(T, A, .tags, { team: \"x\" }) :- want(T, A).\n}.\n\
         apply tags.\n",
    );
    let out = s
        .run(&[
            "--file",
            "p.df",
            "--world",
            "w.json",
            "why",
            "attr(net.vpc, A, P, V)",
            "--all",
        ])
        .success()
        .stdout;
    assert!(
        out.contains("owner p.df:3:26 (arg, module m instance main)"),
        "{out}"
    );
    assert!(out.contains("(p.df:8:3, policy tags)]"), "{out}");
}
