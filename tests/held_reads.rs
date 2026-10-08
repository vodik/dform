//! What a resource `later` holds reads as to the rules of other
//! resources (R-183), on the mock and on the real Kubernetes provider
//! offline: the k8s provider configured from another stack's output not
//! applied yet, a module's Secret and a CronJob of its component, the
//! template one literal reading the Secret's name. A written attribute
//! of the held Secret is the program's value; a computed one is carried
//! inside the literal as what it waits on; a read of a Secret nothing
//! derives is an error at its site, and the module's Secret read bare in
//! its component is the module's (R-186).

mod common;
use common::{Run, Scratch};

/// The module: the Secret, and a component whose CronJob reads its name
/// at `READ` inside the template's one literal.
const BACKUPS: &str = r#"
input namespace: k8s.namespace
let ns = namespace.metadata.name
resource k8s.secret repository {
  metadata = { name: "restic", namespace: ns }
}
component volume {
  input name: string
  resource k8s.cron_job job {
    metadata = { name: "backup-${name}", namespace: backups.ns }
    spec.schedule = "0 3 * * *"
    spec.jobTemplate.spec.template.spec = {
      restartPolicy: "Never",
      containers: [{
        name: "restic",
        image: "restic",
        env: [{ name: "TAG", value: name }],
        envFrom: [{ secretRef: { name: READ } }],
      }],
    }
  }
}
"#;

const PLATFORM: &str = r#"
key env: enum("lab", "prod") = "lab"
use fake
resource db.postgres server { name = "server" }
output kubeconfig = server.endpoint
"#;

/// The apps stack, its `use k8s` the real provider or the mock.
fn apps(real: bool) -> String {
    let source = match real {
        true => "source = \"./providers/k8s\", ",
        false => "",
    };
    format!(
        "key env: enum(\"lab\", \"prod\") = \"lab\"\nuse fake\nuse stacks.platform\n\
         use k8s {{ {source}kubeconfig = platform[env].kubeconfig }}\n\
         resource k8s.namespace apps {{ metadata.name = \"apps\" }}\n\
         use backups {{ namespace = apps }}\n\
         resource backups.volume forgejo_backup {{ name = \"forgejo\" }}\n"
    )
}

fn project(name: &str, real: bool, read: &str) -> Scratch {
    let s = Scratch::project(name);
    if real {
        std::fs::create_dir_all(s.path("providers/k8s")).unwrap();
        std::os::unix::fs::symlink(
            common::exe("dform-provider-k8s"),
            s.path("providers/k8s/dform-provider-k8s"),
        )
        .unwrap();
    }
    s.write("backups.df", &BACKUPS.replace("READ", read));
    s.write("stacks/platform.df", PLATFORM);
    s.write("stacks/apps.df", &apps(real));
    s
}

/// `dform ARGS` in `s` with no cluster in reach: the real provider plans
/// locally against its snapshot.
fn run(s: &Scratch, args: &[&str]) -> Run {
    let mut c = common::dform();
    c.args(args)
        .current_dir(&s.dir)
        .env_remove("KUBERNETES_SERVICE_HOST")
        .env_remove("KUBERNETES_SERVICE_PORT")
        .env("DFORM_K8S_OFFLINE", "1");
    Run::from(c.output().unwrap())
}

const TEMPLATE: &str = "forgejo_backup.job.spec.jobTemplate.spec.template.spec";

/// The module's Secret is held with every k8s object; its name, which
/// the program writes, is read into the CronJob's template, which plans
/// under `later` with it.
#[test]
fn a_held_resources_written_attribute_reads_inside_a_literal() {
    for real in [true, false] {
        let s = project(
            "held-reads-written",
            real,
            "backups.repository.metadata.name",
        );
        let r = run(&s, &["plan", "apps"]).success();
        assert_eq!(
            r.summary(),
            "plan: 3 creates after platform[env=lab] is applied",
            "real: {real}\n{}",
            r.stdout
        );
        assert!(
            r.stdout
                .contains("waits on  provider k8s  kubeconfig = platform[env].kubeconfig")
                && r.stdout.contains("+ k8s.cron_job forgejo_backup.job")
                && r.stdout
                    .contains("envFrom: [{ secretRef: { name: \"restic\" } }]"),
            "real: {real}\n{}",
            r.stdout
        );
        let r = run(&s, &["why", TEMPLATE, "apps"]).success();
        assert!(
            r.stdout.contains("secretRef: { name: \"restic\" }")
                && r.stdout.contains("later  waits on  "),
            "real: {real}\n{}",
            r.stdout
        );
    }
}

