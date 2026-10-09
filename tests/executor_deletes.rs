//! Delete ordering and replacement: deletes in reverse dependency order;
//! a force_new change replaces, destroying first unless
//! create_before_destroy, when the old object stays deposed in state until
//! what depends on it has moved.

mod common;
use common::{Scratch, mock};

/// net.vpc sorts before its dependents; its delete still comes last.
#[test]
fn deletes_run_in_reverse_dependency_order() {
    let s = Scratch::new("delete-order");
    s.write(
        "p.df",
        r#"

resource net.vpc main { cidr = "10.0.0.0/16" }
resource net.vpc_peering p { vpc_id = ref(net.vpc, "main", "id") }
resource net.route_table r { peering_id = ref(net.vpc_peering, "p", "id") }
resource compute.vm keep { size = 1 }
use fake
"#,
    );
    mock(&s, &["apply"]).success();
    assert_eq!(
        s.json("w.state.json")["resources"]["net.route_table::r"]["deps"],
        serde_json::json!(["net.vpc_peering::p"])
    );
    s.write(
        "p.df",
        "\nresource compute.vm keep { size = 1 }\nuse fake\n",
    );
    let r = mock(&s, &["apply"]).success();
    // Each delete's line, without why it is gone (After R-149).
    let order: Vec<&str> = r
        .stdout
        .lines()
        .filter(|l| l.starts_with("  - "))
        .map(|l| l.trim_end_matches("not in the program").trim_end())
        .collect();
    assert_eq!(
        order,
        [
            "  - net.route_table r",
            "  - net.vpc_peering p",
            "  - net.vpc main"
        ],
        "{}",
        r.stdout
    );
    let w = s.json("w.json");
    assert_eq!(
        w["resources"]
            .as_object()
            .unwrap()
            .keys()
            .collect::<Vec<_>>(),
        ["compute.vm::keep"]
    );
}

/// A reference by bare name to an object an earlier apply made is an
/// edge as much as one to an object made in the same apply: its id is
/// known by then, and state still records the dependency, so the delete
/// waits for the dependent's.
#[test]
fn a_reference_to_an_object_an_earlier_apply_made_orders_its_delete() {
    let s = Scratch::new("delete-order-later");
    let first = r#"
resource iam.role app_role { name = "app", assume = { principals: ["x"] } }
resource iam.policy app_policy { name = "p", document = "{}" }
use fake
"#;
    s.write("p.df", first);
    mock(&s, &["apply"]).success();
    s.write(
        "p.df",
        &format!(
            "{first}resource iam.role reader {{ name = \"reader\", assume = {{ principals: [\"y\"] }}, policies = [app_policy] }}\n"
        ),
    );
    mock(&s, &["apply"]).success();
    assert_eq!(
        s.json("w.state.json")["resources"]["iam.role::reader"]["deps"],
        serde_json::json!(["iam.policy::app_policy"])
    );
    // The role sorts after the policy by type: its delete still comes
    // before the policy's.
    s.write("p.df", "\nuse fake\n");
    let r = mock(&s, &["apply"]).success();
    // Each delete's line, without why it is gone (After R-149).
    let order: Vec<&str> = r
        .stdout
        .lines()
        .filter(|l| l.starts_with("  - "))
        .map(|l| l.trim_end_matches("not in the program").trim_end())
        .collect();
    let at = |l: &str| order.iter().position(|o| *o == l);
    assert!(
        at("  - iam.role reader") < at("  - iam.policy app_policy"),
        "{}",
        r.stdout
    );
}

/// An object of a provider configured from another object (a kubeconfig
/// read off the server) depends on that object: state records it, and
/// the server's delete waits for the cluster's objects'.
#[test]
fn an_object_depends_on_what_its_providers_settings_are_made_from() {
    let s = Scratch::new("delete-order-configured");
    std::fs::create_dir_all(s.path("prov")).unwrap();
    std::os::unix::fs::symlink(
        common::exe("dform-provider-fake"),
        s.path("prov/dform-provider-fake"),
    )
    .unwrap();
    s.write(
        "p.df",
        r#"
use fake { source = "prov" }
resource db.postgres server { name = "server" }
use k8s { kubeconfig = str.format("kc@%s", server.endpoint) }
resource k8s.namespace ns { metadata.name = "app" }
"#,
    );
    mock(&s, &["apply"]).success();
    assert_eq!(
        s.json("w.state.json")["resources"]["k8s.namespace::ns"]["deps"],
        serde_json::json!(["db.postgres::server"])
    );
}

