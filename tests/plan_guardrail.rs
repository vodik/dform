//! R-80: a plan that deletes every resource a rule derived at the last
//! apply, or empties a relation that had rows then, says so in a
//! `warning` section with the leaf that changed (R-79's because), and
//! apply asks for it on its own, also under `--yes`, unless
//! `--allow-empty` (or the stack's `allow_empty`) names it. A deny reads
//! the last apply's record as `derived_at_last_apply(rule, n)`.

mod common;
use common::Scratch;
use expectrl::{Eof, Expect, Session};

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
        serde_json::from_str(&s.read("dform.state/net/state.json")).unwrap();
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
/// every subnet goes. The plan's warning names the rule, what it deletes
/// and the row that went, and the relation it empties; an unattended
/// apply refuses, naming the flag, and changes nothing; with the flag it
/// applies.
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
        "{WARNING}      because data/active.csv no longer has the row active(\"east\")\n  \
         active  had 1 row at the last apply, has none now\n"
    );
    assert!(r.stdout.contains(&want), "{want}\n---\n{}", r.stdout);
    // The section is after the ticks, and last.
    assert!(
        r.stdout.find("\nwarning\n").unwrap() > r.stdout.find("tick 1").unwrap()
            && r.stdout
                .ends_with("active  had 1 row at the last apply, has none now\n"),
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
    assert_eq!(j["warnings"][1]["relation"], "active", "{j}");

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
    // Naming the rule leaves the relation, which is asked for too.
    let r = s
        .run(&["apply", "net", "--yes", "--allow-empty", "stacks/net.df:13"])
        .failure();
    assert!(
        r.stderr
            .contains("the plan empties the relation active, which had 1 row at the last apply"),
        "{}",
        r.stderr
    );
    s.run(&[
        "apply",
        "net",
        "--yes",
        "--allow-empty",
        "net.subnet",
        "--allow-empty",
        "active",
    ])
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
        .env("NO_COLOR", "1")
        .current_dir(s.path(""));
    let mut p = Session::spawn(cmd).unwrap();
    p.set_expect_timeout(Some(std::time::Duration::from_secs(60)));
    let text = |b: &[u8]| String::from_utf8_lossy(b).replace('\r', "");
    let all = |c: &expectrl::Captures| c.matches().fold(text(c.before()), |t, m| t + &text(m));
    let before = text(p.expect("[y/N] ").unwrap().before());
    assert!(
        before.ends_with(
            "The plan deletes all 2 resources the rule at stacks/net.df:13 derived at the last \
             apply. Apply it anyway? "
        ),
        "{before}"
    );
    p.send_line("n").unwrap();
    let after = all(&p.expect(Eof).unwrap());
    assert!(!after.contains("not confirmed"), "{after}");
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
            "{NET}\ndeny(m) where {{\n  derived_at_last_apply(\"stacks/net.df:13\", n), n > 0\n  \
             not active(_)\n  m = \"the subnets lost their region\"\n}}\n"
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
    let net = "
use fake
component box {
  input vpc_net: string
  resource net.vpc v { cidr = vpc_net }
}
resource box blue { vpc_net = \"10.1.0.0/16\" }
";
    s.write(
        "stacks/net.df",
        &format!("{net}resource box green {{ vpc_net = \"10.0.0.0/16\" }}\n"),
    );
    s.run(&["apply", "net", "--yes"]).success();
    s.write("stacks/net.df", net);
    let r = s.run(&["plan", "net"]).success();
    assert!(
        r.stdout
            .contains("\n  vpc_net (in green)  had 1 row at the last apply, has none now\n"),
        "{}",
        r.stdout
    );
    assert!(!r.stdout.contains("::"), "{}", r.stdout);
    let r = s.run(&["apply", "net", "--yes"]).failure();
    assert!(
        r.stderr.contains(
            "the plan empties the relation vpc_net (in green), which had 1 row at the last \
             apply; nothing to ask on (stdin is not a terminal): confirm it on a terminal, or \
             pass --allow-empty green.vpc_net if it is meant"
        ),
        "{}",
        r.stderr
    );
    s.run(&["apply", "net", "--yes", "--allow-empty", "green.vpc_net"])
        .success();
}
