//! A provider configured from a secret the program computes once another
//! provider's object exists (R-45): ~/src/ovh-infra's k3s kubeconfig, read
//! over SSH from the server tick 1 creates. Here the server is the mock's
//! `db.postgres` (the mock run as a plugin: a second provider process),
//! the secret an `env.var` joined with its endpoint, which only the
//! object's creation makes known, and the provider configured from it the
//! mock's `k8s`. Tick 1 creates the server, the boundary configures `k8s`
//! with the bytes, tick 2 plans and applies what `later` held: the
//! stable kinds and a kind only the cluster has (a CRD), which waited on
//! the provider for its schema and goes to it once it is configured.
//!
//! The bytes reach the provider (its `expect_account` holds only with
//! them) and nothing else: no file under the project (state, its audit
//! log, the worlds, the cache, a plan file), no line of output.

mod common;
use common::{Run, Scratch};
use std::path::Path;

const STACK: &str = r#"
use env
use fake { source = "prov" }
resource db.postgres server { name = "server" }
let kc = format("%s@%s", env.var("R45_KUBECONFIG"), server.endpoint)
use k8s { kubeconfig = kc, account = kc, expect_account = kc }
resource k8s.namespace ns { metadata.name = "app" }
resource k8s.config_map conf {
  metadata.name = "conf"
  metadata.namespace = ns.metadata.name
}
resource k8s.traefik.io.v1alpha1.middleware strip {
  metadata.name = "strip"
  spec.stripPrefix.prefixes = ["/a"]
}
"#;

fn project() -> Scratch {
    let s = Scratch::project("secret-provider-config");
    s.write("stacks/p.df", STACK);
    std::fs::create_dir_all(s.path("prov")).unwrap();
    std::os::unix::fs::symlink(
        common::exe("dform-provider-fake"),
        s.path("prov/dform-provider-fake"),
    )
    .unwrap();
    s
}

/// A value no program, fixture or minted id contains.
fn token() -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    format!("KC{}x{nanos}SECRET", std::process::id())
}

fn dform(s: &Scratch, token: &str, args: &[&str]) -> Run {
    let out = common::dform()
        .args(common::yes(args))
        .current_dir(&s.dir)
        .env("R45_KUBECONFIG", token)
        .output()
        .unwrap();
    Run::from(out)
}

/// Every file under `dir` whose bytes contain `needle`.
fn holding(dir: &Path, needle: &[u8], out: &mut Vec<String>) {
    for e in std::fs::read_dir(dir).unwrap() {
        let p = e.unwrap().path();
        if p.is_symlink() {
            continue;
        }
        if p.is_dir() {
            holding(&p, needle, out);
        } else if std::fs::read(&p)
            .unwrap()
            .windows(needle.len())
            .any(|w| w == needle)
        {
            out.push(p.display().to_string());
        }
    }
}

#[test]
fn a_provider_configured_from_a_secret_applies_at_tick_two_and_the_bytes_stay_in_memory() {
    let s = project();
    let tok = token();
    let mut runs = Vec::new();

    let plan = dform(&s, &tok, &["plan", "p", "--out", "plan1.json"]).success();
    assert_eq!(
        plan.summary(),
        "plan: 1 change (1 create) over 1 tick, 3 later",
        "{}",
        plan.stdout
    );
    assert!(
        plan.stdout.contains(
            "  waits on  provider k8s (account from kc, kubeconfig from kc)  \
             which this plan does not resolve"
        ),
        "{}",
        plan.stdout
    );
    assert!(
        plan.stdout
            .contains("  waits on  provider k8s for its schema"),
        "{}",
        plan.stdout
    );
    runs.push(plan);

    let apply = dform(&s, &tok, &["apply", "p", "-v"]).success();
    let out = &apply.stdout;
    let (tick1, tick2) = out
        .split_once("provider k8s: configured after tick 1: ")
        .unwrap_or_else(|| panic!("{out}"));
    assert!(tick1.contains("  + db.postgres server"), "{out}");
    assert!(!tick1.contains("tick 2"), "{out}");
    // Each secret setting `(sensitive)`; `-v` adds what it is written as.
    assert!(
        tick2.starts_with("account = (sensitive) from kc, kubeconfig = (sensitive) from kc\n"),
        "{out}"
    );
    for line in [
        "tick 2  3 changes, now that tick 1 reported",
        "  + k8s.namespace ns",
        "  + k8s.config_map conf",
        "  + k8s.traefik.io.v1alpha1.middleware strip",
        "apply: complete",
    ] {
        assert!(tick2.contains(line), "{line}\n{out}");
    }
    // The audit log says the provider was configured, by its settings'
    // keys.
    let audit = s.read("dform.state/p/state.audit.jsonl");
    let configured: Vec<serde_json::Value> = audit
        .lines()
        .map(|l| serde_json::from_str::<serde_json::Value>(l).unwrap())
        .filter(|e| e["kind"] == "configure")
        .collect();
    assert_eq!(configured.len(), 1, "{audit}");
    assert_eq!(configured[0]["provider"], "k8s");
    assert_eq!(configured[0]["tick"], 1);
    assert_eq!(
        configured[0]["settings"],
        serde_json::json!(["account", "kubeconfig"])
    );
    runs.push(apply);

    // Configured at plan time now (the server exists): nothing to do, and
    // a plan file of it.
    let again = dform(&s, &tok, &["plan", "p", "--out", "plan2.json"]).success();
    assert!(again.stdout.contains("is up to date"), "{}", again.stdout);
    runs.push(again);
    runs.push(dform(&s, &tok, &["apply", "p", "-vv"]).success());

    // The provider had the bytes: its account is the secret the program
    // expects, else every plan above refused.
    let mut found = Vec::new();
    holding(&s.dir, tok.as_bytes(), &mut found);
    assert!(found.is_empty(), "the secret is in {found:?}");
    for r in &runs {
        assert!(!r.stdout.contains(&tok), "{}", r.stdout);
        assert!(!r.stderr.contains(&tok), "{}", r.stderr);
    }
}