const NET: &str = r#"

resource net.vpc main { cidr = "10.0.0.0/16" }
resource net.subnet a { vpc_id = ref(net.vpc, "main", "id"), tier = "web" }
use fake
"#;

/// A force_new change is a replace; by default the old object goes first
/// and the new one takes its name. The subnet waits for the new id; the
/// mock without `fresh-ids` mints the same one, so tick 2 has nothing to do.
#[test]
fn a_force_new_change_replaces_destroying_first() {
    let s = Scratch::new("replace");
    s.write("p.df", NET);
    mock(&s, &["apply"]).success();
    s.write("p.df", &NET.replace("10.0.0.0/16", "10.1.0.0/16"));
    let r = mock(&s, &["apply"]).success();
    assert_eq!(
        r.stdout,
        r#"plan: 2 changes (1 update, 1 replace) over 2 ticks

tick 1  1 change
  ± net.vpc main  p.df:3  cidr forces replace
      cidr = "10.0.0.0/16" → "10.1.0.0/16"

tick 2  1 change
  waits on  main
  ~ net.subnet a  p.df:4
      vpc_id = "net.vpc:main" → main

stack p is up to date
tick 2 differs from the plan shown:
  - net.subnet a  update, no longer a change
"#
    );
    let w = s.json("w.json");
    assert_eq!(
        w["resources"]["net.vpc::main"]["attrs"]["cidr"],
        "10.1.0.0/16"
    );
    let st = s.json("w.state.json");
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
    mock(&s, &["apply"]).success();
    let cbd = format!(
        "{}lifecycle(main, \"create_before_destroy\")\n",
        NET.replace("10.0.0.0/16", "10.1.0.0/16")
    );
    s.write("p.df", &cbd);
    // Stop after tick 1: the old object is deposed in state.
    let r = mock(&s, &["apply", "--max-ticks", "1"]).failure();
    assert!(
        r.stdout.contains("  ± net.vpc main  (the new one first)  "),
        "{}",
        r.stdout
    );
    let st = s.json("w.state.json");
    assert_eq!(st["deposed"]["net.vpc::main"]["remote"], "main", "{st}");
    assert_eq!(st["resources"]["net.vpc::main"]["remote"], "main-2", "{st}");
    let w = s.json("w.json");
    assert!(w["resources"].get("net.vpc::main").is_some());
    assert!(w["resources"].get("net.vpc::main-2").is_some());
    // The next apply finishes: the dependent first, then the deposed object.
    let r = mock(&s, &["apply"]).success();
    assert!(
        r.stdout.contains(
            "plan: 2 changes (1 update, 1 delete) over 1 tick\n\ntick 1  2 remaining, resumed\n  \
             ~ net.subnet a  p.df:4\n      vpc_id = \"net.vpc:main\" → \"net.vpc:main-2\"\n  \
             - net.vpc main  (deposed)\n      cidr = \"10.0.0.0/16\"\n"
        ),
        "{}",
        r.stdout
    );
    let st = s.json("w.state.json");
    assert!(st.get("deposed").is_none(), "{st}");
    let w = s.json("w.json");
    assert_eq!(
        w["resources"]
            .as_object()
            .unwrap()
            .keys()
            .collect::<Vec<_>>(),
        ["net.subnet::a", "net.vpc::main-2"]
    );
    let r = mock(&s, &["plan"]).success();
    assert!(
        r.stdout.ends_with("stack p is up to date\n"),
        "{}",
        r.stdout
    );
}

/// In one apply the deposed object's delete is tick 2.
#[test]
fn create_before_destroy_in_one_apply_takes_two_ticks() {
    let s = Scratch::new("cbd-one");
    s.write("p.df", NET);
    mock(&s, &["apply"]).success();
    s.write(
        "p.df",
        &format!(
            "{}lifecycle(main, \"create_before_destroy\")\n",
            NET.replace("10.0.0.0/16", "10.1.0.0/16")
        ),
    );
    let r = mock(&s, &["apply"]).success();
    assert!(
        r.stdout
            .starts_with("plan: 2 changes (1 update, 1 replace) over 2 ticks\n"),
        "{}",
        r.stdout
    );
    assert!(
        r.stdout
            .contains("plan: 2 changes (1 update, 1 delete) over 1 tick\n\ntick 2  2 changes\n"),
        "{}",
        r.stdout
    );
    assert!(!r.stdout.contains("apply: complete"), "{}", r.stdout);
}
