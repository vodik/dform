//! The policy block (R-200): each policy one line, its mark the worst of
//! what it ranges over (`holds`, `fails`, `undetermined`), its text
//! unquoted and its site; under it what does not hold with why, the rest
//! as `N hold`; holding policies collapse into the block's count, `-v`
//! lists everything; the headline carries the count.

mod common;
use common::Scratch;

const STACK: &str = r#"
use fake
resource net.vpc a { cidr = "10.0.0.0/16" }
resource net.vpc b { cidr = "10.1.0.0/16" }
resource net.vpc c { cidr = "10.2.0.0/8" }
resource db.postgres db { name = "db" }
deny "a network is no bigger than a /16" { vpc: v } where v in net.vpc, v.cidr == "10.2.0.0/8"
deny "networks are named" where v in net.vpc, v.name == "unnamed"
deny "the database has no endpoint yet" where d in db.postgres, d.endpoint == "nowhere"
"#;

fn stack(name: &str) -> Scratch {
    let s = Scratch::project(name);
    s.write("p.df", STACK);
    s
}

/// The block's lines, from its header to its end.
fn block(stdout: &str) -> String {
    let lines: Vec<&str> = stdout
        .lines()
        .skip_while(|l| !l.starts_with("policy  "))
        .take_while(|l| !l.is_empty())
        .collect();
    lines.join("\n")
}

#[test]
fn each_policy_is_a_line_with_its_tally_and_what_does_not_hold_under_it() {
    let s = stack("policy-block");
    let r = s.run(&["plan", "p.df"]);
    assert_eq!(r.code, Some(4), "{}\n{}", r.stdout, r.stderr);
    assert_eq!(
        r.summary(),
        "plan: 4 changes (4 create) over 1 tick; policy: 1 hold · 1 fails · 1 undetermined",
        "{}",
        r.stdout
    );
    // One failure of three makes a failing policy; the holding ones are
    // one line; an undetermined one says the cell and the tick; the
    // holding policy is the count.
    assert_eq!(
        block(&r.stdout),
        "policy  1 hold · 1 fails · 1 undetermined\n  \
         fails         a network is no bigger than a /16  p.df:7  2 hold · 1 fails\n    \
         net.vpc c                                      vpc = \"c\"\n    \
         2 hold\n  \
         undetermined  the database has no endpoint yet   p.df:9  1 undetermined\n    \
         db.postgres db                                 until endpoint is known (tick 2)",
        "{}",
        r.stdout
    );
    assert!(!r.stdout.contains("deny \""), "{}", r.stdout);
    // A refused plan still leads with the error naming the policy.
    assert!(
        r.stderr
            .contains("constraint violations:\n- a network is no bigger than a /16  vpc = \"c\""),
        "{}",
        r.stderr
    );
}

#[test]
fn verbose_lists_every_policy_and_what_holds() {
    let s = stack("policy-block-v");
    let r = s.run(&["plan", "p.df", "-v"]);
    let b = block(&r.stdout);
    assert!(
        b.contains("  holds         networks are named")
            && b.contains("    net.vpc a")
            && b.contains("holds"),
        "{}",
        r.stdout
    );
}

#[test]
fn every_policy_holding_is_the_count_alone() {
    let s = Scratch::project("policy-block-holds");
    s.write(
        "p.df",
        "use fake\nresource net.vpc a { cidr = \"10.0.0.0/16\" }\n\
         deny \"no /8\" where v in net.vpc, v.cidr == \"10.0.0.0/8\"\n",
    );
    let r = s.run(&["plan", "p.df"]).success();
    assert_eq!(
        r.summary(),
        "plan: 1 change (1 create) over 1 tick; policy: 1 hold",
        "{}",
        r.stdout
    );
    assert_eq!(block(&r.stdout), "policy  1 hold", "{}", r.stdout);
}

#[test]
fn json_carries_each_policy() {
    let s = stack("policy-block-json");
    let r = s.run(&["plan", "p.df", "--json"]);
    let j: serde_json::Value = serde_json::from_str(&r.stdout).expect(&r.stdout);
    let marks: Vec<(&str, &str)> = j["policy"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| (p["mark"].as_str().unwrap(), p["text"].as_str().unwrap()))
        .collect();
    assert_eq!(
        marks,
        [
            ("fails", "a network is no bigger than a /16"),
            ("undetermined", "the database has no endpoint yet"),
            ("holds", "networks are named"),
        ],
        "{j:#}"
    );
    assert_eq!(j["policy"][0]["fails"][0]["resource"], "net.vpc c", "{j:#}");
    assert_eq!(j["policy"][0]["hold"], 2, "{j:#}");
}

