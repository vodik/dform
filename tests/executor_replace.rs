//! A replace re-plans its dependents: the replacement is a new object, so
//! every null that named the old one goes back to unresolved and what reads
//! it is updated after the replacement exists (chaos `fresh-ids`: every
//! Create mints a new id, as a real cloud does).

mod common;
use common::Scratch;

fn dform(s: &Scratch, args: &[&str]) -> common::Run {
    s.run(&common::on("p.df", &["--world", "w.json"], args))
}

fn json(s: &Scratch, f: &str) -> serde_json::Value {
    serde_json::from_str(&s.read(f)).unwrap()
}

const NET: &str = r#"edition 2026

resource net.vpc main { cidr = "10.0.0.0/16" }
resource net.subnet a { vpc_id = ref(net.vpc, "main", "id"), tier = "web" }
resource net.subnet b { vpc_id = ref(net.vpc, "main", "id"), tier = "db" }
"#;

/// What the world's subnets point at, and the vpc's id.
fn ids(s: &Scratch, vpc: &str) -> (String, Vec<String>) {
    let w = json(s, "w.json");
    let r = &w["resources"];
    let id = r[format!("net.vpc::{vpc}")]["computed"]["id"]
        .as_str()
        .unwrap()
        .to_string();
    let subnets = ["a", "b"]
        .iter()
        .map(|n| {
            r[format!("net.subnet::{n}")]["attrs"]["vpc_id"]
                .as_str()
                .unwrap()
                .to_string()
        })
        .collect();
    (id, subnets)
}

/// Destroy-first: tick 1 replaces the vpc and holds the subnets on its new
/// id; tick 2 updates them to it.
#[test]
fn a_replace_updates_its_dependents_after_the_create() {
    let s = Scratch::new("replace-deps");
    s.write("p.df", NET);
    dform(&s, &["apply", "--chaos", "fresh-ids"]).success();
    let (old, subnets) = ids(&s, "main");
    assert_eq!(subnets, [old.clone(), old.clone()]);

    s.write("p.df", &NET.replace("10.0.0.0/16", "10.1.0.0/16"));
    let r = dform(&s, &["plan"]).success();
    assert!(
        r.stdout.contains(
            "pending on ?net.vpc[\"main\"].id (resolves after tick 1):\n\
             ~ net.subnet[\"a\"]\n  vpc_id: \"net.vpc:main@1\" -> ?net.vpc[\"main\"].id\n"
        ),
        "{}",
        r.stdout
    );
    let r = dform(&s, &["apply", "--chaos", "fresh-ids"]).success();
    assert!(
        r.stdout
            .contains("tick 2:\nplan: 2 deformations (2 update)\n"),
        "{}",
        r.stdout
    );
    let (new, subnets) = ids(&s, "main");
    assert_ne!(new, old);
    assert_eq!(subnets, [new.clone(), new], "{}", r.stdout);
    let r = dform(&s, &["plan"]).success();
    assert!(
        r.stdout.ends_with("stack p is undeformed\n"),
        "{}",
        r.stdout
    );
}

/// Create-first: the subnets move in tick 2, before the deposed vpc is
/// deleted.
#[test]
fn create_before_destroy_moves_dependents_before_the_deposed_delete() {
    let s = Scratch::new("replace-deps-cbd");
    s.write("p.df", NET);
    dform(&s, &["apply", "--chaos", "fresh-ids"]).success();
    s.write(
        "p.df",
        &format!(
            "{}lifecycle(main, \"create_before_destroy\")\n",
            NET.replace("10.0.0.0/16", "10.1.0.0/16")
        ),
    );
    let r = dform(&s, &["apply", "--chaos", "fresh-ids"]).success();
    let tick2 = r.stdout.split("tick 2:\n").nth(1).unwrap_or_default();
    let order: Vec<&str> = tick2
        .lines()
        .filter(|l| l.starts_with("~ ") || l.starts_with("- "))
        .collect();
    assert_eq!(
        order,
        [
            "~ net.subnet[\"a\"]",
            "~ net.subnet[\"b\"]",
            "- net.vpc[\"main\"]  (deposed)"
        ],
        "{}",
        r.stdout
    );
    let (new, subnets) = ids(&s, "main-2");
    assert_eq!(subnets, [new.clone(), new], "{}", r.stdout);
    assert!(json(&s, "w.state.json").get("deposed").is_none());
}

/// A deposed object waits for its dependents: while the subnet is held (on
/// a new database's endpoint), the deposed vpc it still points at is not
/// deleted.
#[test]
fn a_deposed_object_is_held_while_a_dependent_is() {
    let s = Scratch::new("replace-deposed-held");
    let net = r#"edition 2026

resource net.vpc main { cidr = "10.0.0.0/16" }
resource net.subnet a { vpc_id = ref(net.vpc, "main", "id"), note = "x" }
"#;
    s.write("p.df", net);
    dform(&s, &["apply"]).success();
    let cbd = format!(
        "{}lifecycle(main, \"create_before_destroy\")\n",
        net.replace("10.0.0.0/16", "10.1.0.0/16")
    );
    s.write("p.df", &cbd);
    dform(&s, &["apply", "--max-ticks", "1"]).failure();
    assert!(
        json(&s, "w.state.json")["deposed"]
            .get("net.vpc::main")
            .is_some()
    );

    s.write(
        "p.df",
        &format!(
            "{}resource db.postgres d {{ size = 1 }}\n",
            cbd.replace("note = \"x\"", "note = d.endpoint")
        ),
    );
    let r = dform(&s, &["plan"]).success();
    assert!(
        r.stdout.contains(
            "pending on ?db.postgres[\"d\"].endpoint (resolves after tick 1):\n\
             ~ net.subnet[\"a\"]\n"
        ) && r.stdout.contains("- net.vpc[\"main\"]  (deposed)\n"),
        "{}",
        r.stdout
    );
    assert!(
        r.stdout
            .contains("apply order:\n  tick 1\n    db.postgres[\"d\"]\n  tick 2\n    net.subnet[\"a\"]\n    net.vpc[\"main\"]"),
        "{}",
        r.stdout
    );
    dform(&s, &["apply", "--max-ticks", "1"]).failure();
    assert!(
        json(&s, "w.state.json")["deposed"]
            .get("net.vpc::main")
            .is_some(),
        "the deposed vpc was deleted while the subnet still pointed at it"
    );
    dform(&s, &["apply"]).success();
    assert!(json(&s, "w.state.json").get("deposed").is_none());
    let w = json(&s, "w.json");
    assert_eq!(
        w["resources"]["net.subnet::a"]["attrs"]["vpc_id"],
        w["resources"]["net.vpc::main-2"]["computed"]["id"]
    );
}
