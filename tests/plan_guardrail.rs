//! R-80: a plan that deletes every resource a rule derived at the last
//! apply, or resources a relation that emptied since takes with it, says
//! so in a `warning` section with the leaf that changed (R-79's because)
//! and the relations that emptied under them, and apply asks for it once,
//! on its own, also under `--yes`, unless `--allow-empty` (or the stack's
//! `allow_empty`) names it. A relation that emptied with no resource is
//! no warning (`-v` lists it). A deny reads the last apply's record as
//! `derived_at_last_apply(rule, n)`.

use crate::common;
use common::Scratch;

/// Subnets joined through a table of active regions: one row of
/// `data/active.csv` is the whole join.
const NET: &str = r#"

input zone from csv.decode(io.read("data/zones.csv"))
input active from csv.decode(io.read("data/active.csv"))

decl zone(name: string, n: int, region: string)
decl active(name: string)

use fake

resource net.vpc main { cidr = "10.0.0.0/16" }

resource net.subnet "private-${z}" {
  vpc = main
  cidr = inet.subnet(main.cidr, 8, n)
  zone = z
} where zone(z, n, r), active(r)
"#;

fn project(name: &str) -> Scratch {
    let s = Scratch::project(name);
    s.write(".gitignore", "dform.state/\n");
    s.write(
        "data/zones.csv",
        "name,n,region\nus-test-1a,1,east\nus-test-1b,2,east\n",
    );
    s.write("data/active.csv", "name\neast\n");
    s.write("stacks/net.df", NET);
    s
}

fn git(s: &Scratch, args: &[&str]) {
    common::git(&s.dir, args);
}

fn subnets(s: &Scratch) -> usize {
    let st: serde_json::Value =
        serde_json::from_str(&s.read("dform.state/stacks.net/state.json")).unwrap();
    st["resources"]
        .as_object()
        .unwrap()
        .keys()
        .filter(|k| k.starts_with("net.subnet::"))
        .count()
}

const WARNING: &str = "warning\n  \
    stacks/net.df:13  resource net.subnet \"private-${z}\" { .. } where zone(z, n, r), active(r)\n      \
    deletes all 2 it derived at the last apply: net.subnet private-us-test-1a, \
    net.subnet private-us-test-1b\n";

/// A broken join: the one row of the table the join reads is gone, so
/// every subnet goes. The plan's warning names the rule, what it deletes,
/// the row that went and the relation it empties, under it; an
/// unattended apply refuses, naming the flag, and changes nothing; with
/// the flag it applies.
#[test]
fn a_broken_join_is_warned_and_asked_for() {
    let s = project("guard-join");
    git(&s, &["init", "-q", "."]);
    git(&s, &["add", "-A"]);
    git(&s, &["commit", "-qm", "one"]);
    s.run(&["apply", "net", "--yes"]).success();
    assert_eq!(subnets(&s), 2);
    s.write("data/active.csv", "name\n");
    git(&s, &["commit", "-qam", "two"]);
    let r = s.run(&["plan", "net"]).success();
    let want = format!(
        "{WARNING}      because data/active.csv no longer has the row active(\"east\")\n      \
         because active  had 1 row at the last apply, has none now\n"
    );
    // The section is after the ticks, and last.
    assert!(r.stdout.ends_with(&want), "{want}\n---\n{}", r.stdout);
    assert!(
        r.stdout.find("\nwarning\n").unwrap() > r.stdout.find("tick 1").unwrap(),
        "{}",
        r.stdout
    );
    // The bare diff says it too.
    let r = s.run(&["plan", "net", "--why=none"]).success();
    assert!(
        r.stdout
            .contains("warning: this plan empties what the last apply derived\n"),
        "{}",
        r.stdout
    );
    // As JSON, a field of its own.
    let r = s.run(&["plan", "net", "--json"]).success();
    let j: serde_json::Value = serde_json::from_str(&r.stdout).unwrap();
    assert_eq!(j["warnings"][0]["rule"], "stacks/net.df:13", "{j}");
    assert_eq!(j["warnings"][0]["emptied"][0]["relation"], "active", "{j}");
    assert_eq!(j["warnings"].as_array().unwrap().len(), 1, "{j}");

    let r = s.run(&["apply", "net", "--yes"]).failure();
    assert!(
        r.stderr.contains(
            "apply net: the plan deletes all 2 resources the rule at stacks/net.df:13 derived \
             at the last apply; nothing to ask on (stdin is not a terminal): confirm it on a \
             terminal, or pass --allow-empty stacks/net.df:13 if it is meant"
        ),
        "{}",
        r.stderr
    );
    assert_eq!(subnets(&s), 2, "the refused apply deleted");
    // The relation is the rule's because, not a question of its own.
    s.run(&["apply", "net", "--yes", "--allow-empty", "stacks/net.df:13"])
        .success();
    assert_eq!(subnets(&s), 0);
}

