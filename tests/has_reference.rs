//! `has r` of a resource (R-152): it holds once the resource's identity
//! is known. A block gated `where has warm_cache` waits on the cache, as a
//! read of a computed value of it would, and applies the tick after it;
//! `not has old` is a guard that the resource does not exist; `has r.id`
//! is an error naming `has r`.

mod common;
use common::{Scratch, error};

const PROG: &str = r#"
input legacy: bool = false

use fake
resource db.postgres warm_cache { size = 1 }
resource db.postgres old { size = 1 } where legacy
resource net.vpc web { cidr = "10.0.0.0/16" } where has warm_cache
resource net.vpc fresh { cidr = "10.1.0.0/16" } where not has old
"#;

fn scratch(name: &str) -> Scratch {
    let s = Scratch::new(name);
    s.write("p.df", PROG);
    s
}

/// `dform dev --world w.json ARGS p.df`.
fn dev(s: &Scratch, args: &[&str]) -> common::Run {
    let mut all = vec!["dev", "--world", "w.json"];
    all.extend_from_slice(args);
    all.push("p.df");
    let out = common::dform()
        .args(&all)
        .env("DFORM_WAIT_POLL_MS", "50")
        .env("NO_COLOR", "1")
        .current_dir(&s.dir)
        .output()
        .unwrap();
    common::Run::from(out)
}

/// Whether the plan's `later` group has `what`, waiting on `on`.
fn waits(plan: &str, what: &str, on: &str) -> bool {
    let later = plan.split("\nlater\n").nth(1).unwrap_or("");
    later
        .lines()
        .any(|l| l.trim_start().starts_with(what) && l.ends_with(&format!("  waits on {on}")))
}

/// Before the cache exists the gated vpc is `later`, waiting on it; apply
/// makes the cache at tick 1 and the vpc at tick 2; then nothing waits.
#[test]
fn a_gate_on_a_resource_waits_the_tick_it_takes() {
    let s = scratch("has-ref-gate");
    let r = dev(&s, &["plan"]).success();
    let tick1 = r.stdout.split("\nlater\n").next().unwrap();
    assert!(tick1.contains("+ db.postgres warm_cache"), "{}", r.stdout);
    assert!(!tick1.contains("net.vpc web"), "{}", r.stdout);
    assert!(
        waits(&r.stdout, "net.vpc web", "warm_cache"),
        "{}",
        r.stdout
    );
    let r = dev(&s, &["apply", "--yes"]).success();
    let tick2 = r.stdout.split("\ntick 2").nth(1).unwrap_or("");
    assert!(tick2.contains("+ net.vpc web"), "{}", r.stdout);
    assert!(r.stdout.ends_with("apply: complete\n"), "{}", r.stdout);
    let r = dev(&s, &["plan"]).success();
    assert!(r.stdout.contains("is up to date"), "{}", r.stdout);
}

/// `not has old`: with no `old` wanted the vpc is made at once; with one
/// wanted it waits on it, and once `old` exists it is not made.
#[test]
fn not_has_a_resource_guards_on_its_absence() {
    let s = scratch("has-ref-not");
    let r = dev(&s, &["plan"]).success();
    let tick1 = r.stdout.split("\nlater\n").next().unwrap();
    assert!(tick1.contains("+ net.vpc fresh"), "{}", r.stdout);

    let s = scratch("has-ref-not-legacy");
    let r = dev(&s, &["plan", "--set", "legacy=true"]).success();
    assert!(waits(&r.stdout, "net.vpc fresh", "old"), "{}", r.stdout);
    let r = dev(&s, &["apply", "--yes", "--set", "legacy=true"]).success();
    assert!(r.stdout.contains("+ db.postgres old"), "{}", r.stdout);
    assert!(!r.stdout.contains("+ net.vpc fresh"), "{}", r.stdout);
    let r = dev(&s, &["plan", "--set", "legacy=true"]).success();
    assert!(!r.stdout.contains("net.vpc fresh"), "{}", r.stdout);
}

/// `has r.id` is the spelling `.id` took with it: an error naming
/// `has r`, positive and under `not`.
#[test]
fn has_an_id_is_an_error_naming_has_the_resource() {
    for clause in ["has cache.id", "not has cache.id"] {
        let e = error(&format!(
            "use fake\nresource db.postgres cache {{ size = 1 }}\n\
             resource net.vpc v {{ cidr = \"10.0.0.0/16\" }} where {clause}\n"
        ));
        assert!(
            e.contains("`has cache.id`: a program does not read an id; write `has cache`"),
            "{clause}: {e}"
        );
    }
}
