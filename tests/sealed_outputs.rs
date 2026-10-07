//! A secret output no provider holds reaches the stacks that read it
//! (R-166), ~/src/ovh-infra's shape: `platform` reads a kubeconfig off its
//! server (a location's read: bytes, held by no provider) and outputs it;
//! `apps` configures its k8s provider from `platform[env].kubeconfig`.
//! The stack is the unit of custody: the producer's apply seals the value
//! to each deployment of the project that reads it, to the public key its
//! master publishes; the reader opens it with its own master. No stack
//! reads another's master, the plan prints the grant, and the audit logs
//! record the seal and the read.

mod common;
use common::{Run, Scratch, dform, yes};

const PASS: (&str, &str) = ("DFORM_TEST_PASSPHRASE", "sealed outputs");
const KUBECONFIG: &str = "apiVersion: v1 # KUBECONFIG-SECRET-LAB";

fn run(s: &Scratch, env: &[(&str, &str)], args: &[&str]) -> Run {
    let mut c = dform();
    c.args(yes(args))
        .current_dir(&s.dir)
        .env_remove("RANDOM_MASTER")
        .env_remove("DFORM_TEST_PASSPHRASE");
    for (k, v) in env {
        c.env(k, v);
    }
    Run::from(c.output().unwrap())
}

fn project(name: &str) -> Scratch {
    let s = Scratch::project(name);
    s.write(
        "dform.toml",
        "[project]\nedition = \"2026\"\n\n[providers]\nfake = \"fake\"\nk8s = \"k8s\"\n\n\
         [secrets]\npassphrase = \"env:DFORM_TEST_PASSPHRASE\"\n",
    );
    s.write(
        "providers/fake/schema.df",
        &(std::fs::read_to_string(common::repo().join("crates/dform-mock/schemas/fake.df"))
            .unwrap()
            + "type_provider(db.secret, \"fakecloud\")\n\
               type_attr(db.secret, \"password\", \"string\", [\"sensitive\"])\n"),
    );
    s.write(
        "stacks/platform.df",
        r#"key env: enum("lab", "prod") = "lab"
use fake
resource net.vpc main { cidr_block = "10.0.0.0/16" }
# The token a derived secret: the platform holds it as its database's.
resource db.secret token { password = random.password("token") }
let raw: secret(string) = io.read("kubeconfig-${env}.yaml")
output kubeconfig: secret(string) = raw
output token: secret(string) = random.password("token")
"#,
    );
    s.write(
        "stacks/apps.df",
        r#"key env: enum("lab", "prod") = "lab"
use stacks.platform
use k8s { kubeconfig = platform[env].kubeconfig }
resource k8s.namespace apps { metadata.name = "apps" }
resource k8s.secret token {
  metadata = { name: "token", namespace: "apps" }
  stringData = { token: platform[env].token }
}
"#,
    );
    // A stack that reads nothing of the platform's.
    s.write(
        "stacks/other.df",
        "use fake\nresource net.vpc other { cidr_block = \"10.1.0.0/16\" }\n",
    );
    s.write("kubeconfig-lab.yaml", KUBECONFIG);
    s
}

fn audit(s: &Scratch, deployment: &str, kind: &str) -> Vec<serde_json::Value> {
    s.read(&format!("dform.state/{deployment}/state.audit.jsonl"))
        .lines()
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .filter(|e| e["kind"] == kind)
        .collect()
}

fn files(dir: &std::path::Path, out: &mut Vec<(String, String)>) {
    for e in std::fs::read_dir(dir).unwrap() {
        let p = e.unwrap().path();
        if p.is_dir() {
            files(&p, out);
        } else {
            let text = String::from_utf8_lossy(&std::fs::read(&p).unwrap()).into_owned();
            out.push((p.display().to_string(), text));
        }
    }
}

#[test]
fn a_location_read_secret_output_configures_the_reading_stacks_provider() {
    let s = project("sealed-outputs");
    run(&s, &[PASS], &["apply", "other"]).success();
    // The first apply of the reader: the platform is applied first and
    // seals to no one yet; the reader says so, and is one from now.
    let r = run(&s, &[PASS], &["apply", "apps", "env=lab"]).failure();
    assert!(
        r.stderr.contains(
            "apps[env=lab]: platform[env=lab].kubeconfig is held by no provider and not sealed \
             to it yet: apply platform[env=lab] again"
        ),
        "{}",
        r.stderr
    );
    // The platform's plan prints the grant.
    let r = run(&s, &[PASS], &["plan", "platform", "env=lab"]).success();
    assert!(
        r.stdout.contains(
            "output kubeconfig  sealed to apps[env=lab]\noutput token  sealed to apps[env=lab]\n"
        ),
        "{}",
        r.stdout
    );
    // The next applies the platform (which seals) and then the reader,
    // whose k8s provider is configured from the opened kubeconfig.
    let r = run(&s, &[PASS], &["apply", "apps", "env=lab"]).success();
    assert!(r.stdout.contains("+ k8s.namespace apps"), "{}", r.stdout);
    let world = s.read("dform.state/apps/env=lab/remote.json");
    assert!(world.contains("\"apps\""), "{world}");
    // A derived secret crosses the same way: the reader's Secret holds
    // the platform's token, which neither stack's state holds.
    let w: serde_json::Value = serde_json::from_str(&world).unwrap();
    let pw = s.json("dform.state/platform/env=lab/remote.json")["resources"]["db.secret::token"]
        ["attrs"]["password"]
        .as_str()
        .unwrap()
        .to_string();
    assert_eq!(
        w["resources"]["k8s.secret::token"]["attrs"]["stringData"]["token"], pw,
        "{w}"
    );
    // Sealed to its reader only: not to `other`, which reads nothing of
    // it, nor to a deployment of `apps` that is not one.
    let published = s.json("dform.state/platform/env=lab/outputs.json");
    let sealed = published["secret"]["kubeconfig"]["sealed"]
        .as_object()
        .unwrap();
    assert_eq!(
        sealed.keys().collect::<Vec<_>>(),
        ["apps[env=lab]"],
        "{published}"
    );
    // The seal and the read are in the logs.
    let grants = audit(&s, "platform/env=lab", "sealed");
    assert_eq!(
        grants.last().unwrap()["outputs"]["kubeconfig"],
        serde_json::json!(["apps[env=lab]"]),
        "{grants:?}"
    );
    let reads = audit(&s, "apps/env=lab", "opened");
    assert!(
        reads.last().is_some_and(|e| e["outputs"]
            == serde_json::json!(["platform[env=lab].kubeconfig", "platform[env=lab].token"])),
        "{reads:?}"
    );
    // Neither value is in any file dform keeps (the mock's own worlds
    // aside, the provider's storage).
    let mut all = Vec::new();
    files(&s.path("dform.state"), &mut all);
    for (path, text) in &all {
        assert!(!text.contains("KUBECONFIG-SECRET"), "{path}");
        if !path.ends_with("remote.json") {
            assert!(!text.contains(&pw), "{path}");
        }
    }
    let r = run(&s, &[PASS], &["plan", "apps", "env=lab"]).success();
    assert_eq!(r.summary(), "stack apps is up to date", "{}", r.stdout);
    // Without the master the reader cannot open it, and says so.
    let r = run(&s, &[], &["plan", "apps", "env=lab"]).success();
    assert!(
        r.stderr
            .contains("platform[env=lab].kubeconfig is sealed to it: opening it needs its master"),
        "{}",
        r.stderr
    );
}
