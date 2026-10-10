//! Every error in one shape (R-109): what happened, with the address as
//! the plan prints it (`report::address`), never the stored form
//! (`T["A"]`, `\"` inside it); the provider's or the rule's message on its
//! own line; the site. A provider's own naming of the change (the mock's,
//! the Kubernetes and OVH providers' `apply T["A"]: ..`) is dropped and
//! the address said as the plan says it.
//!
//! The check runs every error-producing fixture of tests/syntax/err and
//! the provider failures below, and fails on any stored address form in
//! what dform prints: a type followed by `["`, or an escaped quote `\"`.
//! Code the message quotes for the reader to write (between backticks,
//! `net.vpc["main"].cidr` in a help) is the source's syntax, not an
//! address dform printed, and is not checked; nor is a source line a
//! diagnostic quotes beside its site. The mock's own chaos log on its stderr says the
//! address as the plan does too.

mod common;
use common::{Run, Scratch, repo};

/// The stored address forms in `text`, outside backtick-quoted code: each
/// offending line.
fn stored_forms(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    for line in text.lines() {
        // A source line a diagnostic quotes beside its site (`├─
        // p.df:16  p(c) where ..`) is the program's text.
        let quoted = line
            .trim_start_matches([' ', '├', '└', '│', '─'])
            .split_once("  ")
            .and_then(|(site, _)| site.rsplit_once(':'))
            .is_some_and(|(_, n)| n.chars().all(|c| c.is_ascii_digit() || c == '-'));
        if quoted {
            continue;
        }
        // Drop what is between backticks.
        let mut outside = String::new();
        for (i, part) in line.split('`').enumerate() {
            if i % 2 == 0 {
                outside.push_str(part);
                outside.push(' ');
            }
        }
        let bytes = outside.as_bytes();
        let typed_bracket = bytes.windows(3).any(|w| {
            (w[0].is_ascii_alphanumeric() || w[0] == b'_') && w[1] == b'[' && w[2] == b'"'
        });
        if typed_bracket || outside.contains("\\\"") {
            out.push(line.to_string());
        }
    }
    out
}

fn check(what: &str, r: &Run, found: &mut Vec<String>) {
    for l in stored_forms(&r.stdout)
        .into_iter()
        .chain(stored_forms(&r.stderr))
    {
        found.push(format!("{what}: {l}"));
    }
}

#[test]
fn the_check_finds_a_stored_form() {
    assert_eq!(
        stored_forms("error  apply ovh.domain_record[\"k3s.\\\"x.y\\\"\"]: no"),
        ["error  apply ovh.domain_record[\"k3s.\\\"x.y\\\"\"]: no"]
    );
    assert!(stored_forms("  help: write `v = net.vpc[\"main\"].cidr` (H-15)").is_empty());
    assert!(stored_forms("\"[\" is not a valid pattern").is_empty());
}

/// Each `tests/syntax/err` fixture, as `dform plan` prints its error.
#[test]
fn no_syntax_error_prints_a_stored_address() {
    let dir = repo().join("tests/syntax/err");
    let mut found = Vec::new();
    let mut ran = 0;
    let mut entries: Vec<_> = std::fs::read_dir(&dir)
        .unwrap()
        .flatten()
        .map(|e| e.path())
        .collect();
    entries.sort();
    for p in entries {
        let s = Scratch::new("error-shape-syntax");
        let out = if p.is_dir() {
            common::dform()
                .args(["-C", p.to_str().unwrap(), "plan", "main"])
                .current_dir(&s.dir)
                .output()
        } else if p.extension().is_some_and(|e| e == "df") {
            common::dform()
                .args(["plan", p.to_str().unwrap()])
                .current_dir(&s.dir)
                .output()
        } else {
            continue;
        };
        let r = Run::from(out.unwrap());
        ran += 1;
        check(&common::rel(&p), &r, &mut found);
    }
    assert!(ran > 50, "{ran}");
    assert!(found.is_empty(), "{}", found.join("\n"));
}

/// A resource whose name holds a dot, in a copy: stored as
/// `net.vpc["k3s.\"k8s-lab.vodik.xyz\""]`, printed `net.vpc
/// k3s."k8s-lab.vodik.xyz"`.
fn project(name: &str) -> Scratch {
    let s = Scratch::new(name);
    s.write("p.df", "use fake\nuse k3s\n");
    s.write(
        "k3s.df",
        "resource net.vpc \"k8s-lab.vodik.xyz\" { cidr = \"10.0.0.0/16\" }\n\
         resource net.vpc other { cidr = \"10.1.0.0/16\" }\n",
    );
    s
}

