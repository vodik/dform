//! `why NAME` read in a scope (R-184): a copy's own names, then those of
//! the module instance it was taken from, then the stack's (R-186); a
//! module's; the stack's; and a name nothing in the scope declares, with
//! what reads it. On the mock, a module with a Secret and a component
//! whose copy the stack makes.

mod common;
mod lsp_client;
use common::Scratch;

const BACKUPS: &str = r#"input namespace: k8s.namespace
let ns = namespace.metadata.name
resource k8s.secret repository {
  metadata = { name: "restic", namespace: ns }
}
component volume {
  input name: string
  let tag = "backup-${name}"
  resource k8s.config_map job {
    metadata = { name: tag, namespace: backups.ns }
  }
}
"#;

const APPS: &str = r#"key env: enum("lab", "prod") = "lab"
use k8s
resource k8s.namespace apps { metadata.name = "apps-${env}" }
use backups { namespace = apps }
resource backups.volume forgejo_backup { name = "forgejo" }
"#;

fn project() -> Scratch {
    let s = Scratch::project("why-scope");
    s.write("backups.df", BACKUPS);
    s.write("stacks/apps.df", APPS);
    s
}

/// `dform why NAME apps`'s output.
fn why(s: &Scratch, name: &str) -> String {
    s.run(&["why", name, "apps"]).success().stdout
}

/// The reviewer's shape: the module's Secret read bare in the copy is the
/// module's (R-186), said whose before its chain, however the copy is
/// named.
#[test]
fn a_modules_resource_read_in_a_copy_is_the_modules() {
    let s = project();
    let lead = "repository in volume forgejo_backup: module backups's k8s.secret \
                backups.repository  backups.df:3\nk8s.secret backups.repository  backups.df:3\n";
    for name in [
        "forgejo_backup.repository",
        "volume[\"forgejo_backup\"].repository",
    ] {
        let answer = why(&s, name);
        assert!(answer.starts_with(lead), "{name}\n{answer}");
    }
    // From the stack, bare: the module's, and how to read it.
    assert_eq!(
        why(&s, "repository"),
        "repository: no such name in the stack; module backups's resource is \
         backups.repository (backups.df:3), read it as backups.repository\n"
    );
    assert_eq!(
        why(&s, "forgejo_backup.nothing"),
        "nothing in volume forgejo_backup: no such name in this copy\n"
    );
}

/// A let, an input and a resource of the copy are its own, said as `why`
/// says them; a key is the stack's, read outward from the copy and from
/// the module, with its site.
#[test]
fn a_name_in_a_scope_is_what_the_scope_reads() {
    let s = project();
    assert!(
        why(&s, "forgejo_backup.tag").starts_with("let forgejo_backup.tag = \"backup-forgejo\""),
        "{}",
        why(&s, "forgejo_backup.tag")
    );
    assert!(
        why(&s, "volume[\"forgejo_backup\"].name")
            .starts_with("input forgejo_backup.name = \"forgejo\"  stacks/apps.df:5"),
        "{}",
        why(&s, "volume[\"forgejo_backup\"].name")
    );
    let env = why(&s, "forgejo_backup.env");
    assert!(
        env.starts_with(
            "env in volume forgejo_backup: the stack's key env  stacks/apps.df:1\n\
             key env = \"lab\""
        ),
        "{env}"
    );
    let env = why(&s, "backups.env");
    assert!(
        env.starts_with("env in module backups: the stack's key env  stacks/apps.df:1\n"),
        "{env}"
    );
    let apps = why(&s, "forgejo_backup.apps.metadata.name");
    assert!(
        apps.starts_with(
            "apps in volume forgejo_backup: the stack's k8s.namespace apps  \
             stacks/apps.df:3\nk8s.namespace apps.metadata.name = \"apps-lab\""
        ),
        "{apps}"
    );
}

/// Under two `use`s of the module each copy reads the instance it was
/// taken from: `why` names that instance's.
#[test]
fn a_copy_reads_the_instance_it_was_taken_from() {
    let s = project();
    s.write(
        "stacks/apps.df",
        &format!(
            "{}use backups as b {{ namespace = apps }}\nresource b.volume two {{ name = \"two\" }}\n",
            APPS.replace("use backups {", "use backups as a {")
                .replace("resource backups.volume forgejo_backup", "resource a.volume one")
        ),
    );
    assert!(
        why(&s, "one.repository")
            .starts_with("repository in volume one: module a's k8s.secret a.repository"),
        "{}",
        why(&s, "one.repository")
    );
    assert!(
        why(&s, "two.ns").starts_with("ns in volume two: module b's let b.ns"),
        "{}",
        why(&s, "two.ns")
    );
}

/// The language server's hover on a name read bare in the component's
/// body says what it denotes in each copy, as `why COPY.NAME` does.
#[test]
fn hover_on_a_name_in_a_component_says_what_each_copy_reads() {
    let s = project();
    let root = std::fs::canonicalize(&s.dir).unwrap();
    let file = root.join("backups.df");
    let mut c = lsp_client::Client::start(&root, serde_json::json!({}));
    c.open(&file);
    let hover = c.at(
        "textDocument/hover",
        &file,
        lsp_client::find(&file, "tag, namespace", 1),
    );
    let text = hover["contents"]["value"].as_str().unwrap_or_default();
    assert!(
        text.contains("`tag in volume forgejo_backup: let forgejo_backup.tag  backups.df:8`"),
        "{text}"
    );
    c.shutdown();
}

/// A name the component's own declaration shadows: the hover names both,
/// and how to read the module's (R-186).
#[test]
fn hover_on_a_shadowed_name_names_both() {
    let s = project();
    s.write(
        "backups.df",
        &BACKUPS
            .replace(
                "  let tag = \"backup-${name}\"\n",
                "  let tag = \"backup-${name}\"\n  let ns = \"own\"\n",
            )
            .replace("namespace: backups.ns }", "namespace: ns }"),
    );
    let root = std::fs::canonicalize(&s.dir).unwrap();
    let file = root.join("backups.df");
    let mut c = lsp_client::Client::start(&root, serde_json::json!({}));
    c.open(&file);
    let hover = c.at(
        "textDocument/hover",
        &file,
        lsp_client::find(&file, "tag, namespace: ns", 17),
    );
    let text = hover["contents"]["value"].as_str().unwrap_or_default();
    assert!(
        text.contains(
            "`ns` here is component volume's let ns; it shadows module backups's let ns, read \
             as `super.ns`"
        ),
        "{text}"
    );
    c.shutdown();
}
