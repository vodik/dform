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

/// The summary counts what `later` holds by kind and by what it waits
/// on, followed to the deployment not applied yet, one clause per wait
/// in `later`'s order, never `0 changes`; the denies waiting on the same
/// say `until then`.
#[test]
fn the_summary_counts_what_later_holds_by_what_it_waits_on() {
    let s = project(
        "provisional-summary",
        "deny \"pod without cluster dns\" { workload: w } where pod(w, p), \
         p.dnsPolicy == \"None\"\n",
    );
    let r = plan(&s).success();
    assert_eq!(
        r.summary(),
        "plan: 3 creates after platform[env=lab] is applied; 2 denies undetermined until then",
        "{}",
        r.stdout
    );
    s.write(
        "stacks/dns.df",
        "key env: enum(\"lab\", \"prod\") = \"lab\"\nuse fake\n\
         resource net.vpc zone { cidr = \"10.0.0.0/16\" }\noutput zone = zone.id\n",
    );
    let apps = s.read("stacks/apps.df");
    s.write(
        "stacks/apps.df",
        &format!(
            "{apps}use fake\nuse stacks.dns\nresource db.postgres records {{ name = dns[env].zone }}\n"
        ),
    );
    let r = plan(&s).success();
    assert_eq!(
        r.summary(),
        "plan: 3 creates after platform[env=lab] is applied; 1 create after dns[env=lab] is \
         applied; 2 denies undetermined until platform[env=lab] is applied",
        "{}",
        r.stdout
    );
}

/// `why DENY` agrees with the plan for a deny that waits: undetermined,
/// on what, and the comparison over the value not known yet is not
/// "false".
#[test]
fn why_a_deny_that_waits_says_what_it_waits_on() {
    let s = project(
        "provisional-why-deny",
        "deny \"pod without cluster dns\" { workload: w } where pod(w, p), \
         p.dnsPolicy == \"None\"\n",
    );
    let r = s
        .run(&["why", "deny \"pod without cluster dns\"", "apps"])
        .success();
    assert!(
        r.stdout.starts_with(
            "deny \"pod without cluster dns\": undetermined, waits on \
             api.spec.template.spec.dnsPolicy, web.spec.template.spec.dnsPolicy\n"
        ) && r.stdout.contains(" == \"None\": not known yet\n")
            && !r.stdout.contains("false"),
        "{}",
        r.stdout
    );
}

const MIDDLEWARE: &str = "resource k8s.traefik.middleware mw {\n  metadata = { name: \"mw\", \
                          namespace: apps.metadata.name }\n  spec.headers.stsSeconds = 3\n}\n";

const PROVISIONAL: &str = "  waits on  provider k8s  kubeconfig = platform[env].kubeconfig\n  \
     provisional: planned against the offline schema; planned again once kubeconfig is known\n";

/// The k8s provider held by its connection alone (the mock flags
/// `kubeconfig` so, as the real one does) planned its objects against
/// its offline schema: the group says it is provisional, each create
/// shows its document; a kind no schema has (a CRD's) still waits on the
/// provider's schema, and is no provisional plan; the plan file and
/// `--json` carry the mark.
#[test]
fn a_provider_held_by_its_connection_plans_provisionally() {
    let s = project("provisional-group", MIDDLEWARE);
    let r = plan(&s).success();
    assert_eq!(
        r.summary(),
        "plan: 4 creates after platform[env=lab] is applied",
        "{}",
        r.stdout
    );
    let (_, later) = r.stdout.split_once("\nlater\n").expect(&r.stdout);
    let (provisional, schema) = later
        .split_once("  waits on  provider k8s  schema\n")
        .expect(later);
    assert!(
        provisional.starts_with(PROVISIONAL)
            && provisional.contains(
                "  + k8s.deployment web         stacks/apps.df:6\n      \
                 metadata = { name: \"web\", namespace: \"apps\" }\n"
            ),
        "{}",
        r.stdout
    );
    assert!(
        schema.starts_with("  + k8s.traefik.middleware mw") && !schema.contains("provisional"),
        "{}",
        r.stdout
    );
    s.run(&["plan", "apps", "--out", "p.json"]).success();
    let marked = |j: &serde_json::Value| -> Vec<(String, bool)> {
        j.as_array()
            .unwrap()
            .iter()
            .map(|e| {
                let name = e["name"].as_str().or(e["on"][0]["null"].as_str());
                let mark = e["provisional"].as_bool().unwrap_or(false);
                (name.unwrap_or_default().to_string(), mark)
            })
            .collect()
    };
    assert_eq!(
        marked(&s.json("p.json")["deformations"]),
        [
            ("apps".to_string(), true),
            ("api".to_string(), true),
            ("web".to_string(), true),
            ("mw".to_string(), false),
        ]
    );
    let j: serde_json::Value =
        serde_json::from_str(&s.run(&["plan", "apps", "--json"]).success().stdout).unwrap();
    let held: Vec<bool> = j["later"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|l| l["kind"] == "held")
        .map(|l| l["provisional"].as_bool().unwrap())
        .collect();
    assert_eq!(held, [true, false], "{j:#}");
}

