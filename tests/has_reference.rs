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
    assert!(!r.stdout.contains("apply: complete"), "{}", r.stdout);
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

/// `has r.p` of a computed attribute (After R-152): undetermined until the
/// provider reports it, so what it gates waits on it and applies the tick
/// after; `not has r.p` waits too, and once the value is known it does
/// not hold.
#[test]
fn has_a_computed_attribute_waits_until_it_is_known() {
    let s = Scratch::new("has-ref-computed");
    s.write(
        "p.df",
        r#"
use fake
resource db.postgres cache { size = 1 }
resource net.vpc web { cidr = "10.0.0.0/16" } where has cache.endpoint
resource net.vpc bare { cidr = "10.1.0.0/16" } where not has cache.endpoint
"#,
    );
    let r = dev(&s, &["plan"]).success();
    let tick1 = r.stdout.split("\nlater\n").next().unwrap();
    assert!(tick1.contains("+ db.postgres cache"), "{}", r.stdout);
    assert!(!tick1.contains("net.vpc"), "{}", r.stdout);
    assert!(
        waits(&r.stdout, "net.vpc web", "cache.endpoint"),
        "{}",
        r.stdout
    );
    assert!(
        waits(&r.stdout, "net.vpc bare", "cache.endpoint"),
        "{}",
        r.stdout
    );
    let r = dev(&s, &["apply", "--yes"]).success();
    let tick2 = r.stdout.split("\ntick 2").nth(1).unwrap_or("");
    assert!(tick2.contains("+ net.vpc web"), "{}", r.stdout);
    assert!(!tick2.contains("net.vpc bare"), "{}", r.stdout);
    let r = dev(&s, &["plan"]).success();
    assert!(r.stdout.contains("is up to date"), "{}", r.stdout);
}

/// A Kubernetes object's identity is `metadata.uid`, below the cell
/// (After R-152): `has ns` waits on it and says so, not on every computed
/// field of `metadata` (a list too long for the column, printed as none).
#[test]
fn has_a_kubernetes_object_waits_on_its_uid() {
    let s = Scratch::new("has-ref-k8s");
    s.write(
        "p.df",
        r#"
use k8s
resource k8s.namespace ns { metadata.name = "a" }
resource k8s.config_map gated { metadata.name = "b" } where has ns
"#,
    );
    let r = dev(&s, &["--provider", "k8s", "plan"]).success();
    assert!(
        waits(&r.stdout, "k8s.config_map gated", "ns.metadata.uid"),
        "{}",
        r.stdout
    );
}

/// A field of a computed object (After R-152): unknown while the object
/// is, so a clause over it, `has` and `not has` of it wait on the object.
#[test]
fn a_field_of_a_computed_object_waits_on_it() {
    let s = Scratch::new("has-ref-nested");
    s.write(
        "s.df",
        r#"
type_provider(x.box, "boxcloud")
type_provider(y.box, "boxcloud")
type_attr(x.box, "id", "string", ["computed", "id"])
type_attr(x.box, "status", "object", ["computed"])
"#,
    );
    s.write(
        "p.df",
        r#"
resource x.box b { size = 1 }
resource y.box eq { size = 2 } where b.status.ready == true
resource y.box yes { size = 3 } where has b.status.ready
resource y.box no { size = 4 } where not has b.status.ready
"#,
    );
    let r = dev(&s, &["--provider", "s.df", "plan"]).success();
    for name in ["eq", "yes", "no"] {
        assert!(
            waits(&r.stdout, &format!("y.box {name}"), "b.status"),
            "{name}: {}",
            r.stdout
        );
    }
}

/// `why` of a gated resource says the `has` as the program wrote it
/// (After R-152), never the compiler's `__known(__identity0)`: a
/// resource's identity, a computed attribute, and under `not`.
#[test]
fn why_says_the_has_that_holds_a_block_back() {
    let s = Scratch::new("has-ref-why");
    s.write(
        "p.df",
        r#"
use fake
resource db.postgres cache { size = 1 }
resource net.vpc web { cidr = "10.0.0.0/16" } where has cache
resource net.vpc ep { cidr = "10.1.0.0/16" } where has cache.endpoint
resource net.vpc bare { cidr = "10.2.0.0/16" } where not has cache.endpoint
"#,
    );
    for (addr, want) in [
        ("net.vpc web", "has cache: cache does not exist yet"),
        ("net.vpc ep", "has cache.endpoint: cache.endpoint is not known yet"),
        (
            "net.vpc bare",
            "not has cache.endpoint: cache.endpoint is not known yet",
        ),
    ] {
        let r = dev(&s, &["why", addr]).success();
        assert!(r.stdout.contains(want), "{addr}: {}", r.stdout);
        assert!(!r.stdout.contains("__"), "{addr}: {}", r.stdout);
    }
}

/// A variable bound to a resource is one (After R-152): `has c` with `c
/// in db.postgres` waits on the cache's identity, not a value test.
#[test]
fn has_a_variable_bound_to_a_resource_is_its_identity() {
    let s = Scratch::new("has-ref-var");
    s.write(
        "p.df",
        "use fake\nresource db.postgres cache { size = 1 }\n\
         resource net.vpc web { cidr = \"10.0.0.0/16\" } where c in db.postgres, has c\n",
    );
    let r = dev(&s, &["plan"]).success();
    assert!(waits(&r.stdout, "net.vpc web", "cache"), "{}", r.stdout);
}
