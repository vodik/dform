//! `why` of what is not derived over the rules dform writes beside the
//! program: the policy's lifecycle denies (`zset::POLICY_RULES`), a type's
//! seeded lifecycle, the remote name a provider generated. Each says its
//! words where the program's rule says its statement, and dform's own
//! relations by the names the program knows; a reference prints as its
//! address in every line, a `not` among them; a `not` whose row exists
//! names the row and where it comes from.

mod common;
use common::{Run, Scratch};

/// `dform dev --world w.json [--provider ..] why PATTERN p.df` in `s`.
fn why(s: &Scratch, providers: &[&str], pattern: &str) -> String {
    let mut args = vec!["--world", "w.json"];
    for p in providers {
        args.extend(["--provider", p]);
    }
    s.run(&common::on("p.df", &args, &["why", pattern]))
        .success()
        .stdout
}

/// No line of `out` is the core's: a rule's `:-`, a `__` name, a
/// reference's `ref(..)`.
#[track_caller]
fn surface(out: &str) {
    for word in [":-", "__", "ref(", "deny(M)"] {
        assert!(!out.contains(word), "{word} in:\n{out}");
    }
}

const GUARDED: &str = r#"use fake
resource net.vpc main { cidr = "10.0.0.0/16" }
resource net.vpc data { cidr = "10.1.0.0/16" }
lifecycle(main, "prevent_destroy")
deny "${v} is guarded" where v in net.vpc, not lifecycle(v, "prevent_destroy")
"#;

/// A deny of the policy's that does not hold: its words at `dform`, as
/// the tree says one that holds, and the condition that failed with the
/// reference its message names read back as the reference.
#[test]
fn a_policy_deny_says_its_words() {
    let s = Scratch::new("why-not-policy");
    s.write("p.df", GUARDED);
    let out = why(
        &s,
        &[],
        r#"deny "lifecycle prevent_destroy: the plan would delete net.vpc[\"main\"]""#,
    );
    assert_eq!(
        out,
        "deny \"lifecycle prevent_destroy: the plan would delete net.vpc[\\\"main\\\"]\": does \
         not hold\n  \
         dform  the lifecycle rule prevent_destroy, against a delete\n    \
         deformation(\"delete\", net.vpc main, _): no row\n    \
         nearest: (\"create\", net.vpc main, \"absent\"), (\"create\", net.vpc data, \"absent\")\n"
    );
    let out = why(
        &s,
        &[],
        r#"deny "the world changed under a pending change: net.vpc[\"main\"]""#,
    );
    assert!(
        out.contains(
            "\n  dform  the world rule: a held deformation's resource moved since its plan\n"
        ),
        "{out}"
    );
    surface(&out);
}