/// A computed attribute of the held Secret is not known: the template
/// carries it as the leaf it waits on, the provider receives the rest,
/// and `why` names the unknown term.
#[test]
fn a_held_resources_computed_attribute_is_one_unknown_leaf_of_the_literal() {
    for real in [true, false] {
        let s = project(
            "held-reads-computed",
            real,
            "backups.repository.metadata.uid",
        );
        let r = run(&s, &["plan", "apps"]).success();
        assert_eq!(
            r.summary(),
            "plan: 3 creates after platform[env=lab] is applied",
            "real: {real}\n{}",
            r.stdout
        );
        assert!(
            r.stdout
                .contains("envFrom: [{ secretRef: { name: backups.repository.metadata.uid } }]"),
            "real: {real}\n{}",
            r.stdout
        );
        let r = run(&s, &["why", TEMPLATE, "apps"]).success();
        assert!(
            r.stdout
                .contains("secretRef: { name: ?k8s.secret backups.repository.metadata.uid }"),
            "real: {real}\n{}",
            r.stdout
        );
    }
}

/// A read of a Secret nothing derives answers nothing: an error at the
/// read's site naming the attribute it leaves without a value (R-119's
/// form), not the provider's refusal of a template it never received;
/// `why` of the template says the same.
#[test]
fn a_read_of_what_nothing_derives_is_an_error_at_its_site() {
    for real in [true, false] {
        let s = project(
            "held-reads-none",
            real,
            "k8s.secret[\"absent\"].metadata.name",
        );
        let r = run(&s, &["plan", "apps"]).failure();
        let error = "backups.df:12:5, resource backups.volume forgejo_backup: k8s.secret \
                     forgejo_backup.absent.metadata.name answered nothing, so k8s.cron_job \
                     forgejo_backup.job.spec.jobTemplate.spec.template.spec has no value: \
                     nothing derives k8s.secret forgejo_backup.absent";
        assert!(
            r.stderr.contains(&format!("Error: {error}\n")) && !r.stderr.contains("refused"),
            "real: {real}\n{}",
            r.stderr
        );
        let r = run(&s, &["why", TEMPLATE, "apps"]);
        assert!(
            r.stdout.contains(&format!("no value  {error}\n")),
            "real: {real}\n{}\n{}",
            r.stdout,
            r.stderr
        );
    }
}

/// The reviewer's shape (R-186): the module's Secret read bare inside
/// its component's CronJob literal is the module's, of the instance the
/// copy was taken from, as `backups.repository` reads it.
#[test]
fn a_modules_resource_read_bare_in_its_component_is_the_modules() {
    for real in [true, false] {
        let s = project("held-reads-bare", real, "repository.metadata.name");
        let r = run(&s, &["plan", "apps"]).success();
        assert!(
            r.stdout.contains("+ k8s.cron_job forgejo_backup.job")
                && r.stdout
                    .contains("envFrom: [{ secretRef: { name: \"restic\" } }]"),
            "real: {real}\n{}",
            r.stdout
        );
    }
}

/// Inside the module's own file a component reads the module by its own
/// name, `backups.repository`, whatever name the user's `use` gives it
/// (`use backups as b`).
#[test]
fn a_component_reads_its_module_by_its_name_under_an_alias() {
    let s = project(
        "held-reads-alias",
        false,
        "backups.repository.metadata.name",
    );
    let apps = apps(false)
        .replace("use backups {", "use backups as b {")
        .replace("resource backups.volume", "resource b.volume");
    s.write("stacks/apps.df", &apps);
    let r = run(&s, &["plan", "apps"]);
    assert!(
        r.ok && r
            .stdout
            .contains("envFrom: [{ secretRef: { name: \"restic\" } }]"),
        "{}\n{}",
        r.stdout,
        r.stderr
    );
}
