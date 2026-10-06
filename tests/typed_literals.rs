//! References are the resource (R-43) and literals take the type their
//! position expects (R-31). The errors each have a file under
//! tests/syntax/err (references.df, typed_literals.df); these are the
//! positive cases and the checks that need the provider's schema.

mod common;
use common::Scratch;

fn plan(s: &Scratch) -> common::Run {
    s.run(&common::on(
        "p.df",
        &["--world", "w.json"],
        &["plan", "--why=none"],
    ))
}

fn program(body: &str) -> String {
    format!("\n\n{body}provider fake\n")
}

/// `vpc = main` gives the subnet the vpc; the plan prints the resource,
/// unknown until it exists, then known: never its id.
#[test]
fn an_attribute_typed_ref_takes_the_resource() {
    let s = Scratch::new("r43-ref");
    s.write(
        "p.df",
        &program(
            "resource net.vpc main { cidr = \"10.0.0.0/16\" }\n\
             resource net.subnet a { vpc = main, cidr = \"10.0.1.0/24\" }\n\
             resource db.postgres d { subnets = [ s | s in net.subnet ] }\n",
        ),
    );
    let r = plan(&s).success();
    assert!(
        r.stdout.contains("  vpc = ?net.vpc[\"main\"]\n"),
        "{}",
        r.stdout
    );
    assert!(
        r.stdout.contains("  subnets[0] = ?net.subnet[\"a\"]\n"),
        "{}",
        r.stdout
    );
    s.run(&common::on("p.df", &["--world", "w.json"], &["apply"]))
        .success();
    // The provider got the id; the program holds the reference.
    let w: serde_json::Value = serde_json::from_str(&s.read("w.json")).unwrap();
    assert_eq!(
        w["resources"]["net.subnet::a"]["attrs"]["vpc"],
        "net.vpc:main"
    );
    let q = s
        .run(&common::on(
            "p.df",
            &["--world", "w.json"],
            &["query", "net.subnet[\"a\"].vpc"],
        ))
        .success();
    assert!(q.stdout.contains("net.vpc[\"main\"]"), "{}", q.stdout);
    // A new subnet in the existing vpc prints the vpc, not its id.
    s.write(
        "p.df",
        &program(
            "resource net.vpc main { cidr = \"10.0.0.0/16\" }\n\
             resource net.subnet a { vpc = main, cidr = \"10.0.1.0/24\" }\n\
             resource net.subnet b { vpc = main, cidr = \"10.0.2.0/24\" }\n\
             resource db.postgres d { subnets = [ s | s in net.subnet ] }\n",
        ),
    );
    let r = plan(&s).success();
    assert!(
        r.stdout
            .contains("+ net.subnet[\"b\"]\n  cidr = \"10.0.2.0/24\"\n  vpc = net.vpc[\"main\"]\n"),
        "{}",
        r.stdout
    );
    let j = s
        .run(&common::on(
            "p.df",
            &["--world", "w.json"],
            &["plan", "--why=none", "--json"],
        ))
        .success();
    assert!(j.stdout.contains("\"net.vpc:main\""), "{}", j.stdout);
}

/// Inside the module that declares it, a bare resource name given as a
/// value is the reference: `vpc = vpc` and the pun `vpc` (R-43 amendment 3).
#[test]
fn a_resource_name_is_the_reference_inside_its_module() {
    for entry in ["vpc = vpc", "vpc"] {
        let s = Scratch::new("r43-module");
        s.write(
            "p.df",
            &program(&format!(
                "component network {{\n  input cidr: inet\n  output vpc: net.vpc = vpc\n  \
                 resource net.vpc vpc {{ cidr }}\n  \
                 resource net.subnet a {{ {entry}, cidr = inet.subnet(cidr, 8, 1) }}\n}}\n\
                 instance network blue {{ cidr = \"10.1.0.0/16\" }}\n\
                 resource net.vpc_peering p {{ requester_vpc = blue.vpc, accepter_vpc = blue.vpc }}\n"
            )),
        );
        let r = plan(&s).success();
        assert!(
            r.stdout.contains("  vpc = ?net.vpc[\"blue.vpc\"]\n"),
            "{entry}: {}",
            r.stdout
        );
        assert!(
            r.stdout
                .contains("  requester_vpc = ?net.vpc[\"blue.vpc\"]\n"),
            "{entry}: {}",
            r.stdout
        );
        // The module input's literal was read as an inet (R-31).
        assert!(
            r.stdout.contains("  cidr = \"10.1.1.0/24\"\n"),
            "{entry}: {}",
            r.stdout
        );
    }
}

