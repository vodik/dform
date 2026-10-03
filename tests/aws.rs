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
            .contains("tags.web_az = ?aws.instance[\"web\"].availability_zone"),
        "{}",
        r.stdout
    );
    assert!(r.stdout.contains("password = (sensitive)"), "{}", r.stdout);
    assert!(!r.stdout.contains("correct-horse"), "{}", r.stdout);

    let a = run(&s, &args, "apply").success();
    assert!(!a.stdout.contains("correct-horse"), "{}", a.stdout);
    let w: serde_json::Value = serde_json::from_str(&s.read("w.json")).unwrap();
    let res = &w["resources"];
    // AWS picked what the program left unset, and nothing it set.
    let web_az = res["aws.instance::web"]["computed"]["availability_zone"]
        .as_str()
        .unwrap();
    assert_eq!(
        res["aws.instance::bastion"]["attrs"]["tags"]["web_az"],
        web_az
    );
    assert!(
        res["aws.instance::bastion"]["computed"]["subnet_id"]
            .as_str()
            .unwrap()
            .starts_with("subnet-")
    );
    assert!(
        res["aws.instance::web"]["computed"]
            .get("subnet_id")
            .is_none()
    );
    assert_eq!(
        res["aws.instance::web"]["attrs"]["subnet_id"],
        res["aws.subnet::a"]["computed"]["id"]
    );
    assert_eq!(
        res["aws.db_instance::app"]["computed"]["address"],
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
    let ingress = w["resources"]["aws.security_group::web"]["attrs"]["ingress"]
        .as_array_mut()
        .unwrap();
    ingress.reverse();
    s.write("w.json", &serde_json::to_string_pretty(&w).unwrap());
    let r = run(&s, &args, "plan").success();
    assert_eq!(r.summary(), "stack aws_demo is undeformed", "{}", r.stdout);

    // Someone opened port 22 by hand: an update of the set.
    let mut w: serde_json::Value = serde_json::from_str(&s.read("w.json")).unwrap();
    w["resources"]["aws.security_group::web"]["attrs"]["ingress"]
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
        r.stdout.contains("~ aws.security_group[\"web\"]"),
        "{}",
        r.stdout
    );
}

/// README's opening program on the mock: `aws.availability_zone` is a
/// table the provider answers (R-36), one row per zone with a stable
/// index, so the n-th zone gets the n-th /24.
const ZONES: &str = r#"

provider aws { region = "us-east-1" }

resource aws.vpc main {
  cidr_block = "10.0.0.0/16"
}

resource aws.subnet "private-${availability_zone}" {
  vpc_id = main
  cidr_block = inet.subnet(inet(main.cidr_block), 8, n)
  availability_zone
} where aws.availability_zone("available", availability_zone, n)
"#;

#[test]
fn a_data_source_is_a_table_with_an_index() {
    let s = Scratch::project("aws-zones");
    s.write(
        "dform.toml",
        "[project]\nedition = \"2026\"\n\n[providers]\naws = { source = \"aws-mock\" }\n",
    );
    s.write("main.df", ZONES);
    let r = s.run(&["plan", "main.df"]).success();
    assert_eq!(
        r.summary(),
        "plan: 4 deformations (4 create)",
        "{}",
        r.stdout
    );
    for (zone, cidr) in [
        ("us-east-1a", "10.0.0.0/24"),
        ("us-east-1b", "10.0.1.0/24"),
        ("us-east-1c", "10.0.2.0/24"),
    ] {
        let want = format!(
            "+ aws.subnet[\"private-{zone}\"]\n  availability_zone = \"{zone}\"\n  cidr_block = \"{cidr}\"\n"
        );
        assert!(r.stdout.contains(&want), "{want}\n{}", r.stdout);
    }
}

/// A type is its provider's, by its namespace: without `provider aws` the
/// plan names the statement to add.
#[test]
fn a_type_names_the_provider_to_declare() {
    let s = Scratch::project("aws-undeclared");
    s.write(
        "main.df",
        "\nprovider fake\nresource aws.vpc main { cidr_block = \"10.0.0.0/16\" }\n",
    );
    let r = s.run(&["plan", "main.df"]).failure();
    assert!(
        r.stderr
            .contains("provider fake does not declare aws.vpc; declared by: aws-mock"),
        "{}",
        r.stderr
    );
    assert!(
        r.stderr
            .contains("aws.vpc is provider aws's type: add `provider aws` to the stack"),
        "{}",
        r.stderr
    );
}
