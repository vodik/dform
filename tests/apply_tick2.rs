//! Tick 2 of a provider configured from what tick 1 makes (R-45): the
//! plan schedules that provider's resources in tick 2, waiting on its
//! settings, their attributes as written (R-156); apply makes tick 1,
//! configures the provider at the boundary (waiting, within the
//! provider's `timeout`, while the value is not there yet) and applies
//! what tick 2 showed, asked for once before tick 1 (R-122). The plan
//! file holds them too.
//!
//! The server is the mock's `db.postgres` (the mock run as a plugin, a
//! second provider process), the settings its endpoint behind an
//! `env.var`, the provider configured from them the mock's `k8s`. What
//! the apply decides is read back as what it said (`common::brief`).

mod common;
use common::{Run, STOPPED, Scratch, brief};

const PROG: &str = r#"
use env
use fake { source = "prov" }
resource db.postgres server { name = "server" }
let kc = str.format("%s@%s", env.var("R45_KUBECONFIG"), server.endpoint)
use k8s { kubeconfig = kc }
resource k8s.namespace ns { metadata.name = "app" }
"#;

fn scratch(name: &str) -> Scratch {
    let s = Scratch::new(name);
    s.write("p.df", PROG);
    std::fs::create_dir_all(s.path("prov")).unwrap();
    std::os::unix::fs::symlink(
        common::exe("dform-provider-fake"),
        s.path("prov/dform-provider-fake"),
    )
    .unwrap();
    s
}

fn dform(s: &Scratch, args: &[&str]) -> std::process::Command {
    let mut c = common::dform();
    common::saying(&mut c, s)
        .args(args)
        .env("R45_KUBECONFIG", "kc")
        .env("DFORM_WAIT_POLL_MS", "50")
        .env("NO_COLOR", "1")
        .current_dir(&s.dir);
    c
}

/// `dform dev --world w.json ARGS` (ARGS from the verb on).
fn dev(s: &Scratch, args: &[&str]) -> Run {
    let mut all = vec!["dev", "--world", "w.json"];
    all.extend_from_slice(args);
    Run::from(dform(s, &all).output().unwrap())
}

fn audit(s: &Scratch, kind: &str) -> Vec<serde_json::Value> {
    s.read("w.state.audit.jsonl")
        .lines()
        .map(|l| serde_json::from_str::<serde_json::Value>(l).unwrap())
        .filter(|e| e["kind"] == kind)
        .collect()
}

/// Answering each prompt in turn: what it printed up to each prompt and
/// after the last, and its exit code.
fn answers(s: &Scratch, answers: &[&str]) -> (Vec<String>, i32) {
    let cmd = dform(s, &["dev", "--world", "w.json", "apply", "p.df"]);
    common::answering(&s.dir, cmd, answers)
}

/// The plan schedules the namespace in tick 2, after the server its
/// provider's settings are made from (R-156): one question for both
/// ticks; the boundary configures `k8s` and tick 2 applies what was
/// shown, not asked again.
#[test]
fn apply_asks_once_for_what_waits_on_the_provider() {
    let s = scratch("tick2-asks");
    let (said, code) = answers(&s, &["y"]);
    assert_eq!(code, 0, "{said:?}");
    let events = s.said();
    assert_eq!(
        events.iter().map(brief).collect::<Vec<_>>(),
        [
            "plan 1",
            "asked plan",
            "answered yes",
            "configured k8s 1",
            "plan 2"
        ],
        "{said:?}"
    );
    assert_eq!(events[1]["changes"], 2);
    // A setting is said by its key at the default level: its value may
    // be a secret.
    assert_eq!(
        events[3]["settings"],
        serde_json::json!(["kubeconfig = (sensitive)"])
    );
    assert!(s.read("w.json").contains("k8s.namespace"));
}

/// `n` to the one question: nothing is made, neither tick.
#[test]
fn declining_makes_neither_tick() {
    let s = scratch("tick2-declined");
    let (said, code) = answers(&s, &["n"]);
    assert_eq!(code, 3, "a decline exits 3 (R-147): {said:?}");
    assert!(!s.path("w.json").exists() || !s.read("w.json").contains("k8s.namespace"));
}

/// `--yes` applies both ticks: the addresses were in the printed plan.
#[test]
fn yes_applies_tick_two() {
    let s = scratch("tick2-yes");
    dev(&s, &["apply", "--yes", "p.df"]).success();
    assert_eq!(
        s.said().iter().map(brief).collect::<Vec<_>>(),
        ["plan 1", "configured k8s 1", "plan 2"]
    );
    let c = audit(&s, "configure");
    assert_eq!(c.len(), 1, "{c:?}");
    assert_eq!(
        c[0],
        serde_json::json!({
            "kind": "configure", "tick": 1, "provider": "k8s", "settings": ["kubeconfig"],
            "hash": c[0]["hash"], "prev": c[0]["prev"], "seq": c[0]["seq"], "time": c[0]["time"],
        })
    );
}

