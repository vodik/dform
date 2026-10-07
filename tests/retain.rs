//! `lifecycle(r, "retain")` (R-154): a planned delete of r becomes a
//! forget. No Delete is sent; state drops the object and the world keeps
//! it; the audit log says `forgot` with its remote id; the plan prints it
//! as its own kind, `~ T A  forgotten, kept in the world  (lifecycle
//! retain)`. It names an address no rule wants any more, and a destroy
//! honours it. With `prevent_destroy` on the same object it is an error.

mod common;
use common::{Run, Scratch};

const BOTH: &str = r#"
use fake
resource net.vpc main { cidr = "10.0.0.0/16" }
resource net.vpc data { cidr = "10.1.0.0/16" }
"#;

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
    let s = Scratch::new(name);
    s.write("p.df", BOTH);
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

const LINE: &str = "  ~ net.vpc data  forgotten, kept in the world  (lifecycle retain)\n";

#[test]
fn a_retained_object_no_rule_wants_is_forgotten_not_deleted() {
    let s = applied(
        "retain-apply",
        r#"
use fake
resource net.vpc main { cidr = "10.0.0.0/16" }
lifecycle(net.vpc["data"], "retain")
"#,
    );
    let p = dev(&s, &["plan"]).success();
    assert_eq!(
        p.summary(),
        "plan: 1 change (1 forget) over 1 tick",
        "{}",
        p.stdout
    );
    assert!(p.stdout.contains(LINE), "{}", p.stdout);
    // The same line at -v: there is nothing it changes to list.
    let v = dev(&s, &["plan", "-v"]).success();
    assert!(v.stdout.contains(LINE), "{}", v.stdout);
    let j = dev(&s, &["plan", "--json"]).success();
    assert!(j.stdout.contains("\"forget\""), "{}", j.stdout);
    dev(&s, &["apply", "--yes"]).success();
    // The world keeps it; state no longer does.
    assert_eq!(keys(&s.json("w.json")), ["net.vpc::data", "net.vpc::main"]);
    assert_eq!(keys(&s.json("w.state.json")), ["net.vpc::main"]);
    let log: Vec<serde_json::Value> = s
        .read("w.state.audit.jsonl")
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    let forgot: Vec<&serde_json::Value> = log.iter().filter(|e| e["kind"] == "forgot").collect();
    assert_eq!(forgot.len(), 1, "{log:?}");
    assert_eq!(forgot[0]["address"], "net.vpc[\"data\"]");
    assert_eq!(forgot[0]["remote"], "data");
    // No Delete was sent for it.
    assert!(
        !log.iter().any(|e| e["kind"] == "action"
            && e["address"] == "net.vpc[\"data\"]"
            && e["action"] == "delete"),
        "{log:?}"
    );
    let again = dev(&s, &["plan"]).success();
    assert_eq!(again.summary(), "stack p is up to date", "{}", again.stdout);
}

#[test]
fn a_destroy_forgets_what_the_program_retains() {
    let s = applied(
        "retain-destroy",
        &format!("{BOTH}lifecycle(data, \"retain\")\n"),
    );
    let p = dev(&s, &["plan", "--destroy"]).success();
    assert_eq!(
        p.summary(),
        "plan: 2 changes (1 delete, 1 forget) over 1 tick",
        "{}",
        p.stdout
    );
    // The resource is still in the program: its line keeps its site.
    assert!(
        p.stdout.contains(LINE.trim_end_matches('\n')),
        "{}",
        p.stdout
    );
    let r = dev(&s, &["destroy", "--yes"]).success();
    assert!(!r.stdout.contains("destroy: complete"), "{}", r.stdout);
    assert_eq!(keys(&s.json("w.json")), ["net.vpc::data"]);
    assert!(keys(&s.json("w.state.json")).is_empty());
    assert!(
        s.read("w.state.audit.jsonl")
            .contains("\"kind\":\"forgot\"")
    );
}

#[test]
fn prevent_destroy_and_retain_on_one_object_is_an_error_naming_both() {
    let s = applied(
        "retain-conflict",
        &format!("{BOTH}lifecycle(data, \"retain\")\nlifecycle(data, \"prevent_destroy\")\n"),
    );
    let r = dev(&s, &["plan"]).failure();
    assert!(
        r.stderr.contains(
            "lifecycle(net.vpc[\"data\"], \"prevent_destroy\") and \
             lifecycle(net.vpc[\"data\"], \"retain\") are both written"
        ),
        "{}",
        r.stderr
    );
}

/// On an object the program still wants, `retain` changes nothing.
#[test]
fn retain_on_a_wanted_object_plans_nothing() {
    let s = applied(
        "retain-wanted",
        &format!("{BOTH}lifecycle(data, \"retain\")\n"),
    );
    let r = dev(&s, &["plan"]).success();
    assert_eq!(r.summary(), "stack p is up to date", "{}", r.stdout);
}
