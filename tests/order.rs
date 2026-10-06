//! Statement order is irrelevant (R-100): every declaration of a file and
//! of the modules it uses is collected before any read is resolved, so a
//! read may come before what it names, as `dform fmt` writes a file
//! (outputs first, R-11a).

mod common;
use common::Scratch;

/// A module named like its component, as a k3s cluster's is written.
const K3S: &str = r#"
component k3s {
  input name: string
  output kubeconfig: string = "kubeconfig of ${name}"
  output ip: string = server.cidr
  resource net.vpc server { cidr = "10.0.0.0/16" }
}
"#;

fn outputs(s: &Scratch) -> String {
    s.run(&["query", "attr(\"output\", \"\", k, v)", "main.df"])
        .success()
        .stdout
}

/// The header reads a copy's outputs; the `use` and the `instance` come
/// last. The copy's component is named like its module, whose name the
/// `use` binds: `k3s` reads the module, `k3s.k3s` its component, and the
/// stack's outputs are named like the copy's.
#[test]
fn outputs_first_then_the_instance() {
    let s = Scratch::project("order-outputs");
    s.write("k3s.df", K3S);
    s.write(
        "main.df",
        r#"input env: string = "lab"
output kubeconfig: string = cluster.kubeconfig
output ip: string = cluster.ip
ips(n, i) where i = k3s.k3s[n].ip

use fake

use k3s

resource k3s.k3s cluster { name = "k8s-${env}" }
"#,
    );
    s.run(&["plan", "main.df"]).success();
    let out = outputs(&s);
    assert!(
        out.contains("\"kubeconfig\"  \"kubeconfig of k8s-lab\""),
        "{out}"
    );
    assert!(
        out.contains("\"ip\"          net.vpc cluster.server.cidr"),
        "{out}"
    );
    let ips = s.run(&["query", "ips(n, i)", "main.df"]).success().stdout;
    assert!(
        ips.contains("\"cluster\"  net.vpc cluster.server.cidr"),
        "{ips}"
    );
}

/// A `let` reads a resource declared below it, and an output reads a used
/// module's item above the `use`.
#[test]
fn a_read_comes_before_what_it_names() {
    let s = Scratch::project("order-reads");
    s.write(
        "config.df",
        "input region: string = \"r1\"\nlet zone = \"${region}-a\"\n",
    );
    s.write(
        "main.df",
        r#"output zone = config.zone
output early = vpc_cidr
let vpc_cidr = later.cidr
use fake
resource net.vpc later { cidr = "10.1.0.0/16" }
use config
"#,
    );
    let out = outputs(&s);
    assert!(out.contains("\"zone\"   \"r1-a\""), "{out}");
    assert!(out.contains("\"early\"  net.vpc later.cidr"), "{out}");
}

/// A copy of a module rather than of a component is the one error, at the
/// `instance`, naming the component's path; a read of the copy says no
/// more.
#[test]
fn an_instance_of_a_module_names_its_components() {
    let s = Scratch::project("order-module");
    s.write("k3s.df", K3S);
    s.write(
        "main.df",
        "output ip: string = cluster.ip\nuse fake\nuse k3s\nresource k3s cluster { name = \"a\" }\n",
    );
    let r = s.run(&["plan", "main.df"]).failure();
    assert!(
        r.stderr.contains("k3s is a module; `use` it"),
        "{}",
        r.stderr
    );
    assert!(
        r.stderr
            .contains("its components are instanced by their path: `k3s.k3s`"),
        "{}",
        r.stderr
    );
    assert!(!r.stderr.contains("unknown name"), "{}", r.stderr);
    assert!(r.stderr.contains("1 error"), "{}", r.stderr);
}
