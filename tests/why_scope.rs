//! `why NAME` read in a scope (R-184): a copy's own names, its module's,
//! private to the module, and its user's read outward; a module's; the
//! stack's; and a name nothing in the scope declares, with what reads it.
//! On the mock, a module with a Secret and a component whose copy the
//! stack makes.

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

/// The reviewer's shape: the module's Secret read bare in the copy is no
/// name of the copy's; the answer is the module's resource and how to
/// read it, however the copy is named.
#[test]
fn a_modules_resource_read_in_a_copy_is_no_name_of_the_copy() {
    let s = project();
    let answer = "repository in volume forgejo_backup: no such name in this copy; the \
                  module's resource is backups.repository (backups.df:3), read it as \
                  backups.repository\n";
    assert_eq!(why(&s, "forgejo_backup.repository"), answer);
    assert_eq!(why(&s, "volume[\"forgejo_backup\"].repository"), answer);
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
             input env = \"lab\""
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
