//! The audit log (README "The audit log"): one hash-chained JSON-lines file
//! per deployment beside its state. A full apply, a failed apply and its
//! resume, and a controller event each leave a chain `dform log verify`
//! accepts; an edited entry is named; secrets never appear; a sink gets
//! every entry and its failure is only a warning.

mod common;
use common::{Scratch, repo};

const PROG: &str = r#"edition 2026

resource net.vpc main { cidr = "10.0.0.0/16" }
resource net.subnet a { vpc_id = ref(net.vpc, "main", "id"), cidr = "10.0.1.0/24" }
resource compute.vm app { subnet_id = ref(net.subnet, "a", "id") }
"#;

fn dform(s: &Scratch, args: &[&str]) -> common::Run {
    s.run(&[&["--file", "p.df", "--world", "w.json"][..], args].concat())
}

fn entries(s: &Scratch) -> Vec<serde_json::Value> {
    let r = dform(s, &["log", "--json"]).success();
    serde_json::from_str(&r.stdout).unwrap()
}

fn kinds(es: &[serde_json::Value]) -> Vec<String> {
    es.iter()
        .map(|e| e["kind"].as_str().unwrap().to_string())
        .collect()
}

#[test]
fn a_full_apply_is_a_verifiable_chain() {
    let s = Scratch::new("audit-full");
    s.write("p.df", PROG);
    dform(&s, &["apply"]).success();
    let es = entries(&s);
    assert_eq!(
        kinds(&es),
        [
            "plan",
            "approval",
            "apply_start",
            "action",
            "action",
            "action",
            "tick",
            "apply_end"
        ]
    );
    assert_eq!(es[1]["result"], "not required");
    assert!(es[0]["digest"].as_str().unwrap().starts_with("sha256:"));
    assert_eq!(es[2]["dform"], env!("CARGO_PKG_VERSION"));
    let actions: Vec<(&str, &str, &str)> = es[3..6]
        .iter()
        .map(|e| {
            (
                e["action"].as_str().unwrap(),
                e["address"].as_str().unwrap(),
                e["result"].as_str().unwrap(),
            )
        })
        .collect();
    assert_eq!(
        actions,
        [
            ("create", "net.vpc.main", "ok"),
            ("create", "net.subnet.a", "ok"),
            ("create", "compute.vm.app", "ok")
        ]
    );
    assert!(es[3]["remote"].is_string(), "{}", es[3]);
    assert!(es[3]["diff"].as_str().unwrap().starts_with("sha256:"));
    assert!(es[6]["world"].as_str().unwrap().starts_with("hmac-sha256:"));
    assert_eq!(es[7]["result"], "ok");
    // Each entry names the one before.
    for w in es.windows(2) {
        assert_eq!(w[1]["prev"], w[0]["hash"]);
    }
    let r = dform(&s, &["log", "verify"]).success();
    assert!(
        r.stdout
            .contains("w.state.audit.jsonl: 8 entries, the chain holds"),
        "{}",
        r.stdout
    );
    // The text form, from an entry on.
    let r = dform(&s, &["log", "--since", "7"]).success();
    let lines: Vec<&str> = r.stdout.lines().collect();
    assert_eq!(lines.len(), 2, "{}", r.stdout);
    assert!(
        lines[0].contains(" tick tick=1 world=hmac-sha256:"),
        "{}",
        r.stdout
    );
    assert!(lines[1].ends_with(" apply_end result=ok"), "{}", r.stdout);
}

#[test]
fn a_failed_apply_and_its_resume_are_one_chain() {
    let s = Scratch::new("audit-resume");
    s.write("p.df", PROG);
    dform(&s, &["apply", "--chaos", "fail=compute.vm/app"]).failure();
    dform(&s, &["apply"]).success();
    let es = entries(&s);
    let failed: Vec<&serde_json::Value> = es
        .iter()
        .filter(|e| e["kind"] == "action" && e["result"] == "failed")
        .collect();
    assert_eq!(failed.len(), 1, "{es:#?}");
    assert_eq!(failed[0]["address"], "compute.vm.app");
    assert!(failed[0]["error"].is_string());
    let ends: Vec<&str> = es
        .iter()
        .filter(|e| e["kind"] == "apply_end")
        .map(|e| e["result"].as_str().unwrap())
        .collect();
    assert_eq!(ends, ["failed", "ok"]);
    // The resume applies the vm.
    let last_action = es.iter().rev().find(|e| e["kind"] == "action").unwrap();
    assert_eq!(last_action["address"], "compute.vm.app");
    assert_eq!(last_action["result"], "ok");
    dform(&s, &["log", "verify"]).success();
}

#[test]
fn an_edited_entry_is_named() {
    let s = Scratch::new("audit-edit");
    s.write("p.df", PROG);
    dform(&s, &["apply"]).success();
    let log = s.read("w.state.audit.jsonl");
    // The subnet's action (entry 5) claims another remote id.
    let lines: Vec<&str> = log.lines().collect();
    let edited: serde_json::Value = serde_json::from_str(lines[4]).unwrap();
    assert_eq!(edited["address"], "net.subnet.a");
    let remote = edited["remote"].as_str().unwrap();
    let forged = lines[4].replace(&format!("\"remote\":\"{remote}\""), "\"remote\":\"forged\"");
    assert_ne!(forged, lines[4]);
    s.write("w.state.audit.jsonl", &log.replace(lines[4], &forged));
    let r = dform(&s, &["log", "verify"]).failure();
    assert!(
        r.stderr
            .contains("entry 5 (action) was altered: its hash does not match its content"),
        "{}",
        r.stderr
    );
    // Removing it instead breaks the next one's link.
    let without: Vec<&str> = lines
        .iter()
        .enumerate()
        .filter(|(i, _)| *i != 4)
        .map(|(_, l)| *l)
        .collect();
    s.write("w.state.audit.jsonl", &(without.join("\n") + "\n"));
    let r = dform(&s, &["log", "verify"]).failure();
    assert!(
        r.stderr
            .contains("entry 5 (action): its prev is not entry 4's hash"),
        "{}",
        r.stderr
    );
}