/// A deliberate delete: the rule is gone from the program. Without git
/// there is no because, and the warning is the same; the flag silences
/// it for apply, the stack's `allow_empty` for plan and apply.
#[test]
fn a_deliberate_delete_is_silenced_by_the_flag() {
    let s = project("guard-delete");
    s.run(&["apply", "net", "--yes"]).success();
    let without = NET.split("resource net.subnet").next().unwrap();
    s.write("stacks/net.df", without);
    let r = s.run(&["plan", "net"]).success();
    assert!(r.stdout.contains(WARNING), "{}", r.stdout);
    assert!(!r.stdout.contains("because"), "{}", r.stdout);
    s.run(&["apply", "net", "--yes"]).failure();
    assert_eq!(subnets(&s), 2);
    // Named in dform.toml: the plan says nothing, apply asks nothing.
    s.write(
        "dform.toml",
        "[project]\nedition = \"2026\"\n\n[stacks.net]\nallow_empty = [\"net.subnet\"]\n",
    );
    let r = s.run(&["plan", "net"]).success();
    assert!(!r.stdout.contains("warning"), "{}", r.stdout);
    s.run(&["apply", "net", "--yes"]).success();
    assert_eq!(subnets(&s), 0);
    // A plan that deletes some of a rule's resources says nothing.
    let t = project("guard-some");
    t.run(&["apply", "net", "--yes"]).success();
    t.write(
        "data/zones.csv",
        "name,n,region\nus-test-1a,1,east\nus-test-1b,2,west\n",
    );
    let r = t.run(&["plan", "net"]).success();
    assert!(r.stdout.contains("(1 delete)"), "{}", r.stdout);
    assert!(!r.stdout.contains("warning"), "{}", r.stdout);
}

/// The ask is on its own, after the plan's: on a terminal, under
/// `--yes`, `n` declines and nothing is deleted.
#[test]
fn the_ask_is_on_a_terminal_also_under_yes() {
    let s = project("guard-pty");
    s.run(&["apply", "net", "--yes"]).success();
    s.write("data/active.csv", "name\n");
    let mut cmd = common::dform();
    cmd.args(["apply", "net", "--yes", "--allow-empty", "active"])
        .current_dir(s.path(""));
    let (said, _) = common::answering(&s.dir, cmd, &["n"]);
    assert!(
        said[0].ends_with(
            "The plan deletes all 2 resources the rule at stacks/net.df:13 derived at the last \
             apply. Apply it anyway? [y/N] "
        ),
        "{}",
        said[0]
    );
    assert!(!said[1].contains("not confirmed"), "{}", said[1]);
    assert_eq!(subnets(&s), 2);
}

/// `derived_at_last_apply(rule, n)` is a relation a deny reads: here, a
/// deny when a rule that derived resources is left with no region.
#[test]
fn a_deny_reads_what_the_last_apply_derived() {
    let s = project("guard-deny");
    s.write(
        "stacks/net.df",
        &format!(
            "{NET}\ndeny(m) where\n  derived_at_last_apply(\"stacks/net.df:13\", n), n > 0,\n  \
             not active(_),\n  m = \"the subnets lost their region\",\n"
        ),
    );
    s.run(&["apply", "net", "--yes"]).success();
    let r = s.run(&["query", "derived_at_last_apply", "net"]).success();
    assert!(r.stdout.contains("stacks/net.df:13"), "{}", r.stdout);
    s.write("data/active.csv", "name\n");
    let r = s.run(&["plan", "net"]).failure();
    assert!(
        r.stdout
            .contains("denied\n  the subnets lost their region    stacks/net.df:19\n"),
        "{}",
        r.stdout
    );
    let r = s.run(&["why", "deny(m)", "net"]).success();
    assert!(
        r.stdout
            .contains("├─ derived_at_last_apply(\"stacks/net.df:13\", 2)   plan\n"),
        "{}",
        r.stdout
    );
}

