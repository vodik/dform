//! One diagnostic shape for every error (the plan's grammar, `why
//! --tree`'s glyphs): the kind word and the sentence on one line, its
//! code dim at the end; each site `file:line` with its source line, a
//! caret under a span narrower than the line; notes dim, the help last.
//! A syntax error, a value given outside its check, a conflict, a deny
//! that refuses, a provider's refusal and an error with no site, each
//! rendered in-process: the text is the promise.

use dform::ast::{RuleStmt, Stmt};
use dform::diag::{self, Diagnostic, Kind};
use dform::query::Redactor;
use dform::report::{self, Failure};
use dform::schema::Schema;
use std::collections::BTreeSet;

fn redactor() -> Redactor {
    Redactor::new(&BTreeSet::new(), &Schema::default())
}

/// The rules a source writes, parsed.
fn rules(name: &str, src: &str) -> Vec<RuleStmt> {
    let program = dform::parser::parse_file(name, src).unwrap();
    program
        .statements
        .into_iter()
        .filter_map(|s| match s {
            Stmt::Rule(r) => Some(r),
            _ => None,
        })
        .collect()
}

#[test]
fn a_block_never_closed_is_said_over_its_lines() {
    let src = "use fake\n\nresource net.vpc main {\n  cidr = \"10.0.0.0/16\"\n  tags = {}\n  \
               x = 1\n  y = 2\n";
    let err = dform::parser::parse_file("shapes/open.df", src).unwrap_err();
    assert_eq!(
        diag::report(&err, false),
        "error  the block opened on line 3 is never closed\n  \
         shapes/open.df:3-7  resource net.vpc main {\n                        \
         cidr = \"10.0.0.0/16\"\n                      \
         ...\n                            \
         ^ expected `}` by here\n  \
         help: close it: a `}` after its last line\n"
    );
}

#[test]
fn a_value_outside_its_check_is_labelled_at_the_check_and_where_it_was_given() {
    let src = "input agents: int = 0 check 0 <= agents, agents <= 3\n";
    let file = diag::add_source("shapes/app.df", src);
    let span = |start: usize, end: usize| dform::ast::Span {
        file,
        start: start as u32,
        end: end as u32,
        origin: 0,
    };
    let check = src.find("check").unwrap();
    let d = Diagnostic::error(
        span(check, src.len() - 1),
        "--set agents=4 is outside the check on agents",
    )
    .labelled("checked here")
    .with_given("--set agents=4", "given here")
    .with_help("give agents a value of int check 0 <= agents, agents <= 3");
    assert_eq!(
        d.render(false),
        "error  --set agents=4 is outside the check on agents\n  \
         ├─ shapes/app.df:1  input agents: int = 0 check 0 <= agents, agents <= 3\n  \
         │                                         ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^ checked here\n  \
         └─ --set agents=4   given here\n  \
         help: give agents a value of int check 0 <= agents, agents <= 3\n"
    );
}

#[test]
fn a_conflict_is_said_at_both_writes() {
    diag::add_source(
        "shapes/conflict.df",
        "use fake\nresource net.vpc a { cidr = \"10.0.0.0/16\" }\nresource net.vpc a {\n  \
         cidr = \"10.1.0.0/16\"\n}\n",
    );
    let witness = |v: &str, at: &str| {
        format!(
            r#"{{"rank":"normal","value":"{v}","from":["arg(..) (at shapes/conflict.df:{at})"]}}"#
        )
    };
    let v = format!(
        r#"{} ctx={{"type":"net.vpc","addr":"a","path":"cidr","reason":"two contributions disagree","witnesses":[{},{}]}}"#,
        report::CONFLICT,
        witness("10.0.0.0/16", "2:22"),
        witness("10.1.0.0/16", "4:3"),
    );
    let said = report::refusals(&[v], &[], &redactor());
    assert_eq!(said.len(), 1);
    assert_eq!(
        said[0].render(false),
        "conflict  net.vpc a.cidr: two contributions disagree\n  \
         ├─ shapes/conflict.df:2  resource net.vpc a { cidr = \"10.0.0.0/16\" }\n  \
         │                                                    ^^^^^^^^^^^^^\n  \
         └─ shapes/conflict.df:4  cidr = \"10.1.0.0/16\"\n                                  \
         ^^^^^^^^^^^^^\n  \
         help: rank one of them, `@default` or `@override`\n"
    );
}

#[test]
fn a_deny_that_refuses_is_its_line_with_what_it_fired_for_under_it() {
    let src = "use fake\ndeny \"no big networks\" { vpc: v } where v in net.vpc, v.cidr == \"x\"\n\
               deny \"never\" where 1 == 1\n";
    let rules = rules("shapes/deny.df", src);
    let vs = [
        r#"no big networks ctx={"vpc":"c"}"#.to_string(),
        r#"no big networks ctx={"vpc":"d"}"#.to_string(),
        "never".to_string(),
    ];
    let said: Vec<String> = report::refusals(&vs, &rules, &redactor())
        .iter()
        .map(|d| d.render(false))
        .collect();
    assert_eq!(
        said,
        [
            "refused  no big networks  shapes/deny.df:2\n  \
             ├─ vpc = \"c\"\n  \
             └─ vpc = \"d\"\n",
            "refused  never  shapes/deny.df:3\n",
        ]
    );
}

#[test]
fn a_providers_refusal_is_at_the_resources_block() {
    diag::add_source(
        "shapes/vm.df",
        "use fake\nresource compute.vm app {\n  size = \"huge\"\n}\n",
    );
    let addr = dform::address::Address {
        typ: "compute.vm".into(),
        name: "app".into(),
    };
    let f = Failure::of(
        "plan",
        &addr,
        "refused",
        "size \"huge\" is not offered\nsizes: small, large",
    )
    .at(Some("shapes/vm.df:2".into()));
    assert_eq!(
        diag::report(&anyhow::Error::new(f), false),
        "refused  plan compute.vm app: size \"huge\" is not offered\n  \
         shapes/vm.df:2  resource compute.vm app {\n  \
         sizes: small, large\n"
    );
}

/// An error with no site is the first line alone, by the same printer;
/// its code goes to the end of the line.
#[test]
fn an_error_with_no_site_is_one_line() {
    let err = anyhow::anyhow!("no stack named web");
    assert_eq!(diag::report(&err, false), "error  no stack named web\n");
    let d = Diagnostic::bare(Kind::Error, "E0304: a secret reaches an output");
    assert_eq!(
        d.render(false),
        "error  a secret reaches an output  [E0304]\n"
    );
}

/// In colour, the kind word is the plan's error colour, a site and the
/// code dim, the caret the accent.
#[test]
fn the_palette_is_the_plans() {
    let file = diag::add_source("shapes/paint.df", "p(x) where q(]\n");
    let d = Diagnostic::error(
        dform::ast::Span {
            file,
            start: 13,
            end: 14,
            origin: 0,
        },
        "E0001: expected a term",
    );
    let s = report::Style { color: true };
    assert_eq!(
        d.render(true),
        format!(
            "{}  expected a term  {}\n  {}  p(x) where q(]\n{}{}\n",
            s.paint(report::Paint::Error, "error"),
            s.paint(report::Paint::Dim, "[E0001]"),
            s.paint(report::Paint::Dim, "shapes/paint.df:1"),
            " ".repeat(2 + 17 + 2 + 13),
            s.paint(report::Paint::Because, "^"),
        )
    );
}
