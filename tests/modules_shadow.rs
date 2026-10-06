//! A module's or a component's own declarations win a read in its body
//! over what its user's scope brings in (R-101), and a read through an
//! input typed `ref(T)` reads the referenced resource's attribute.

mod common;
use common::Scratch;

/// traefik.df's own `resource net.vpc traefik` is what `traefik.cidr`
/// reads inside it, though the stack's `use traefik` names the module
/// there too; in the stack, the same two names are R-76's ambiguity.
#[test]
fn a_modules_own_resource_wins_over_its_users_use() {
    let s = Scratch::project("modules-shadow");
    s.write(
        "traefik.df",
        r#"

resource net.vpc traefik { cidr = "10.0.0.0/16" }
resource net.subnet web { cidr = traefik.cidr }
"#,
    );
    s.write("stacks/app.df", "\n\nuse fake\n\nuse traefik\n");
    let r = s.run(&["plan", "--why=none", "app"]).success();
    assert!(
        r.stdout
            .contains("+ net.subnet[\"traefik.web\"]\n  cidr = \"10.0.0.0/16\"\n"),
        "{}",
        r.stdout
    );
    // The stack's own scope keeps the error: there both are its names.
    s.write(
        "stacks/app.df",
        "\n\nuse fake\n\nuse traefik\nresource net.vpc traefik { cidr = \"10.9.0.0/16\" }\n\
         resource net.subnet s { cidr = traefik.cidr }\n",
    );
    let r = s.run(&["plan", "--why=none", "app"]).failure();
    assert!(
        r.stderr
            .contains("`traefik` names the module and the resource net.vpc[\"traefik\"]"),
        "{}",
        r.stderr
    );
}

/// A component's own value wins over a resource of its user's of the name.
#[test]
fn a_components_own_value_wins_over_its_users_resource() {
    let s = Scratch::new("modules-shadow-value");
    s.write(
        "p.df",
        r#"
component c {
  input cfg: string
  resource net.vpc v { cidr = cfg }
}
resource net.vpc cfg { cidr = "10.1.0.0/16" }
instance c a { cfg = "10.2.0.0/16" }
use fake
"#,
    );
    let r = s
        .run(&["dev", "--world", "w.json", "plan", "--why=none", "p.df"])
        .success();
    assert!(
        r.stdout
            .contains("+ net.vpc[\"a.v\"]\n    cidr = \"10.2.0.0/16\"\n"),
        "{}",
        r.stdout
    );
}

/// `vpc.cidr` on `input vpc: ref(net.vpc)` reads the referenced vpc's
/// attribute, in the copy's resources and outputs, and in a copy inside a
/// copy, where the reference is its user's resource.
#[test]
fn a_ref_input_reads_through_the_reference() {
    let s = Scratch::new("modules-shadow-ref");
    s.write(
        "p.df",
        r#"
component sub {
  input vpc: ref(net.vpc)
  resource net.subnet s { cidr = vpc.cidr }
  output c: string = vpc.cidr
}
component edge {
  resource net.vpc main { cidr = "10.1.0.0/16" }
  instance sub inner { vpc = main }
  output c: string = inner.c
}
resource net.vpc main { cidr = "10.0.0.0/16" }
instance sub a { vpc = main }
instance edge e
resource net.subnet s { cidr = e.c }
use fake
"#,
    );
    let r = s
        .run(&["dev", "--world", "w.json", "plan", "--why=none", "p.df"])
        .success();
    for want in [
        "+ net.subnet[\"a.s\"]\n    cidr = \"10.0.0.0/16\"\n",
        "+ net.subnet[\"e.inner.s\"]\n      cidr = \"10.1.0.0/16\"\n",
        "+ net.subnet[\"s\"]\n  cidr = \"10.1.0.0/16\"\n",
    ] {
        assert!(r.stdout.contains(want), "{want}\n---\n{}", r.stdout);
    }
}
