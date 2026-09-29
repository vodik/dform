//! The Terraform-shaped mock AWS provider: Optional+Computed and keyless sets.

mod common;
use common::{Scratch, repo};

fn demo() -> String {
    let prog = repo().join("examples/aws/stacks/aws_demo.df");
    prog.to_str().unwrap().to_string()
}

fn run(s: &Scratch, prog: &str, cmd: &str) -> common::Run {
    s.run(&common::on(
        prog,
        &["--provider", "aws-mock", "--world", "w.json"],
        &[cmd],
    ))
}

#[test]
fn optional_computed_is_a_constant_when_set_and_a_null_when_not() {
    let s = Scratch::new("aws-oc");
    let args = demo();
    let r = run(&s, &args, "plan").success();
    assert_eq!(
        r.summary(),
        "plan: 7 deformations (7 create)",
        "{}",
        r.stdout
    );
    assert!(
        r.stdout.contains("tags.subnet_az = \"us-east-1a\""),
        "{}",
        r.stdout
    );
    assert!(
        r.stdout
            .contains("tags.web_az = ?aws_instance[\"web\"].availability_zone"),
        "{}",
        r.stdout
    );
    assert!(r.stdout.contains("let password = (sensitive)"), "{}", r.stdout);
    assert!(!r.stdout.contains("correct-horse"), "{}", r.stdout);

    let a = run(&s, &args, "apply").success();
    assert!(!a.stdout.contains("correct-horse"), "{}", a.stdout);
    let w: serde_json::Value = serde_json::from_str(&s.read("w.json")).unwrap();
    let res = &w["resources"];
    // AWS picked what the program left unset, and nothing it set.
    let web_az = res["aws_instance::web"]["computed"]["availability_zone"]
        .as_str()
        .unwrap();
    assert_eq!(
        res["aws_instance::bastion"]["attrs"]["tags"]["web_az"],
        web_az
    );
    assert!(
        res["aws_instance::bastion"]["computed"]["subnet_id"]
            .as_str()
            .unwrap()
            .starts_with("subnet-")
    );
    assert!(
        res["aws_instance::web"]["computed"]
            .get("subnet_id")
            .is_none()
    );
    assert_eq!(
        res["aws_instance::web"]["attrs"]["subnet_id"],
        res["aws_subnet::a"]["computed"]["id"]
    );
    assert_eq!(
        res["aws_db_instance::app"]["computed"]["address"],
        "app.c123abc.us-east-1.rds.amazonaws.com"
    );

    let r = run(&s, &args, "plan").success();
    assert_eq!(r.summary(), "stack aws_demo is undeformed", "{}", r.stdout);
}

#[test]
fn keyless_sets_ignore_order() {
    let s = Scratch::new("aws-sets");
    let args = demo();
    run(&s, &args, "apply").success();

    // AWS returns the rules in another order: not a change.
    let mut w: serde_json::Value = serde_json::from_str(&s.read("w.json")).unwrap();
    let ingress = w["resources"]["aws_security_group::web"]["attrs"]["ingress"]
        .as_array_mut()
        .unwrap();
    ingress.reverse();
    s.write("w.json", &serde_json::to_string_pretty(&w).unwrap());
    let r = run(&s, &args, "plan").success();
    assert_eq!(r.summary(), "stack aws_demo is undeformed", "{}", r.stdout);

    // Someone opened port 22 by hand: an update of the set.
    let mut w: serde_json::Value = serde_json::from_str(&s.read("w.json")).unwrap();
    w["resources"]["aws_security_group::web"]["attrs"]["ingress"]
        .as_array_mut()
        .unwrap()
        .push(serde_json::json!({"from_port": 22, "to_port": 22, "protocol": "tcp", "cidr_blocks": ["0.0.0.0/0"]}));
    s.write("w.json", &serde_json::to_string_pretty(&w).unwrap());
    let r = run(&s, &args, "plan").success();
    assert_eq!(
        r.summary(),
        "plan: 1 deformation (1 update)",
        "{}",
        r.stdout
    );
    assert!(
        r.stdout.contains("~ aws_security_group[\"web\"]"),
        "{}",
        r.stdout
    );
}