/// The schema types `vpc` as `ref(net.vpc)`: a string, or a reference of
/// another type, is an error at the entry naming both.
#[test]
fn a_ref_attribute_refuses_what_is_no_reference_of_its_type() {
    let s = Scratch::new("r43-ref-type");
    s.write(
        "p.df",
        &program(
            "resource net.vpc main { cidr = \"10.0.0.0/16\" }\n\
             resource net.subnet a { vpc = main }\n\
             resource net.subnet b { vpc = \"main\" }\n\
             resource net.subnet c { vpc = a }\n\
             resource db.postgres d { subnets = [main] }\n\
             resource k8s.nodepool n { cluster = ref(main) }\n",
        ),
    );
    let r = plan(&s).failure();
    for want in [
        "p.df:5:25: net.subnet[\"b\"].vpc takes a ref(net.vpc): a resource",
        "not the string \"main\"",
        "p.df:6:25: net.subnet[\"c\"].vpc takes a ref(net.vpc), got net.subnet[\"a\"]",
        "p.df:7:26: db.postgres[\"d\"].subnets takes a ref(net.subnet), got net.vpc[\"main\"]",
        "p.df:8:27: k8s.nodepool[\"n\"].cluster takes a ref(k8s.cluster), got net.vpc[\"main\"]",
    ] {
        assert!(r.stderr.contains(want), "{want}\n---\n{}", r.stderr);
    }
}

/// A `string` attribute takes no reference, unless it is written out as
/// `ref(r)`, the id where an API wants one as text.
#[test]
fn a_string_attribute_takes_a_reference_only_written_out() {
    let s = Scratch::new("r43-string");
    s.write(
        "schema.df",
        "\n\
         type_provider(app.thing, \"mock\")\n\
         type_attr(app.thing, \"id\", \"string\", [\"computed\", \"id\"])\n\
         type_attr(app.thing, \"owner\", \"string\", [])\n\
         type_attr(app.thing, \"port\", \"int\", [])\n\
         type_attr(app.thing, \"net\", \"inet\", [])\n",
    );
    let run = |body: &str| {
        s.write("p.df", &format!("\n{body}"));
        s.run(&common::on(
            "p.df",
            &["--provider", "schema.df", "--world", "w.json"],
            &["plan", "--why=none"],
        ))
    };
    let r = run("resource app.thing a { port = 1 }\n\
         resource app.thing b { owner = ref(a), net = \"10.0.0.0/8\" }\n")
    .success();
    assert!(
        r.stdout.contains("owner = ?app.thing[\"a\"]"),
        "{}",
        r.stdout
    );
    let r = run("resource app.thing a { port = \"1\" }\n\
         resource app.thing b { owner = a, net = \"10.0.0/8\" }\n")
    .failure();
    for want in [
        "app.thing[\"a\"].port is int, not the string \"1\"",
        "app.thing[\"b\"].owner is string, not a reference: app.thing[\"a\"] is no string",
        "or write `ref(r)`",
        "app.thing[\"b\"].net is an inet: \"10.0.0/8\" is not a network",
    ] {
        assert!(r.stderr.contains(want), "{want}\n---\n{}", r.stderr);
    }
}

/// A string literal where an `inet` is declared is one: an input's
/// default, a function's argument (R-31).
#[test]
fn a_literal_takes_its_declared_type() {
    let s = Scratch::new("r31-literal");
    s.write(
        "p.df",
        &program(
            "input net: inet = \"10.0.0.0/16\"\n\
             resource net.vpc main { cidr = inet.subnet(net, 8, 1) }\n\
             resource net.vpc other { cidr = inet.subnet(\"10.9.0.0/16\", 8, 2) }\n",
        ),
    );
    let r = plan(&s).success();
    assert!(r.stdout.contains("cidr = \"10.0.1.0/24\""), "{}", r.stdout);
    assert!(r.stdout.contains("cidr = \"10.9.2.0/24\""), "{}", r.stdout);
}

/// An instance's literal input that is not its declared type is an error
/// at the entry.
#[test]
fn an_instance_input_literal_is_checked() {
    let s = Scratch::new("r31-instance");
    s.write(
        "p.df",
        &program(
            "component m {\n  input cidr: inet\n  resource net.vpc v { cidr }\n}\n\
             instance m one { cidr = \"nope\" }\n",
        ),
    );
    let r = plan(&s).failure();
    assert!(
        r.stderr
            .contains("p.df:7:18: input cidr of component m is an inet: \"nope\" is not a network"),
        "{}",
        r.stderr
    );
}

/// `r == main` compares references; `s.vpc == main` reads a reference.
#[test]
fn references_compare_as_references() {
    let s = Scratch::new("r43-compare");
    s.write(
        "p.df",
        &program(
            "resource net.vpc main { cidr = \"10.0.0.0/16\" }\n\
             resource net.vpc other { cidr = \"10.1.0.0/16\" }\n\
             resource net.subnet a { vpc = main }\n\
             resource net.subnet b { vpc = other }\n\
             in_main(s) where s in net.subnet, s.vpc == main\n",
        ),
    );
    let q = s
        .run(&common::on(
            "p.df",
            &["--world", "w.json"],
            &["query", "in_main(s)"],
        ))
        .success();
    assert!(q.stdout.contains("\"a\""), "{}", q.stdout);
    assert!(!q.stdout.contains("\"b\""), "{}", q.stdout);
}