/// A provider that also waits on a setting that says what it makes (the
/// default namespace) is no provisional plan: its objects wait as ever.
#[test]
fn a_provider_held_by_more_than_its_connection_is_not_provisional() {
    let s = project("provisional-namespace", "");
    s.write(
        "stacks/platform.df",
        &format!("{PLATFORM}output namespace = server.endpoint\n"),
    );
    let apps = s.read("stacks/apps.df").replace(
        "kubeconfig = platform[env].kubeconfig }",
        "kubeconfig = platform[env].kubeconfig, namespace = platform[env].namespace }",
    );
    s.write("stacks/apps.df", &apps);
    let r = plan(&s).success();
    assert!(
        r.stdout
            .contains("\nlater\n  waits on  provider k8s  kubeconfig = ")
            && !r.stdout.contains("provisional"),
        "{}",
        r.stdout
    );
}

/// The real provider, offline, plans against its snapshot (R-110): the
/// same provisional group, a Traefik Middleware still waiting on the
/// schema its cluster will serve.
#[test]
fn the_k8s_provider_plans_provisionally_against_its_snapshot() {
    let s = project("provisional-k8s", MIDDLEWARE);
    std::fs::create_dir_all(s.path("providers/k8s")).unwrap();
    std::os::unix::fs::symlink(
        common::exe("dform-provider-k8s"),
        s.path("providers/k8s/dform-provider-k8s"),
    )
    .unwrap();
    let apps = s.read("stacks/apps.df").replace(
        "use k8s { kubeconfig",
        "use k8s { source = \"./providers/k8s\", kubeconfig",
    );
    s.write("stacks/apps.df", &apps);
    let mut c = common::dform();
    c.args(["plan", "apps"])
        .current_dir(&s.dir)
        .env_remove("KUBERNETES_SERVICE_HOST")
        .env_remove("KUBERNETES_SERVICE_PORT")
        .env("DFORM_K8S_OFFLINE", "1");
    let r = Run::from(c.output().unwrap()).success();
    let (_, later) = r.stdout.split_once("\nlater\n").expect(&r.stdout);
    assert!(
        later.starts_with(PROVISIONAL)
            && later.contains("      spec.selector.matchLabels.app = \"web\"\n")
            && later.contains("  waits on  provider k8s  schema\n  + k8s.traefik.middleware mw"),
        "{}",
        r.stdout
    );
}

/// A tick whose provider's connection an earlier tick makes is planned
/// against the offline schema too, and says so as `later` does.
#[test]
fn a_tick_whose_connection_an_earlier_tick_makes_is_provisional() {
    let s = Scratch::project("provisional-tick");
    s.write(
        "p.df",
        "use fake\nresource db.postgres server { name = \"server\" }\n\
         use k8s { kubeconfig = server.endpoint }\n\
         resource k8s.namespace apps { metadata.name = \"apps\" }\n",
    );
    let r = s.run(&["plan", "p.df"]).success();
    assert!(
        r.stdout.contains(
            "tick 2  1 change\n  waits on  provider k8s  kubeconfig = server.endpoint\n  \
             provisional: planned against the offline schema; planned again once kubeconfig \
             is known\n  + k8s.namespace apps"
        ),
        "{}",
        r.stdout
    );
    let r = s.run(&["plan", "p.df", "--json"]).success();
    let j: serde_json::Value = serde_json::from_str(&r.stdout).unwrap();
    let ticks = j["ticks"].as_array().unwrap();
    assert_eq!(ticks[0].get("provisional"), None, "{}", r.stdout);
    assert_eq!(ticks[1]["provisional"], true, "{}", r.stdout);
}
