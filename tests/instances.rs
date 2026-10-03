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
