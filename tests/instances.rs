//! A component is a resource type the program defines (R-65, R-67): its
//! copies are instances, named and printed as resources are.

mod common;
use common::Scratch;

/// A component, `vpc`, copied twice, one copy gated by a clause.
const NET: &str = r#"
input env: string = "dev"
provider fake
component vpc {
  input vpc_net: string
  output id = vpc.cidr
  resource net.vpc vpc { cidr = vpc_net }
  resource net.subnet a { cidr = vpc_net, vpc_id = ref(vpc) }
}
instance vpc blue { vpc_net = "10.1.0.0/16" }
instance vpc green { vpc_net = "10.2.0.0/16" } where env != "dev"
"#;

fn project(name: &str, extra: &str) -> Scratch {
    let s = Scratch::project(name);
    s.write("main.df", &format!("{NET}{extra}"));
    s
}

/// `why` prints a copy's own relations by their names there, and a gated
/// copy's gate as the statement that makes it (R-73 item 3), never in the
/// core's spelling (`green::__instance("vpc")`, `green::vpc_net(..)`).
#[test]
fn why_prints_a_copys_relations_in_its_frame() {
    let s = project("instances-why", "");
    let r = s
        .run(&[
            "why",
            "net.vpc[\"green/vpc\"].cidr",
            "main.df",
            "--set",
            "env=prod",
        ])
        .success();
    assert!(
        r.stdout.contains("├─ instance vpc green\n")
            && r.stdout
                .contains("└─ vpc_net(\"10.2.0.0/16\")   (in green)\n"),
        "{}",
        r.stdout
    );
    assert!(!r.stdout.contains("::"), "{}", r.stdout);
}

/// A name whose `/` comes from a value at run time is refused where the
/// plan assembles addresses (R-73 item 5): `/` separates a copy's scope.
#[test]
fn a_name_with_a_slash_from_a_value_is_an_error() {
    let s = project(
        "instances-slash",
        "part(\"a/b\")\nresource net.vpc \"${p}\" { cidr = \"10.9.0.0/16\" } where part(p)\n",
    );
    let r = s.run(&["plan", "main.df"]).failure();
    assert!(
        r.stderr.contains(
            "net.vpc[\"a/b\"]: its name holds `/` from a value the program computed (\"a/b\")"
        ),
        "{}",
        r.stderr
    );
    // A copy's own resources are scoped, and pass.
    let s = project("instances-slash-ok", "");
    s.run(&["plan", "main.df"]).success();
}
