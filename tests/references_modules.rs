//! A reference crosses a module's boundary as a reference (R-204): a
//! relation a copy exports (`blue.p(..)`, `c[t].p(..)`) or a module takes
//! from its user (`input p`) carries its resources as the resources, so
//! the reader reads through them with no `x in T` to type them again; a
//! stack's `output p` cannot carry one out of its deployment.

mod common;
use common::{Scratch, mock};

/// A program under the fake provider.
fn scratch(name: &str, src: &str) -> Scratch {
    let s = Scratch::new(name);
    s.write("p.df", &format!("\nuse fake\n\n{src}"));
    s
}

/// A component that takes subnets from its user and places a machine in
/// each, and a subnet of the stack's.
const STORE: &str = r#"
component store {
  input subnet
  decl subnet(s: net.subnet)
  resource compute.vm vm {
    size = 1
    subnet = s
  } where subnet(s)
}
resource net.vpc v {
  cidr = "10.0.0.0/16"
}
resource net.subnet a {
  vpc = v
  cidr = "10.0.1.0/24"
}
mine(s) where s in net.subnet
"#;

/// The rows a copy's block gives its relation are the user's resources:
/// by a rule over the user's relation, or by the resource's name, and the
/// copy's scope never goes in front of them. The name was "variable `a`
/// shadows the resource `a`", and a row of a reference matched nothing.
#[test]
fn a_components_relation_input_takes_the_users_resources() {
    let s = scratch(
        "refs-mod-input",
        &format!(
            "{STORE}\
             resource store blue {{\n  subnet(s) where mine(s)\n}}\n\
             resource store green {{\n  subnet(a)\n}}\n"
        ),
    );
    let r = mock(&s, &["plan", "--why=none"]).success();
    for copy in ["blue", "green"] {
        assert!(
            r.stdout.contains(&format!(
                "+ compute.vm[\"{copy}.vm\"]\n    size = 1\n    subnet = ?net.subnet[\"a\"]\n"
            )),
            "{copy}: {}",
            r.stdout
        );
    }
}

/// Every copy's relation, `c[t].p(..)`, carries its references too: `s`
/// is the subnet, read through with no `s in net.subnet`.
#[test]
fn every_copys_relation_carries_references() {
    let s = scratch(
        "refs-mod-every",
        "component vnet {\n  input cidr: inet\n  resource net.vpc vpc { cidr }\n  \
         resource net.subnet \"s-${z}\" {\n    vpc\n    cidr = inet.subnet(cidr, 8, i)\n    \
         zone = z\n  } where az(z, i)\n  \
         subnet(s) where s in net.subnet\n  output subnet\n}\n\
         az(\"a\", 0)\naz(\"b\", 1)\n\
         resource vnet blue { cidr = \"10.0.0.0/16\" }\n\
         resource vnet green { cidr = \"10.1.0.0/16\" }\n\
         deny \"zone\" { copy: t, cidr: c } where vnet[t].subnet(s), s.zone == \"b\", c = s.cidr\n",
    );
    let r = mock(&s, &["plan"]).failure();
    for (t, c) in [("blue", "10.0.1.0/24"), ("green", "10.1.1.0/24")] {
        assert!(
            r.stderr
                .contains(&format!("- zone  cidr = \"{c}\", copy = \"{t}\"\n")),
            "{t}: {}",
            r.stderr
        );
    }
}

/// A stack's `output p` whose column holds references is the error at
/// the output, naming the column: another deployment has no such
/// resource to read through or write, where the rows were published as
/// the stack's own addresses.
#[test]
fn a_stacks_relation_output_cannot_carry_references() {
    let s = scratch("refs-mod-stack", &format!("{STORE}\noutput mine\n"));
    let r = mock(&s, &["plan"]).failure();
    assert!(
        r.stderr.contains(
            "`output mine` publishes mine's column `s`, ref(net.subnet): a reference cannot \
             leave a deployment"
        ),
        "{}",
        r.stderr
    );
    assert!(
        r.stderr
            .contains("publish what a reader needs of `s` as a value, such as `s.id`"),
        "{}",
        r.stderr
    );
}
