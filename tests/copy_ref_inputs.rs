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
             \x20       metadata = { name: \"one-creds\", namespace: \"apps\" }\n"
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
        "use k8s\nnames(2, \"b\")\nresource k8s.namespace apps { metadata.name = \"apps\" }\nuse tenant { namespace = apps }\n",
    );
    let r = s.run(&["plan", "main.df"]).success();
    assert!(
        r.stdout.contains(
            "  + k8s.config_map tenant.cfg  tenant.df:2\n\
             \x20     metadata = { name: \"cfg\", namespace: \"apps\" }\n"
        ),
        "{}",
        r.stdout
    );
}

/// A statement whose own clause holds and that derives nothing is listed
/// under `not planned` with why, never silently absent: here the copy's
/// input reads a row nobody states, and what reads its output follows. A
/// statement held back by its own clause is quiet. A reference to one of
/// them is an error at the read (R-194).
#[test]
fn a_statement_that_derives_nothing_is_listed_with_why() {
    let s = Scratch::project("copy-ref-not-planned");
    s.write("databases.df", DB);
    s.write(
        "main.df",
        "use k8s\nnames(2, \"b\")\nresource k8s.namespace apps { metadata.name = \"apps\" }\n\
         resource databases.pg one { namespace = apps, name = names[1] }\n\
         resource k8s.config_map cm { metadata = { name: \"cm\", namespace: \"apps\" }, \
         data = { host: one.host } }\n\
         resource k8s.config_map quiet { metadata = { name: \"q\", namespace: \"apps\" } } \
         where names[1] == \"x\"\n",
    );
    let r = s.run(&["plan", "main.df"]).success();
    assert_eq!(
        r.summary(),
        "plan: 1 change (1 create) over 1 tick, 2 not planned",
        "{}",
        r.stdout
    );
    assert!(
        r.stdout.contains(
            "not planned\n  \
             k8s.config_map cm     main.df:5  output one.host is not set\n  \
             k8s.secret one.creds  databases.df:6  input one.name is not set\n"
        ),
        "{}",
        r.stdout
    );
    assert!(!r.stdout.contains("quiet"), "{}", r.stdout);

    // What reads a statement that derives nothing is refused by naming it.
    s.write(
        "main.df",
        "use k8s\nnames(2, \"b\")\nresource k8s.secret ghost { metadata = { name: \"g-${names[1]}\", namespace: \"a\" } }\n\
         resource k8s.config_map cm { metadata = { name: \"cm\", namespace: \"a\" }, \
         data = { x: ghost.metadata.name } }\n",
    );
    let r = s.run(&["plan", "main.df"]).failure();
    assert!(
        r.stderr.starts_with(
            "error  k8s.secret ghost.metadata.name answered nothing, so \
             k8s.config_map cm.data.x has no value: nothing derives k8s.secret ghost\n  \
             main.df:4  resource k8s.config_map cm"
        ),
        "{}",
        r.stderr
    );
}

/// `why` explains a copy's output by the name the program reads it by.
#[test]
fn why_explains_a_copys_output() {
    let s = Scratch::project("copy-ref-why");
    s.write("databases.df", DB);
    s.write("main.df", USER);
    let r = s.run(&["why", "--tree", "one.host", "main.df"]).success();
    assert!(
        r.stdout
            .starts_with("output one.host = \"one.apps.svc\"\n  merged from 1 contribution\n")
            && r.stdout.contains("├─ input one.name = \"one\"\n"),
        "{}",
        r.stdout
    );
}
