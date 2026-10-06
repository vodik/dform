//! An input typed by a resource type (`input namespace: k8s.namespace`)
//! takes a reference, `namespace = apps`, and binds from it as from any
//! value (R-120): on a component's copy, inside a stack and outside one,
//! and on a module's `use`. A type need not declare an `id` for its
//! reference to be given.

mod common;
use common::Scratch;

/// A component whose input is a namespace, reading the namespace's name.
const DB: &str = r#"
component pg {
  input namespace: k8s.namespace
  input name: string
  output host = "${name}.${namespace.metadata.name}.svc"
  resource k8s.secret creds {
    metadata = { name: "${name}-creds", namespace: namespace.metadata.name }
  }
}
"#;

const USER: &str = r#"
use k8s
resource k8s.namespace apps { metadata.name = "apps" }
resource databases.pg one { namespace = apps, name = "one" }
resource k8s.config_map cm {
  metadata = { name: "cm", namespace: apps.metadata.name }
  data = { host: one.host }
}
"#;

fn copies(r: &common::Run) {
    assert!(
        r.stdout.contains(
            "  + databases.pg one\n    + k8s.secret one.creds  databases.df:6\n\
             \x20       metadata.name = \"one-creds\"\n\
             \x20       metadata.namespace = \"apps\"\n"
        ) && r.stdout.contains("      data.host = \"one.apps.svc\"\n"),
        "{}",
        r.stdout
    );
}

/// A stack's copy binds its reference-typed input, and what reads the
/// copy's output plans with it.
#[test]
fn a_copy_in_a_stack_binds_a_reference() {
    let s = Scratch::project("copy-ref-stack");
    s.write("databases.df", DB);
    s.write("stacks/apps.df", &format!("use databases\n{USER}"));
    copies(&s.run(&["plan", "apps"]).success());
}

/// Outside a stack, the same.
#[test]
fn a_copy_outside_a_stack_binds_a_reference() {
    let s = Scratch::project("copy-ref-file");
    s.write("databases.df", DB);
    s.write("main.df", USER);
    copies(&s.run(&["plan", "main.df"]).success());
}

/// A module's input given a reference by its `use`.
#[test]
fn a_module_input_binds_a_reference() {
    let s = Scratch::project("copy-ref-use");
    s.write(
        "tenant.df",
        "input namespace: k8s.namespace\n\
         resource k8s.config_map cfg { metadata = { name: \"cfg\", namespace: namespace.metadata.name } }\n",
    );
    s.write(
        "main.df",
        "use k8s\nresource k8s.namespace apps { metadata.name = \"apps\" }\nuse tenant { namespace = apps }\n",
    );
    let r = s.run(&["plan", "main.df"]).success();
    assert!(
        r.stdout.contains(
            "  + k8s.config_map tenant.cfg  tenant.df:2\n\
             \x20     metadata.name = \"cfg\"\n\
             \x20     metadata.namespace = \"apps\"\n"
        ),
        "{}",
        r.stdout
    );
}
