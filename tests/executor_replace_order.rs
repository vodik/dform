//! Replacement order is the schema's: `type_replace(T, create_first |
//! destroy_first | either)`. `lifecycle(r, create_before_destroy)` picks
//! the order only where the schema allows either, and is an error on a
//! destroy_first type.

mod common;
use common::Scratch;

const NET: &str = r#"

resource net.vpc main { cidr = "10.0.0.0/16" }
resource net.subnet a { vpc_id = ref(net.vpc, "main", "id"), cidr = "10.0.1.0/24" }
"#;

/// Apply NET, change the subnet's cidr (force_new), and plan with the fake
/// schema plus `order.df`.
fn replan(name: &str, order: &str, extra: &str) -> (Scratch, common::Run) {
    let s = Scratch::new(name);
    s.write("order.df", order);
    let run = |s: &Scratch, args: &[&str]| {
        s.run(&common::on(
            "p.df",
            &[
                "--world",
                "w.json",
                "--provider",
                "fake",
                "--provider",
                "order.df",
            ],
            args,
        ))
    };
    s.write("p.df", NET);
    run(&s, &["apply"]).success();
    s.write(
        "p.df",
        &format!("{}{extra}", NET.replace("10.0.1.0/24", "10.0.2.0/24")),
    );
    let r = run(&s, &["plan"]);
    (s, r)
}

#[test]
fn either_destroys_first_unless_lifecycle_says_otherwise() {
    let (_s, r) = replan(
        "order-either",
        "type_replace(\"net.subnet\", \"either\")\n",
        "",
    );
    let r = r.success();
    assert!(
        r.stdout
            .contains("± net.subnet a  p.df:4  cidr is immutable"),
        "{}",
        r.stdout
    );
    let (_s, r) = replan(
        "order-either-cbd",
        "type_replace(\"net.subnet\", \"either\")\n",
        "lifecycle(a, \"create_before_destroy\")\n",
    );
    let r = r.success();
    assert!(
        r.stdout.contains("± net.subnet a  (the new one first)"),
        "{}",
        r.stdout
    );
}

/// A create_first type is replaced create-first with no lifecycle fact; the
/// fact is redundant there, not an error.
#[test]
fn create_first_needs_no_lifecycle_fact() {
    let (_s, r) = replan(
        "order-create-first",
        "type_replace(\"net.subnet\", \"create_first\")\n",
        "",
    );
    let r = r.success();
    assert!(
        r.stdout.contains("± net.subnet a  (the new one first)"),
        "{}",
        r.stdout
    );
    let (_s, r) = replan(
        "order-create-first-cbd",
        "type_replace(\"net.subnet\", \"create_first\")\n",
        "lifecycle(a, \"create_before_destroy\")\n",
    );
    assert!(
        r.success()
            .stdout
            .contains("± net.subnet a  (the new one first)"),
        "redundant lifecycle fact"
    );
}

/// create_before_destroy on a destroy_first type is an error naming the
/// type.
#[test]
fn create_before_destroy_on_a_destroy_first_type_is_an_error() {
    let (_s, r) = replan(
        "order-destroy-first",
        "type_replace(\"net.subnet\", \"destroy_first\")\n",
        "lifecycle(a, \"create_before_destroy\")\n",
    );
    let r = r.failure();
    assert!(
        r.stderr.contains(
            "lifecycle(net.subnet[\"a\"], create_before_destroy): type net.subnet is \
             type_replace destroy_first"
        ),
        "{}",
        r.stderr
    );
    let (_s, r) = replan(
        "order-destroy-first-plain",
        "type_replace(\"net.subnet\", \"destroy_first\")\n",
        "",
    );
    assert!(
        r.success()
            .stdout
            .contains("± net.subnet a  p.df:4  cidr is immutable"),
        "destroy_first without the fact"
    );
}

/// The mock schemas state the order of the types the decision names.
#[test]
fn mock_schemas_declare_their_replace_order() {
    use dform::schema::{ReplaceOrder, load_provider};
    let k8s = load_provider("k8s").unwrap();
    assert_eq!(
        k8s.replace_order("k8s.deployment"),
        ReplaceOrder::CreateFirst
    );
    assert_eq!(k8s.replace_order("k8s.service"), ReplaceOrder::CreateFirst);
    assert_eq!(
        k8s.replace_order("k8s.namespace"),
        ReplaceOrder::DestroyFirst
    );
    let aws = load_provider("aws-mock").unwrap();
    assert_eq!(aws.replace_order("aws.instance"), ReplaceOrder::CreateFirst);
    assert_eq!(
        aws.replace_order("aws.s3_bucket"),
        ReplaceOrder::DestroyFirst
    );
    assert_eq!(
        load_provider("fake").unwrap().replace_order("net.vpc"),
        ReplaceOrder::Either
    );
}