const SPEC: &str = "net.vpc[\"k3s.\\\"k8s-lab.vodik.xyz\\\"\"]";

fn apply(s: &Scratch, chaos: &str) -> Run {
    let out = common::dform()
        .args(common::on(
            "p.df",
            &["--world", "w.json", "--chaos", chaos],
            &["apply", "--yes"],
        ))
        .env("NO_COLOR", "1")
        .current_dir(&s.dir)
        .output()
        .unwrap();
    Run::from(out)
}

/// A refused Apply: below the block, once, in three lines; the block's
/// `!` line its mark and time alone; the run ends naming what failed.
#[test]
fn a_refused_apply_is_said_once_in_three_lines() {
    let s = project("error-shape-refused");
    let r = apply(&s, &format!("fail={SPEC}")).failure();
    let at = "net.vpc k3s.\"k8s-lab.vodik.xyz\"";
    let lines: Vec<&str> = r.stderr.lines().collect();
    let below = lines
        .iter()
        .position(|l| l.starts_with(&format!("! apply {at}: refused, nothing changed")))
        .unwrap_or_else(|| panic!("{}", r.stderr));
    assert!(
        lines[below + 1].starts_with("    injected failure") && lines[below + 2] == "    k3s.df:1",
        "{}",
        r.stderr
    );
    // The inline line keeps its mark and time (nested under the module
    // it is in, as the plan nests it).
    let inline = lines
        .iter()
        .find(|l| l.trim_start().starts_with(&format!("! {at}")))
        .unwrap_or_else(|| panic!("{}", r.stderr));
    assert!(!inline.contains("injected"), "{}", r.stderr);
    // The message once.
    assert_eq!(
        r.stderr.matches("injected failure").count(),
        1,
        "{}",
        r.stderr
    );
    assert!(
        r.stderr
            .ends_with(&format!("error  apply p: tick 1 failed: {at}\n")),
        "{}",
        r.stderr
    );
    let mut found = Vec::new();
    check("fail", &r, &mut found);
    assert!(found.is_empty(), "{}", found.join("\n"));
}

/// A call with no answer and a provider that died: the same shape, saying
/// the change may have taken effect.
#[test]
fn every_provider_failure_prints_the_plans_address() {
    let mut found = Vec::new();
    for (kind, happened) in [
        ("timeout", "no answer; the change may have taken effect"),
        (
            "crash",
            "the provider died; the change may have taken effect",
        ),
    ] {
        let s = project(&format!("error-shape-{kind}"));
        let r = apply(&s, &format!("{kind}={SPEC}"));
        assert!(
            r.stderr.contains(&format!(
                "! apply net.vpc k3s.\"k8s-lab.vodik.xyz\": {happened}\n"
            )) || r.ok,
            "{kind}: {}",
            r.stderr
        );
        check(kind, &r, &mut found);
    }
    assert!(found.is_empty(), "{}", found.join("\n"));
}

/// An error outside the apply loop (a state that does not parse, a plan
/// file that is not there or not JSON) is in the same shape: what
/// happened, then what caused it indented under it, never anyhow's
/// `Caused by:` list (After R-156).
#[test]
fn an_error_outside_apply_has_no_caused_by_list() {
    let s = Scratch::new("error-shape-chain");
    s.write(
        "p.df",
        "use fake\nresource net.vpc main { cidr = \"10.0.0.0/16\" }\n",
    );
    s.write("w.state.json", "nope");
    s.write("bad.json", "{");
    let run = |args: &[&str]| {
        Run::from(
            common::dform()
                .args(args)
                .current_dir(&s.dir)
                .output()
                .unwrap(),
        )
        .failure()
    };
    for (args, said) in [
        (
            &["dev", "--world", "w.json", "plan", "p.df"][..],
            "error  parse state\n    expected ident at line 1 column 2\n",
        ),
        (
            &["apply", "missing.json"][..],
            "error  read plan file missing.json\n    No such file or directory (os error 2)\n",
        ),
        (
            &["apply", "bad.json"][..],
            "error  parse plan file bad.json\n    EOF while parsing an object at line 1 column 1\n",
        ),
    ] {
        let r = run(args);
        assert_eq!(r.stderr, said, "{args:?}");
    }
}