/// A plan file shows tick 2's namespace, its attributes as written
/// (R-156): applying the file makes both ticks.
#[test]
fn a_plan_file_applies_the_tick_it_showed() {
    let s = scratch("tick2-plan-file");
    let p = dev(&s, &["plan", "--out", "plan.json", "p.df"]).success();
    assert!(p.stdout.contains("tick 2  1 change"), "{}", p.stdout);
    let r = Run::from(dform(&s, &["apply", "plan.json"]).output().unwrap()).success();
    assert!(!r.stderr.contains(STOPPED), "{}", r.stderr);
    assert!(s.read("w.fakecloud.json").contains("db.postgres"));
    assert!(s.read("w.json").contains("k8s.namespace"));
}

/// The server's endpoint not there yet after tick 1 (chaos `not-ready`, a
/// host still booting): the tick waits on it, then configures the
/// provider and applies what waited on it. The tick that only waits
/// prints its header, which says what it waits on; a wait is not a
/// difference from the plan shown.
#[test]
fn the_boundary_waits_for_the_settings_then_configures() {
    let s = scratch("tick2-waits");
    let r = dev(
        &s,
        &[
            "--chaos",
            "not-ready=db.postgres[\"server\"].endpoint:3",
            "apply",
            "--yes",
            "p.df",
        ],
    )
    .success();
    // Tick 2 only waits: the report gives it no section of its own.
    let events = s.said();
    assert_eq!(
        events.iter().map(brief).collect::<Vec<_>>(),
        ["plan 1", "plan 2", "configured k8s 2", "plan 3"],
        "{}",
        r.stdout
    );
    assert_eq!(
        events[1]["waits"],
        serde_json::json!(["db.postgres server.endpoint"])
    );
    assert!(
        events[3]["text"]
            .as_str()
            .unwrap()
            .contains("  + k8s.namespace ns"),
        "{}",
        r.stdout
    );
    let w = audit(&s, "wait");
    assert_eq!(w.len(), 1, "{w:?}");
    assert_eq!(w[0]["result"], "resolved");
}

/// A kind the provider serves only once the program configures it (a
/// cluster's CRD; the mock's `schemas` setting plays the cache the k8s
/// provider writes at that Configure) is learned by dform at the boundary:
/// tick 2 plans it with its schema, so another object reads its computed
/// `metadata.uid` once it exists, as the next run would. Without it, the
/// apply failed: "does not set metadata.uid".
#[test]
fn a_kind_served_after_the_boundary_is_planned_with_its_schema() {
    let s = scratch("tick2-learns-schema");
    s.write(
        "crd.df",
        "type_provider(k8s.example.io.v1.token, \"k8s\")\n\
         type_attr(k8s.example.io.v1.token, \"metadata.name\", \"string\", [\"id\"])\n\
         type_attr(k8s.example.io.v1.token, \"metadata.namespace\", \"string\", [])\n\
         type_attr(k8s.example.io.v1.token, \"metadata.uid\", \"string\", [\"computed\", \"id\"])\n\
         type_mint(k8s.example.io.v1.token, \"metadata.uid\", \"uid-{name}\")\n\
         type_attr(k8s.example.io.v1.token, \"spec.value\", \"string\", [\"sensitive\"])\n",
    );
    s.write(
        "p.df",
        &format!(
            "{}resource k8s.example.io.v1.token t {{\n  metadata.name = \"t\"\n  \
             metadata.namespace = ns.metadata.name\n  spec.value = \"TOKEN-VALUE\"\n}}\n\
             resource k8s.config_map c {{\n  metadata.name = \"c\"\n  \
             metadata.namespace = ns.metadata.name\n  data = {{ uid: t.metadata.uid }}\n}}\n",
            PROG.replace(
                "use k8s { kubeconfig = kc }",
                "use k8s { kubeconfig = kc, schemas = [\"crd.df\"] }"
            )
        ),
    );
    // Tick 2's plan, as the boundary planned it with the schema it
    // learned, is printed again (After R-206).
    let r = dev(&s, &["apply", "--yes", "p.df"]).success();
    let events = s.said();
    assert_eq!(
        events.iter().map(brief).collect::<Vec<_>>(),
        ["plan 1", "configured k8s 1", "plan 2"],
        "{}",
        r.stdout
    );
    let tick2 = events[2]["text"].as_str().unwrap();
    assert!(
        tick2.contains("  + k8s.example.io.v1.token t  ")
            && tick2.contains("      spec.value = (sensitive)\n"),
        "{tick2}"
    );
    assert!(!tick2.contains("TOKEN-VALUE"), "{tick2}");
    let w: serde_json::Value = serde_json::from_str(&s.read("w.json")).unwrap();
    assert_eq!(
        w["resources"]["k8s.config_map::c"]["attrs"]["data"]["uid"], "uid-t",
        "{w}"
    );
}
