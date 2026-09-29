//! Delete ordering and replacement: deletes in reverse dependency order;
//! a force_new change replaces, destroying first unless
//! create_before_destroy, when the old object stays deposed in state until
//! what depends on it has moved.

mod common;
use common::Scratch;

fn dform(s: &Scratch, args: &[&str]) -> common::Run {
    s.run(&common::on("p.df", &["--world", "w.json"], args))
}

fn json(s: &Scratch, f: &str) -> serde_json::Value {
    serde_json::from_str(&s.read(f)).unwrap()
}

/// net.vpc sorts before its dependents; its delete still comes last.
#[test]
fn deletes_run_in_reverse_dependency_order() {
    let s = Scratch::new("delete-order");
    s.write(
        "p.df",
        r#"edition 2026

resource net.vpc main { cidr = "10.0.0.0/16" }
resource net.vpc_peering p { vpc_id = ref(net.vpc, "main", "id") }
resource net.route r { peering_id = ref(net.vpc_peering, "p", "id") }
resource compute.vm keep { size = 1 }
"#,
    );
    dform(&s, &["apply"]).success();
    assert_eq!(
        json(&s, "w.state.json")["resources"]["net.route::r"]["deps"],
        serde_json::json!(["net.vpc_peering::p"])
    );
    s.write(
        "p.df",
        "edition 2026\nresource compute.vm keep { size = 1 }\n",
    );
    let r = dform(&s, &["apply"]).success();
    let order: Vec<&str> = r.stdout.lines().filter(|l| l.starts_with("- ")).collect();
    assert_eq!(
        order,
        [
            "- net.route[\"r\"]",
            "- net.vpc_peering[\"p\"]",
            "- net.vpc[\"main\"]"
        ],
        "{}",
        r.stdout
    );
    let w = json(&s, "w.json");
    assert_eq!(
        w["resources"]
            .as_object()
            .unwrap()
            .keys()
            .collect::<Vec<_>>(),
        ["compute.vm::keep"]
    );
}

const NET: &str = r#"edition 2026

resource net.vpc main { cidr = "10.0.0.0/16" }
resource net.subnet a { vpc_id = ref(net.vpc, "main", "id"), tier = "web" }
"#;

/// A force_new change is a replace; by default the old object goes first
/// and the new one takes its name. The subnet waits for the new id; the
/// mock without `fresh-ids` mints the same one, so tick 2 has nothing to do.
#[test]
fn a_force_new_change_replaces_destroying_first() {
    let s = Scratch::new("replace");
    s.write("p.df", NET);
    dform(&s, &["apply"]).success();
    s.write("p.df", &NET.replace("10.0.0.0/16", "10.1.0.0/16"));
    let r = dform(&s, &["apply"]).success();
    assert_eq!(
        r.stdout,
        "tick 1:\nplan: 1 deformation (1 replace), 1 pending\ndefinite:\n\
         -/+ net.vpc[\"main\"]  (replace)\n  cidr: \"10.0.0.0/16\" -> \"10.1.0.0/16\"\n\
         pending on ?net.vpc[\"main\"].id (resolves after tick 1):\n\
         ~ net.subnet[\"a\"]\n  vpc_id: \"net.vpc:main\" -> ?net.vpc[\"main\"].id\n\
         apply order:\n  tick 1\n    net.vpc[\"main\"]\n  tick 2\n    net.subnet[\"a\"]\n\
         tick 2:\nstack p is undeformed\n\
         apply: complete\n"
    );
    let w = json(&s, "w.json");
    assert_eq!(
        w["resources"]["net.vpc::main"]["attrs"]["cidr"],
        "10.1.0.0/16"
    );
    let st = json(&s, "w.state.json");
    assert_eq!(st["resources"]["net.vpc::main"]["remote"], "main");
    assert!(st.get("deposed").is_none(), "{st}");
}

/// create_before_destroy: tick 1 creates the replacement under a new name
/// and deposes the old object; tick 2 moves the subnet to the new vpc and
/// then deletes the deposed one.
#[test]
fn create_before_destroy_deposes_the_old_object_until_dependents_move() {
    let s = Scratch::new("cbd");
    s.write("p.df", NET);
    dform(&s, &["apply"]).success();
    let cbd = format!(
        "{}lifecycle(net.vpc, \"main\", \"create_before_destroy\")\n",
        NET.replace("10.0.0.0/16", "10.1.0.0/16")
    );
    s.write("p.df", &cbd);
    // Stop after tick 1: the old object is deposed in state.
    let r = dform(&s, &["apply", "--max-ticks", "1"]).failure();
    assert!(
        r.stdout.contains("+/- net.vpc[\"main\"]  (replace)"),
        "{}",
        r.stdout
    );
    let st = json(&s, "w.state.json");
    assert_eq!(st["deposed"]["net.vpc::main"]["remote"], "main", "{st}");
    assert_eq!(st["resources"]["net.vpc::main"]["remote"], "main-2", "{st}");
    let w = json(&s, "w.json");
    assert!(w["resources"].get("net.vpc::main").is_some());
    assert!(w["resources"].get("net.vpc::main-2").is_some());
    // The next apply finishes: the dependent first, then the deposed object.
    let r = dform(&s, &["apply"]).success();
    assert!(
        r.stdout.contains(
            "plan: 2 deformations (1 update, 1 delete)\ndefinite:\n\
             ~ net.subnet[\"a\"]\n  vpc_id: \"net.vpc:main\" -> \"net.vpc:main-2\"\n\
             - net.vpc[\"main\"]  (deposed)\n  cidr was \"10.0.0.0/16\"\n"
        ),
        "{}",
        r.stdout
    );
    let st = json(&s, "w.state.json");
    assert!(st.get("deposed").is_none(), "{st}");
    let w = json(&s, "w.json");
    assert_eq!(
        w["resources"]
            .as_object()
            .unwrap()
            .keys()
            .collect::<Vec<_>>(),
        ["net.subnet::a", "net.vpc::main-2"]
    );
    let r = dform(&s, &["plan"]).success();
    assert!(
        r.stdout.ends_with("stack p is undeformed\n"),
        "{}",
        r.stdout
    );
}

/// In one apply the deposed object's delete is tick 2.
#[test]
fn create_before_destroy_in_one_apply_takes_two_ticks() {
    let s = Scratch::new("cbd-one");
    s.write("p.df", NET);
    dform(&s, &["apply"]).success();
    s.write(
        "p.df",
        &format!(
            "{}lifecycle(net.vpc, \"main\", \"create_before_destroy\")\n",
            NET.replace("10.0.0.0/16", "10.1.0.0/16")
        ),
    );
    let r = dform(&s, &["apply"]).success();
    assert!(
        r.stdout
            .starts_with("tick 1:\nplan: 1 deformation (1 replace), 1 pending\n"),
        "{}",
        r.stdout
    );
    assert!(
        r.stdout
            .contains("tick 2:\nplan: 2 deformations (1 update, 1 delete)\n"),
        "{}",
        r.stdout
    );
    assert!(r.stdout.ends_with("apply: complete\n"), "{}", r.stdout);
}