/// A `not` over a reference prints it as its address: in the tree of
/// what holds, and in the why-not of what the row it found stops, with
/// that row and where the program writes it.
#[test]
fn a_not_says_a_reference_by_its_address() {
    let s = Scratch::new("why-not-ref");
    s.write("p.df", GUARDED);
    let out = why(&s, &[], r#"deny "net.vpc[\"data\"] is guarded""#);
    assert!(
        out.ends_with("  └─ not lifecycle(net.vpc data, \"prevent_destroy\")   (absent)\n"),
        "{out}"
    );
    let out = why(&s, &[], r#"deny "net.vpc[\"main\"] is guarded""#);
    assert_eq!(
        out,
        "deny \"net.vpc[\\\"main\\\"] is guarded\": does not hold\n  \
         p.df:5  deny \"${v} is guarded\" where v in net.vpc, not lifecycle(v, \"prevent_destroy\")\n    \
         not lifecycle(net.vpc main, \"prevent_destroy\"): the row exists: \
         lifecycle(net.vpc main, \"prevent_destroy\")   p.df:4\n"
    );
}

/// A project whose fake provider's schema says a `net.vpc` is retained,
/// its program `p`.
fn seeded(name: &str, p: &str) -> Scratch {
    let s = Scratch::project(name);
    s.write(
        "providers/fake/schema.df",
        &(std::fs::read_to_string(common::repo().join("crates/dform-mock/schemas/fake.df"))
            .unwrap()
            + "\ntype_lifecycle(\"net.vpc\", \"retain\")\n"),
    );
    s.write("p.df", p);
    s
}

/// `dform dev --world w.json why PATTERN p.df` in a project.
fn why_in(s: &Scratch, pattern: &str) -> String {
    let mut c = common::dform();
    c.args(["dev", "--world", "w.json", "why", pattern, "p.df"])
        .env("NO_COLOR", "1")
        .current_dir(&s.dir);
    Run::from(c.output().unwrap()).success().stdout
}

const SEEDED: &str = r#"use fake
resource net.vpc main { cidr = "10.0.0.0/16" }
resource net.vpc data { cidr = "10.1.0.0/16" }
lifecycle(main, "destroy")
deny "${v} is kept" where v in net.vpc, not lifecycle(v, "retain")
"#;

/// The seeded lifecycle's rules say their words, and the one whose `not`
/// fails says the row the program writes that stops it, not that every
/// condition holds.
#[test]
fn a_seeded_lifecycle_says_its_words_and_the_row_that_stops_it() {
    let s = seeded("why-not-seed", SEEDED);
    let out = why_in(&s, r#"lifecycle(net.vpc["main"], "retain")"#);
    assert_eq!(
        out,
        "lifecycle(net.vpc main, \"retain\"): no rule derives it\n  \
         dform  the program's lifecycle\n    \
         lifecycle(net.vpc main, \"retain\"): no row\n    \
         nearest: (net.vpc main, \"destroy\")\n  \
         dform  the schema of provider fakecloud: each net.vpc is \"retain\" (type_lifecycle), \
         unless the program writes another\n    \
         the program writes no lifecycle for net.vpc main: it writes \
         lifecycle(net.vpc main, \"destroy\")   p.df:4\n"
    );
    // A seeded row a `not` finds: by the words of the rule that seeds it.
    let out = why_in(&s, r#"deny "net.vpc[\"data\"] is kept""#);
    assert!(
        out.ends_with(
            "    not lifecycle(net.vpc data, \"retain\"): the row exists: \
             lifecycle(net.vpc data, \"retain\")   (the schema of provider fakecloud: each \
             net.vpc is \"retain\" (type_lifecycle), unless the program writes another)\n"
        ),
        "{out}"
    );
    surface(&out);
}

const NAMES: &str = r#"
type_provider(app.config, "fakecloud")
type_attr(app.config, "name", "string", ["required", "id"])
type_remote_name(app.config, "name")
"#;

/// The rule that makes a read of a generated name answer dform's says its
/// words, its unbound columns as `_`.
#[test]
fn a_remote_name_says_its_words() {
    let s = Scratch::new("why-not-remote");
    s.write("names.df", NAMES);
    s.write("p.df", "resource app.config cfg { name = \"cfg\" }\n");
    let out = why(
        &s,
        &["fake", "names.df"],
        r#"attr("app.config", "cfg", "name", "other")"#,
    );
    assert_eq!(
        out,
        "app.config cfg.name: no rule derives it\n  \
         dform  the name dform gave the object (remote_name), over the one the program writes\n    \
         remote_name(\"app.config\", \"cfg\", _, _): no row\n    \
         remote_name has no rows\n"
    );
}

/// Not yet: the computed attribute's rules say their words, but the
/// conditions under them name the core's relations (`resolve(net.vpc
/// main.id, _): not derived`, `identity(_, _, _): no row`), which no
/// program writes; reached only by a hand-written `attr` row.
#[test]
#[ignore = "a computed attribute's conditions name the core's relations"]
fn a_computed_attribute_says_its_conditions_in_words() {
    let s = Scratch::new("why-not-computed");
    s.write("p.df", GUARDED);
    let out = why(&s, &[], r#"attr("net.vpc", "main", "id", "x")"#);
    assert!(
        !out.contains("resolve(") && !out.contains("identity("),
        "{out}"
    );
}

/// Not yet: a relation of the program's whose column is a reference is
/// read by `why` with the address as its name (`kept("main")`), so the
/// row it names is never the one the program derives; the pattern's
/// reading is the query parser's (`query::parse`), which does not know
/// the relation's declaration.
#[test]
#[ignore = "a pattern names a program relation's reference column by its name"]
fn a_program_relation_takes_a_reference_by_its_address() {
    let s = Scratch::new("why-not-kept");
    s.write(
        "p.df",
        &format!("{GUARDED}kept(v) where v in net.vpc, not lifecycle(v, \"prevent_destroy\")\n"),
    );
    let out = why(&s, &[], r#"kept(net.vpc["main"])"#);
    assert!(
        out.starts_with("kept(net.vpc main): no rule derives it\n"),
        "{out}"
    );
}
