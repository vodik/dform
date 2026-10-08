//! A resource that leaves unset an attribute its schema requires is
//! refused by the plan at the resource's site, before its provider is
//! asked (R-184), and `why` of the resource lists it: a CronJob of a
//! module's component with its pod template left out, on the real
//! Kubernetes provider offline (the snapshot's schema, where
//! `spec.jobTemplate.spec.template` is required where
//! `spec.jobTemplate.spec` is written) and on the mock (whose CronJob
//! requires the template's containers).

mod common;
use common::{Run, Scratch};

/// The module: a component whose CronJob writes its job's spec but not
/// its pod template.
const BACKUPS: &str = r#"input namespace: k8s.namespace
let ns = namespace.metadata.name
component volume {
  input name: string
  resource k8s.cron_job job {
    metadata = { name: "backup-${name}", namespace: backups.ns }
    spec.schedule = "0 3 * * *"
    spec.jobTemplate.spec.backoffLimit = 1
  }
}
"#;

fn project(name: &str, real: bool) -> Scratch {
    let s = Scratch::project(name);
    let source = match real {
        true => {
            std::fs::create_dir_all(s.path("providers/k8s")).unwrap();
            std::os::unix::fs::symlink(
                common::exe("dform-provider-k8s"),
                s.path("providers/k8s/dform-provider-k8s"),
            )
            .unwrap();
            " { source = \"./providers/k8s\" }"
        }
        false => "",
    };
    s.write("backups.df", BACKUPS);
    s.write(
        "stacks/apps.df",
        &format!(
            "use k8s{source}\n\
             resource k8s.namespace apps {{ metadata.name = \"apps\" }}\n\
             use backups {{ namespace = apps }}\n\
             resource backups.volume forgejo_backup {{ name = \"forgejo\" }}\n"
        ),
    );
    s
}

/// `dform ARGS` in `s` with no cluster in reach.
fn run(s: &Scratch, args: &[&str]) -> Run {
    let mut c = common::dform();
    c.args(args)
        .current_dir(&s.dir)
        .env_remove("KUBERNETES_SERVICE_HOST")
        .env_remove("KUBERNETES_SERVICE_PORT")
        .env("DFORM_K8S_OFFLINE", "1");
    Run::from(c.output().unwrap())
}

#[test]
fn a_required_attribute_left_unset_is_an_error_at_the_resources_site() {
    for (real, unset) in [
        (
            true,
            "spec.jobTemplate.spec.template is unset (required: describes the pod that \
             will be created when executing a job)",
        ),
        (
            false,
            "spec.jobTemplate.spec.template.spec.containers is unset (required)",
        ),
    ] {
        let s = project("why-required", real);
        let r = run(&s, &["plan", "apps"]).failure();
        assert!(
            r.stderr.contains(&format!(
                "Error: backups.df:5, k8s.cron_job forgejo_backup.job: {unset}\n"
            )) && !r.stderr.contains("refused"),
            "real: {real}\n{}",
            r.stderr
        );
        let r = run(&s, &["why", "forgejo_backup.job", "apps"]).success();
        let path = unset.split_once(" is unset").unwrap().0;
        let doc = unset
            .split_once("(required")
            .unwrap()
            .1
            .trim_start_matches(": ")
            .trim_end_matches(')');
        let line = match doc.is_empty() {
            true => format!("  {path}\n"),
            false => format!("  {path}  ({doc})\n"),
        };
        assert!(
            r.stdout
                .starts_with("k8s.cron_job forgejo_backup.job  backups.df:5\n")
                && r.stdout
                    .contains(&format!("\nunset, required by the schema:\n{line}")),
            "real: {real}\n{}",
            r.stdout
        );
    }
}

/// Written, the template is no longer listed and the CronJob plans.
#[test]
fn a_required_attribute_written_is_not_listed() {
    let s = project("why-required-set", true);
    s.write(
        "backups.df",
        &BACKUPS.replace(
            "    spec.jobTemplate.spec.backoffLimit = 1\n",
            "    spec.jobTemplate.spec.backoffLimit = 1\n    \
             spec.jobTemplate.spec.template.spec = { restartPolicy: \"Never\", \
             containers: [{ name: \"restic\", image: \"restic\" }] }\n",
        ),
    );
    let r = run(&s, &["plan", "apps"]).success();
    assert!(
        r.stdout.contains("+ k8s.cron_job forgejo_backup.job"),
        "{}",
        r.stdout
    );
    let r = run(&s, &["why", "forgejo_backup.job", "apps"]).success();
    assert!(!r.stdout.contains("unset"), "{}", r.stdout);
}

/// `dform test` fails the combination the plan would refuse, at the
/// resource's site with the plan's message, rather than passing a stack
/// whose every CronJob a plan refuses.
#[test]
fn dform_test_fails_what_the_plan_refuses() {
    for real in [true, false] {
        let s = project("why-required-test", real);
        let r = run(&s, &["test", "apps"]);
        assert!(
            r.stdout.contains("error  dform plan apps\n")
                && r.stdout.contains(
                    "  Error: backups.df:5, k8s.cron_job forgejo_backup.job: spec.jobTemplate.spec.template"
                )
                && r.stdout.contains("test apps: 1 combination, 1 failed"),
            "real: {real}\n{}\n{}",
            r.stdout,
            r.stderr
        );
    }
}