/// A copy's own relation is named as the source reads it, by its name
/// there and the copy (R-111), never in the core's spelling
/// (`green::vpc_net`); `--allow-empty` takes its path.
#[test]
fn a_copys_relation_is_named_as_the_source_reads_it() {
    let s = Scratch::project("guard-copy");
    let net = |on: &str| {
        format!(
            "
use fake
component box {{
  input cidr: string
  input on: bool = true
  let vpc_net = cidr where on
  resource net.vpc v {{ cidr = vpc_net }}
}}
resource box blue {{ cidr = \"10.1.0.0/16\" }}
resource box green {{ cidr = \"10.0.0.0/16\", on = {on} }}
"
        )
    };
    s.write("stacks/net.df", &net("true"));
    s.run(&["apply", "net", "--yes"]).success();
    s.write("stacks/net.df", &net("false"));
    let r = s.run(&["plan", "net"]).success();
    assert!(
        r.stdout.contains(
            "\nwarning\n  deletes net.vpc green.v\n      \
             because vpc_net (in green)  had 1 row at the last apply, has none now\n"
        ),
        "{}",
        r.stdout
    );
    let warning = &r.stdout[r.stdout.find("\nwarning\n").unwrap()..];
    assert!(!warning.contains("::"), "{}", r.stdout);
    let r = s.run(&["apply", "net", "--yes"]).failure();
    assert!(
        r.stderr
            .contains("pass --allow-empty green.vpc_net if it is meant"),
        "{}",
        r.stderr
    );
    s.run(&["apply", "net", "--yes", "--allow-empty", "green.vpc_net"])
        .success();
}

/// What still derives is not a relation that emptied: a `let` renamed or
/// moved (its old name no relation of the program now), and one whose
/// value waits on what the plan does not know yet (a new database's
/// endpoint), had rows at the last apply and are not named.
#[test]
fn a_let_renamed_or_waiting_is_no_relation_that_emptied() {
    let s = Scratch::project("guard-let");
    let net = "
use fake
let tag = \"x\"
resource db.postgres d1 { }
let ep = \"postgres://${d1.endpoint}\"
resource net.vpc main { cidr = \"10.0.0.0/16\" }
";
    s.write("stacks/net.df", net);
    s.run(&["apply", "net", "--yes"]).success();
    s.write(
        "stacks/net.df",
        &net.replace("d1", "d2").replace("let tag", "let label"),
    );
    let r = s.run(&["plan", "net", "-v"]).success();
    assert!(r.stdout.contains("+ db.postgres d2"), "{}", r.stdout);
    assert!(!r.stdout.contains("warning"), "{}", r.stdout);
}

/// Resources stated once that read a `let` over a table, whose one row
/// goes: no rule's resources, so the headline is the resources, the
/// `let` and the table under it as a chain; apply asks once, naming both.
#[test]
fn resources_a_relation_takes_are_the_headline_and_asked_for_once() {
    let s = project("guard-taken");
    s.write(
        "stacks/net.df",
        r#"
input active from csv.decode(io.read("data/active.csv"))
decl active(name: string)
use fake
let east = true where active("east")
resource net.vpc a { cidr = "10.0.0.0/16" } where east
resource net.vpc b { cidr = "10.1.0.0/16" } where east
"#,
    );
    s.run(&["apply", "net", "--yes"]).success();
    s.write("data/active.csv", "name\n");
    let r = s.run(&["plan", "net"]).success();
    let want = "\nwarning\n  deletes net.vpc a, net.vpc b\n      \
        because east  had 1 row at the last apply, has none now\n        \
        because active  had 1 row at the last apply, has none now\n";
    assert!(r.stdout.ends_with(want), "{want}\n---\n{}", r.stdout);
    let r = s.run(&["apply", "net", "--yes"]).failure();
    assert!(
        r.stderr.contains(
            "apply net: the plan deletes net.vpc a and net.vpc b because the relation east, \
             which had 1 row at the last apply, has none now; nothing to ask on"
        ),
        "{}",
        r.stderr
    );
    let mut cmd = common::dform();
    cmd.args(["apply", "net", "--yes"]).current_dir(s.path(""));
    let (said, _) = common::answering(&s.dir, cmd, &["n"]);
    assert!(
        said[0].ends_with(
            "The plan deletes net.vpc a and net.vpc b because the relation east, which had 1 \
             row at the last apply, has none now. Apply it anyway? [y/N] "
        ),
        "{said:?}"
    );
    assert!(
        !said[1].contains("Apply it anyway"),
        "asked twice: {said:?}"
    );
    s.run(&["apply", "net", "--yes", "--allow-empty", "active"])
        .success();
}

/// A table whose rows go and a `let` over it that empties, read by no
/// resource, in a plan that makes a resource: the plan warns of nothing
/// and apply asks nothing; `-v` lists them.
#[test]
fn a_relation_no_resource_reads_is_no_warning() {
    let s = project("guard-unread");
    let net = r#"
input active from csv.decode(io.read("data/active.csv"))
decl active(name: string)
use fake
let east = true where active("east")
resource net.vpc main { cidr = "10.0.0.0/16" }
"#;
    s.write("stacks/net.df", net);
    s.run(&["apply", "net", "--yes"]).success();
    s.write("data/active.csv", "name\n");
    s.write(
        "stacks/net.df",
        &format!("{net}resource net.vpc spare {{ cidr = \"10.1.0.0/16\" }}\n"),
    );
    let r = s.run(&["plan", "net"]).success();
    assert!(!r.stdout.contains("warning"), "{}", r.stdout);
    let r = s.run(&["plan", "net", "-v"]).success();
    assert!(
        r.stdout.ends_with(
            "\nwarning\n  active  had 1 row at the last apply, has none now\n  \
             east  had 1 row at the last apply, has none now\n"
        ),
        "{}",
        r.stdout
    );
    let r = s.run(&["plan", "net", "--json"]).success();
    let j: serde_json::Value = serde_json::from_str(&r.stdout).unwrap();
    assert!(j.get("warnings").is_none(), "{j}");
    assert_eq!(j["emptied_relations"][1]["relation"], "east", "{j}");
    s.run(&["apply", "net", "--yes"]).success();
}
