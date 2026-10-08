//! A plan whose every change waits on another deployment says what it
//! knows (R-193): the k8s provider configured from a stack not applied
//! yet holds its objects under `later`, and a deny over them is decided
//! where it reads what the program wrote, or prints what it waits on.

mod common;
use common::{Run, Scratch};

const PLATFORM: &str = r#"
key env: enum("lab", "prod") = "lab"
use fake
resource db.postgres server { name = "server" }
output kubeconfig = server.endpoint
"#;

/// The apps stack: the mock's k8s configured from the platform's
/// kubeconfig, two Deployments (one runs as root), and `RULES`.
const APPS: &str = r#"
key env: enum("lab", "prod") = "lab"
use stacks.platform
use k8s { kubeconfig = platform[env].kubeconfig }
resource k8s.namespace apps { metadata.name = "apps" }
resource k8s.deployment web {
  metadata = { name: "web", namespace: apps.metadata.name }
  spec.selector.matchLabels = { app: "web" }
  spec.template.spec.containers = [{ name: "web", image: "nginx" }]
}
resource k8s.deployment api {
  metadata = { name: "api", namespace: apps.metadata.name }
  spec.selector.matchLabels = { app: "api" }
  spec.template.spec = {
    securityContext: { runAsNonRoot: true },
    containers: [{ name: "api", image: "api" }],
  }
}
pod(w, p) where w in k8s.deployment, p = w.spec.template.spec
RULES
"#;

fn project(name: &str, rules: &str) -> Scratch {
    let s = Scratch::project(name);
    s.write("stacks/platform.df", PLATFORM);
    s.write("stacks/apps.df", &APPS.replace("RULES", rules));
    s
}

fn plan(s: &Scratch) -> Run {
    s.run(&["plan", "apps"])
}

/// The private project's deny: a pod spec holding what the server
/// computes (`dnsPolicy`) is no reason to wait, since the deny reads
/// what the program wrote. It holds for `web` and not for `api`, before
/// the platform is applied; it was three bare `deny` lines under `later`.
#[test]
fn a_deny_over_written_values_of_held_objects_is_decided() {
    let s = project(
        "provisional-deny-decided",
        "deny \"pod may run as root\" { workload: w } where {\n  pod(w, p)\n  \
         not p.securityContext.runAsNonRoot == true\n}\n",
    );
    let r = plan(&s).failure();
    let all = format!("{}{}", r.stdout, r.stderr);
    assert!(
        all.contains("- pod may run as root  workload = \"web\"\n")
            && !all.contains("workload = \"api\"")
            && !r.stdout.contains("undetermined")
            && !r.stdout.contains("deny \"pod may run as root\""),
        "{all}"
    );
}

/// A deny that does read what the server computes waits on it, and its
/// line under `later` says so whatever its width: the resources the
/// values are of when the values do not fit beside it.
#[test]
fn a_deny_under_later_always_says_what_it_waits_on() {
    let s = project(
        "provisional-deny-waits",
        "deny \"pod keeps the server's defaults\" { workload: w } where {\n  pod(w, p)\n  \
         { a: p.dnsPolicy, b: p.restartPolicy, c: p.serviceAccountName } == \
         { a: \"x\", b: \"y\", c: \"z\" }\n}\n",
    );
    let r = plan(&s).success();
    let (_, later) = r.stdout.split_once("\nlater\n").expect(&r.stdout);
    for w in ["api", "web"] {
        assert!(
            later.contains(&format!(
                "  deny \"pod keeps the server's defaults\"  stacks/apps.df:20  waits on {w}\n"
            )),
            "{w}\n{}",
            r.stdout
        );
    }
}
