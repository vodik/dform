//! A type's lifecycle, seeded by its provider's schema: `type_lifecycle(T,
//! "retain")` gives each resource of `T` the row `lifecycle(r, "retain")`,
//! so a removal from the program forgets the object, as a program's
//! `retain` does (R-154); a program's row with another removal word
//! (`destroy`, `prevent_destroy`) replaces it, as a `set` wins over a
//! schema default. The row is read in a body as any other, and `why`
//! says the provider's schema answered it. Here the fake's `net.vpc`,
//! its schema with the fact added.

mod common;
use common::{Run, Scratch};

const BOTH: &str = r#"
use fake
resource net.vpc main { cidr = "10.0.0.0/16" }
resource net.vpc data { cidr = "10.1.0.0/16" }
"#;

const MAIN: &str = r#"
use fake
resource net.vpc main { cidr = "10.0.0.0/16" }
"#;

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

/// `dform dev --world w.json ARGS p.df`.
fn dev(s: &Scratch, args: &[&str]) -> Run {
    let mut all = vec!["dev", "--world", "w.json"];
    all.extend_from_slice(args);
    all.push("p.df");
    let mut c = common::dform();
    c.args(&all).env("NO_COLOR", "1").current_dir(&s.dir);
    Run::from(c.output().unwrap())
}

/// Both networks made, then the program `p`.
fn applied(name: &str, p: &str) -> Scratch {
    let s = seeded(name, BOTH);
    dev(&s, &["apply", "--yes"]).success();
    s.write("p.df", p);
    s
}

fn keys(v: &serde_json::Value) -> Vec<String> {
    v["resources"]
        .as_object()
        .unwrap()
        .keys()
        .cloned()
        .collect()
}

const FORGET: &str = "  ~ net.vpc data  forgotten, kept in the world  (lifecycle retain)\n";

#[test]
fn a_removal_forgets_an_object_its_type_retains() {
    let s = applied("seed-forget", MAIN);
    let p = dev(&s, &["plan"]).success();
    assert_eq!(
        p.summary(),
        "plan: 1 change (1 forget) over 1 tick",
        "{}",
        p.stdout
    );
    assert!(p.stdout.contains(FORGET), "{}", p.stdout);
    dev(&s, &["apply", "--yes"]).success();
    assert_eq!(keys(&s.json("w.json")), ["net.vpc::data", "net.vpc::main"]);
    assert_eq!(keys(&s.json("w.state.json")), ["net.vpc::main"]);
}

/// The program's `destroy`, by the address it no longer makes, is the
/// delete its type's `retain` would have made a forget.
#[test]
fn the_programs_destroy_replaces_the_types_retain() {
    let s = applied(
        "seed-destroy",
        &format!("{MAIN}lifecycle(net.vpc[\"data\"], \"destroy\")\n"),
    );
    let p = dev(&s, &["plan"]).success();
    assert_eq!(
        p.summary(),
        "plan: 1 change (1 delete) over 1 tick",
        "{}",
        p.stdout
    );
    assert!(
        p.stdout.contains("  - net.vpc data  not in the program\n"),
        "{}",
        p.stdout
    );
}

/// A body reads the seeded rows as the program's own: each resource has
/// one removal word, the program's where it writes one.
#[test]
fn a_body_reads_the_seeded_row_and_the_programs_wins() {
    let s = seeded(
        "seed-read",
        &format!(
            "{BOTH}lifecycle(main, \"destroy\")\n\
             deny \"${{v}} is not retained\" where v in net.vpc, not lifecycle(v, \"retain\")\n"
        ),
    );
    let q = dev(&s, &["query", "lifecycle"]).success();
    assert_eq!(
        q.stdout,
        "a             b\nnet.vpc data  \"retain\"\nnet.vpc main  \"destroy\"\n"
    );
    let r = dev(&s, &["plan"]).failure();
    assert!(
        r.stderr.contains("- net.vpc[\"main\"] is not retained"),
        "{}",
        r.stderr
    );
    assert!(
        !r.stderr.contains("net.vpc[\"data\"] is not"),
        "{}",
        r.stderr
    );
}

/// `why` says who answered the row: the provider's schema, by name, and
/// the schema fact it read.
#[test]
fn why_names_the_providers_schema() {
    let s = seeded("seed-why", BOTH);
    let w = dev(&s, &["why", "lifecycle(data, \"retain\")"]).success();
    for line in [
        "lifecycle(net.vpc data, \"retain\")\n",
        "  dform  the schema of provider fakecloud: each net.vpc is \"retain\" (type_lifecycle), \
         unless the program writes another\n",
        "  ├─ type_lifecycle(\"net.vpc\", \"retain\")   provider schema\n",
        "  ├─ net.vpc data   p.df:4\n",
    ] {
        assert!(w.stdout.contains(line), "{line}\n{}", w.stdout);
    }
}

/// A destroy forgets what its type retains, as it does what the program
/// retains.
#[test]
fn a_destroy_forgets_what_the_type_retains() {
    let s = applied("seed-destroy-all", BOTH);
    let p = dev(&s, &["plan", "--destroy"]).success();
    assert_eq!(
        p.summary(),
        "plan: 2 changes (2 forget) over 1 tick",
        "{}",
        p.stdout
    );
}

/// A copy's `destroy` reaches each of its resources in the plan, but the
/// seeded row is still read for them in a body: the seed reads the
/// resource's own rows, not its copy's.
#[test]
#[ignore = "a copy's removal word does not yet suppress the seeded row of its resources in the \
            evaluation (the plan honours it, `zset::Lifecycle::from_facts`): the seed would read \
            the copy's rows through `instance_of`"]
fn a_copys_destroy_replaces_the_seeded_row_of_its_resources() {
    let s = seeded(
        "seed-copy",
        r#"
use fake
component pair {
  resource net.vpc v { cidr = "10.0.0.0/16" }
}
resource pair p {}
lifecycle(p, "destroy")
deny "${v} is retained" where v in net.vpc, lifecycle(v, "retain")
"#,
    );
    dev(&s, &["plan"]).success();
}