/// A deny over the vms (`m in compute.vm`) undetermined while a vm's
/// `db_host` waits on the database's endpoint is about the vm: its
/// subject is the vm, at the path it reads, and the vm is not counted as
/// holding (it named the database, and counted the vm a hold).
#[test]
fn an_undetermined_deny_is_about_what_it_ranges_over() {
    let s = Scratch::project("policy-block-subject");
    s.write(
        "p.df",
        "use fake\n\
         resource db.postgres db { size = 1 }\n\
         resource compute.vm app { db_host = db.endpoint }\n\
         deny \"the vm reads no endpoint nowhere\" where m in compute.vm, m.db_host == \"nowhere\"\n",
    );
    let r = s.run(&["plan", "p.df", "--json"]);
    let j: serde_json::Value = serde_json::from_str(&r.stdout).expect(&r.stdout);
    let p = &j["policy"][0];
    assert_eq!(
        (&p["mark"], &p["hold"], &p["undetermined"]),
        (
            &serde_json::json!("undetermined"),
            &serde_json::json!(0),
            &serde_json::json!([{ "of": "compute.vm app", "until": "until db_host is known (tick 2)" }]),
        ),
        "{j:#}"
    );
}

/// A failing deny with no context is about each resource its firings
/// range over (`v in net.vpc`), counted as failing, not holding; one
/// that ranges over none (`where region == "eu"`) is about no subject.
/// It printed `fails` alone under the policy and counted every vpc a
/// hold but one.
#[test]
fn a_failing_deny_without_context_is_about_what_it_fired_for() {
    let s = Scratch::project("policy-block-fired");
    s.write(
        "p.df",
        "use fake\n\
         resource net.vpc a { cidr = \"10.0.0.0/16\" }\n\
         resource net.vpc b { cidr = \"10.0.0.0/16\" }\n\
         resource net.vpc c { cidr = \"10.3.0.0/16\" }\n\
         let region = \"eu\"\n\
         deny \"no network is 10.0/16\" where v in net.vpc, v.cidr == \"10.0.0.0/16\"\n\
         deny \"the region is not eu\" where region == \"eu\"\n",
    );
    let r = s.run(&["plan", "p.df", "--json"]);
    let j: serde_json::Value = serde_json::from_str(&r.stdout).expect(&r.stdout);
    let of = |i: usize| -> Vec<serde_json::Value> {
        j["policy"][i]["fails"]
            .as_array()
            .unwrap()
            .iter()
            .map(|f| f["resource"].clone())
            .collect()
    };
    assert_eq!(j["policy"][0]["text"], "no network is 10.0/16", "{j:#}");
    assert_eq!(of(0), ["net.vpc a", "net.vpc b"], "{j:#}");
    assert_eq!(j["policy"][0]["hold"], 1, "{j:#}");
    assert_eq!(j["policy"][1]["text"], "the region is not eu", "{j:#}");
    assert_eq!(of(1), [serde_json::Value::Null], "{j:#}");
}

/// A deny that refuses a plan with no changes is listed as in any plan:
/// the headline counts it, the block says it, and the plan ends refused,
/// not `up to date` (it said `stack p is up to date`, no block, exit 4,
/// the violation only on stderr). Neither `-q` nor `--json` says up to
/// date.
#[test]
fn a_failing_deny_on_a_plan_with_no_changes_is_in_the_block() {
    let s = Scratch::project("policy-block-empty");
    s.write("p.df", "use fake\ndeny \"never\" where 1 == 1\n");
    let r = s.run(&["plan", "p.df"]);
    assert_eq!(r.code, Some(4), "{}\n{}", r.stdout, r.stderr);
    assert_eq!(
        r.summary(),
        "plan: 0 changes; policy: 1 fails",
        "{}",
        r.stdout
    );
    assert_eq!(
        block(&r.stdout),
        "policy  1 fails\n  fails  never  p.df:2  1 fails\n    (no subject)",
        "{}",
        r.stdout
    );
    assert!(!r.stdout.contains("up to date"), "{}", r.stdout);
    let q = s.run(&["plan", "p.df", "-q"]);
    assert_eq!(q.code, Some(4), "{}\n{}", q.stdout, q.stderr);
    assert!(!q.stdout.contains("up to date"), "{}", q.stdout);
    let j = s.run(&["plan", "p.df", "--json"]);
    let j: serde_json::Value = serde_json::from_str(&j.stdout).expect(&j.stdout);
    assert_eq!(j["up_to_date"], false, "{j:#}");
}
