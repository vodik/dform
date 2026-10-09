//! A replace re-plans its dependents: the replacement is a new object, so
//! every null that named the old one goes back to unresolved and what reads
//! it is updated after the replacement exists (chaos `fresh-ids`: every
//! Create mints a new id, as a real cloud does).

mod common;
use common::{Scratch, mock};

const NET: &str = r#"

resource net.vpc main { cidr = "10.0.0.0/16" }
resource net.subnet a { vpc_id = ref(net.vpc, "main", "id"), tier = "web" }
resource net.subnet b { vpc_id = ref(net.vpc, "main", "id"), tier = "db" }
use fake
"#;

/// What the world's subnets point at, and the vpc's id.
fn ids(s: &Scratch, vpc: &str) -> (String, Vec<String>) {
    let w = s.json("w.json");
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
    mock(&s, &["apply", "--chaos", "fresh-ids"]).success();
    let (old, subnets) = ids(&s, "main");
    assert_eq!(subnets, [old.clone(), old.clone()]);

    s.write("p.df", &NET.replace("10.0.0.0/16", "10.1.0.0/16"));
    let r = mock(&s, &["plan"]).success();
    assert!(
        r.stdout.contains(
            "tick 2  2 changes\n  waits on  main\n  \
             ~ net.subnet a  p.df:4\n      vpc_id = \"net.vpc:main@1\" → main\n"
        ),
        "{}",
        r.stdout
    );
    let r = mock(&s, &["apply", "--chaos", "fresh-ids"]).success();
    assert!(
        r.stdout
            .contains("plan: 2 changes (2 update) over 1 tick\n\ntick 2  2 changes\n"),
        "{}",
        r.stdout
    );
    let (new, subnets) = ids(&s, "main");
    assert_ne!(new, old);
    assert_eq!(subnets, [new.clone(), new], "{}", r.stdout);
    let r = mock(&s, &["plan"]).success();
    assert!(
        r.stdout.ends_with("stack p is up to date\n"),
        "{}",
        r.stdout
    );
}

/// Create-first: the subnets move in tick 2, before the deposed vpc is
/// deleted.
#[test]
fn create_first_moves_dependents_before_the_deposed_delete() {
    let s = Scratch::new("replace-deps-cbd");
    s.write("p.df", NET);
    mock(&s, &["apply", "--chaos", "fresh-ids"]).success();
    s.write(
        "p.df",
        &format!(
            "{}lifecycle(main, \"create_first\")\n",
            NET.replace("10.0.0.0/16", "10.1.0.0/16")
        ),
    );
    let r = mock(&s, &["apply", "--chaos", "fresh-ids"]).success();
    // Tick 2's calls in the order they were made, as the audit log has
    // them.
    let order: Vec<(String, String)> = s
        .read("w.state.audit.jsonl")
        .lines()
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .filter(|e| e["kind"] == "action" && e["tick"] == 2)
        .map(|e| {
            let s = |k: &str| e[k].as_str().unwrap_or_default().to_string();
            (s("action"), s("address"))
        })
        .collect();
    let call = |a: &str, b: &str| (a.to_string(), b.to_string());
    assert_eq!(
        order,
        [
            call("update", "net.subnet[\"a\"]"),
            call("update", "net.subnet[\"b\"]"),
            call("delete_deposed", "net.vpc[\"main\"]"),
        ],
        "{}",
        r.stdout
    );
    let (new, subnets) = ids(&s, "main-2");
    assert_eq!(subnets, [new.clone(), new], "{}", r.stdout);
    assert!(s.json("w.state.json").get("deposed").is_none());
}

/// A deposed object waits for its dependents: while the subnet is held (on
/// a new database's endpoint), the deposed vpc it still points at is not
/// deleted.
#[test]
fn a_deposed_object_is_held_while_a_dependent_is() {
    let s = Scratch::new("replace-deposed-held");
    let net = r#"

resource net.vpc main { cidr = "10.0.0.0/16" }
resource net.subnet a { vpc_id = ref(net.vpc, "main", "id"), note = "x" }
use fake
"#;
    s.write("p.df", net);
    mock(&s, &["apply"]).success();
    let cbd = format!(
        "{}lifecycle(main, \"create_first\")\n",
        net.replace("10.0.0.0/16", "10.1.0.0/16")
    );
    s.write("p.df", &cbd);
    mock(&s, &["apply", "--max-ticks", "1"]).failure();
    assert!(
        s.json("w.state.json")["deposed"]
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
    let r = mock(&s, &["plan"]).success();
    assert!(
        r.stdout.contains(
            "tick 2  2 changes\n  waits on  d.endpoint\n  \
             ~ net.subnet a   p.df:4\n      note = \"x\" → d.endpoint\n"
        ) && r.stdout.contains("  - net.vpc main  (deposed)\n"),
        "{}",
        r.stdout
    );
    assert!(
        r.stdout.contains("tick 1  1 change\n  + db.postgres d  "),
        "{}",
        r.stdout
    );
    mock(&s, &["apply", "--max-ticks", "1"]).failure();
    assert!(
        s.json("w.state.json")["deposed"]
            .get("net.vpc::main")
            .is_some(),
        "the deposed vpc was deleted while the subnet still pointed at it"
    );
    mock(&s, &["apply"]).success();
    assert!(s.json("w.state.json").get("deposed").is_none());
    let w = s.json("w.json");
    assert_eq!(
        w["resources"]["net.subnet::a"]["attrs"]["vpc_id"],
        w["resources"]["net.vpc::main-2"]["computed"]["id"]
    );
}