#[test]
fn a_controller_event_is_in_the_chain() {
    let s = Scratch::new("audit-controller");
    s.write("p.df", PROG);
    s.run(&["--file", "p.df", "controller", "--once"]).success();
    s.run(&["--file", "p.df", "controller", "--once"]).success();
    let r = s.run(&["--file", "p.df", "log", "--json"]).success();
    let es: Vec<serde_json::Value> = serde_json::from_str(&r.stdout).unwrap();
    let events: Vec<&str> = es
        .iter()
        .filter(|e| e["kind"] == "controller")
        .filter_map(|e| e["event"].as_str())
        .collect();
    assert_eq!(events, ["event start", "event resync"]);
    let k = kinds(&es);
    assert_eq!(k[0], "controller");
    assert!(k.contains(&"action".to_string()), "{k:?}");
    let r = s.run(&["--file", "p.df", "log", "verify"]).success();
    assert!(r.stdout.contains("the chain holds"), "{}", r.stdout);
}

#[test]
fn secrets_never_appear() {
    let s = Scratch::new("audit-secrets");
    s.write(
        "p.df",
        "edition 2026\n\nresource leaky.vault v {\n  password = \"VAULT-SECRET-DO-NOT-LOG\"\n}\n",
    );
    let schema = repo().join("tests/fixtures/providers/leaky/schema.df");
    let args = ["--provider", schema.to_str().unwrap()];
    dform(&s, &[&args[..], &["apply"]].concat()).success();
    s.write(
        "p.df",
        "edition 2026\n\nresource leaky.vault v {\n  password = \"ANOTHER-SECRET-DO-NOT-LOG\"\n}\n",
    );
    dform(&s, &[&args[..], &["apply"]].concat()).success();
    let log = s.read("w.state.audit.jsonl");
    assert!(!log.contains("SECRET-DO-NOT-LOG"), "{log}");
    // The two updates' diffs differ: the secret's digest is keyed.
    let es = entries(&s);
    let diffs: Vec<&str> = es
        .iter()
        .filter(|e| e["kind"] == "action")
        .map(|e| e["diff"].as_str().unwrap())
        .collect();
    assert_eq!(diffs.len(), 2);
    assert_ne!(diffs[0], diffs[1]);
}

#[test]
fn a_sink_gets_every_entry_and_its_failure_is_a_warning() {
    let s = Scratch::new("audit-sink");
    s.write("p.df", PROG);
    dform(&s, &["--audit-sink", "cat >> sink.jsonl", "apply"]).success();
    assert_eq!(s.read("sink.jsonl"), s.read("w.state.audit.jsonl"));
    s.write("p.df", &PROG.replace("10.0.1.0/24", "10.0.2.0/24"));
    let r = dform(&s, &["--audit-sink", "exit 3", "apply"]).success();
    assert!(
        r.stderr
            .contains("warning: audit sink `exit 3`: it exited with exit status: 3; the local log has the entry"),
        "{}",
        r.stderr
    );
    dform(&s, &["log", "verify"]).success();
}

#[test]
fn a_handover_is_logged_where_the_state_goes() {
    let s = Scratch::new("audit-handover");
    s.write("p.df", PROG);
    s.run(&["--file", "p.df", "apply"]).success();
    s.run(&[
        "--file",
        "p.df",
        "stack",
        "handover",
        "p",
        "--to",
        "local(\"moved\")",
    ])
    .success();
    let log = s.read("moved/state.audit.jsonl");
    let last: serde_json::Value = serde_json::from_str(log.lines().last().unwrap()).unwrap();
    assert_eq!(last["kind"], "handover");
    assert_eq!(last["to"], "local(\"moved\")");
    let r = s.run(&["--file", "p.df", "log", "verify"]).success();
    assert!(r.stdout.contains("moved/state.audit.jsonl"), "{}", r.stdout);
}

#[test]
fn a_rekey_is_logged_where_the_state_goes() {
    let s = Scratch::new("audit-rekey");
    s.write(
        "k.df",
        "edition 2026\n\nstack k[env] {\n  isolated = true\n}\n\ninput env: string = \"a\"\n\n\
         resource net.vpc main {\n  cidr = \"10.0.0.0/16\"\n}\n",
    );
    s.run(&["--file", "k.df", "apply"]).success();
    s.run(&["--file", "k.df", "stack", "rekey", "k", "env=a", "env=b"])
        .success();
    let r = s
        .run(&["--file", "k.df", "--set", "env=b", "log", "--json"])
        .success();
    let es: Vec<serde_json::Value> = serde_json::from_str(&r.stdout).unwrap();
    let last = es.last().unwrap();
    assert_eq!(last["kind"], "rekey");
    assert_eq!(
        (&last["from"], &last["to"]),
        (&"k[env=a]".into(), &"k[env=b]".into())
    );
    s.run(&["--file", "k.df", "--set", "env=b", "log", "verify"])
        .success();
}
