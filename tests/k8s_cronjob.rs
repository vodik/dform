//! A CronJob's and a Job's pod template (R-182): planned against the real
//! Kubernetes provider offline (the snapshot's schema, where
//! `spec.jobTemplate.spec.template` and a Job's `spec.template` are
//! required) and against the mock, the template written leaf-wise, as
//! its `spec` whole, and as one object with `spec` inside. A template
//! left out is refused by the plan at its site (R-184).

mod common;
use common::{Run, Scratch};

/// `dform plan` in `s` with no cluster in reach: the real provider plans
/// locally against its snapshot.
fn plan(s: &Scratch) -> Run {
    let mut c = common::dform();
    c.args(["plan", "p.df"])
        .current_dir(&s.dir)
        .env_remove("KUBERNETES_SERVICE_HOST")
        .env_remove("KUBERNETES_SERVICE_PORT")
        .env("DFORM_K8S_OFFLINE", "1");
    Run::from(c.output().unwrap())
}

/// A project whose `use k8s` is the real provider, or the mock.
fn project(name: &str, real: bool, body: &str) -> Scratch {
    let s = Scratch::project(name);
    let using = match real {
        true => {
            std::fs::create_dir_all(s.path("providers/k8s")).unwrap();
            std::os::unix::fs::symlink(
                common::exe("dform-provider-k8s"),
                s.path("providers/k8s/dform-provider-k8s"),
            )
            .unwrap();
            "use k8s { source = \"./providers/k8s\" }"
        }
        false => "use k8s",
    };
    s.write("p.df", &format!("\n{using}\n{body}"));
    s
}

const POD: &str =
    "{ restartPolicy: \"OnFailure\", containers: [{ name: \"b\", image: \"busybox\" }] }";

/// The template's three spellings under `at` (`spec.jobTemplate.spec` or
/// `spec`).
fn templates(at: &str) -> [String; 3] {
    [
        format!(
            "  {at}.template.spec.restartPolicy = \"OnFailure\"\n  \
             {at}.template.spec.containers = [{{ name: \"b\", image: \"busybox\" }}]\n"
        ),
        format!("  {at}.template.spec = {POD}\n"),
        format!("  {at}.template = {{ spec: {POD} }}\n"),
    ]
}

#[test]
fn a_cron_jobs_pod_template_plans_however_it_is_written() {
    for real in [true, false] {
        for t in templates("spec.jobTemplate.spec") {
            let s = project(
                "k8s-cronjob",
                real,
                &format!(
                    "resource k8s.cron_job job {{\n  metadata.name = \"backup\"\n  \
                     spec.schedule = \"0 3 * * *\"\n  spec.jobTemplate.spec.backoffLimit = 1\n{t}}}\n"
                ),
            );
            let r = plan(&s);
            assert_eq!(
                r.summary(),
                "plan: 1 change (1 create) over 1 tick",
                "real: {real}\n{t}\n{}\n{}",
                r.stdout,
                r.stderr
            );
            assert!(
                r.stdout.contains("+ k8s.cron_job job") && r.stdout.contains("busybox"),
                "{}",
                r.stdout
            );
        }
    }
}

#[test]
fn a_jobs_pod_template_plans_however_it_is_written() {
    for real in [true, false] {
        for t in templates("spec") {
            let s = project(
                "k8s-job",
                real,
                &format!("resource k8s.job once {{\n  metadata.name = \"once\"\n{t}}}\n"),
            );
            let r = plan(&s);
            assert_eq!(
                r.summary(),
                "plan: 1 change (1 create) over 1 tick",
                "real: {real}\n{t}\n{}\n{}",
                r.stdout,
                r.stderr
            );
        }
    }
}

/// The template left out: the plan refuses the CronJob at its site
/// before the provider is asked (R-184), naming what is unset.
#[test]
fn a_cron_job_without_its_template_is_refused_naming_it() {
    let s = project(
        "k8s-cronjob-none",
        true,
        "resource k8s.cron_job job {\n  metadata.name = \"backup\"\n  \
         spec.schedule = \"0 3 * * *\"\n  spec.jobTemplate.spec.backoffLimit = 1\n}\n",
    );
    let r = plan(&s);
    assert!(
        r.stderr.contains(
            "Error: p.df:3, k8s.cron_job job: spec.jobTemplate.spec.template is unset (required: "
        ),
        "{}",
        r.stderr
    );
}
