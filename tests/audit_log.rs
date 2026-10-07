//! The audit log (README "The audit log"): one hash-chained JSON-lines file
//! per deployment beside its state. A full apply, a failed apply and its
//! resume, and a controller event each leave a chain `dform log verify`
//! accepts; an edited entry is named; secrets never appear; a sink gets
//! every entry and its failure is only a warning.

mod common;
use common::{Scratch, mock, repo};

const PROG: &str = r#"

resource net.vpc main { cidr = "10.0.0.0/16" }
resource net.subnet a { vpc_id = ref(net.vpc, "main", "id"), cidr = "10.0.1.0/24" }
resource compute.vm app { subnet_id = ref(net.subnet, "a", "id") }
use fake
"#;

fn entries(s: &Scratch) -> Vec<serde_json::Value> {
    let r = mock(s, &["log", "--json"]).success();
    serde_json::from_str(&r.stdout).unwrap()
}

fn kinds(es: &[serde_json::Value]) -> Vec<String> {
    es.iter()
        .map(|e| e["kind"].as_str().unwrap().to_string())
        .collect()
}

#[test]
fn a_full_apply_is_a_verifiable_chain() {
    let s = Scratch::project("audit-full");
    s.write("p.df", PROG);
    mock(&s, &["apply"]).success();
    let es = entries(&s);
    // Each change of state is logged before anything after it (R-146):
    // the tick's in-flight record before its first call, each call's
    // answer before its action entry, the end's state before `apply_end`.
    assert_eq!(
        kinds(&es),
        [
            "plan",
            "approval",
            "apply_start",
            // The first apply records its master (R-163).
            "master",
            "state",
            "state",
            "action",
            "state",
            "action",
            "state",
            "action",
            "tick",
            "state",
            "apply_end"
        ]
    );
    assert!(es[4]["full"].is_object(), "{}", es[4]);
    assert!(es[5]["changes"].is_array(), "{}", es[5]);
    assert_eq!(es[1]["result"], "not required");
    assert!(es[0]["digest"].as_str().unwrap().starts_with("sha256:"));
    assert_eq!(es[2]["dform"], env!("CARGO_PKG_VERSION"));
    let es: Vec<serde_json::Value> = es
        .into_iter()
        .filter(|e| e["kind"] != "state" && e["kind"] != "master")
        .collect();
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
            ("create", "net.vpc[\"main\"]", "ok"),
            ("create", "net.subnet[\"a\"]", "ok"),
            ("create", "compute.vm[\"app\"]", "ok")
        ]
    );
    assert!(es[3]["remote"].is_string(), "{}", es[3]);
    assert!(es[3]["diff"].as_str().unwrap().starts_with("sha256:"));
    assert!(es[6]["world"].as_str().unwrap().starts_with("hmac-sha256:"));
    assert_eq!(es[7]["result"], "ok");
    // Each entry names the one before.
    for w in entries(&s).windows(2) {
        assert_eq!(w[1]["prev"], w[0]["hash"]);
    }
    let r = mock(&s, &["log", "verify"]).success();
    assert!(
        r.stdout
            .contains("w.state.audit.jsonl: 14 entries, the chain holds"),
        "{}",
        r.stdout
    );
    // The text form, from an entry on.
    let r = mock(&s, &["log", "--since", "12"]).success();
    let lines: Vec<&str> = r.stdout.lines().collect();
    assert_eq!(lines.len(), 3, "{}", r.stdout);
    assert!(
        lines[0].contains(" tick tick=1 world=hmac-sha256:"),
        "{}",
        r.stdout
    );
    assert!(
        lines[1].contains(" state changes=[{\"at\":[\"in_flight\"],\"gone\":true}] fence=0"),
        "{}",
        r.stdout
    );
    assert!(lines[2].ends_with(" apply_end result=ok"), "{}", r.stdout);
}

#[test]
fn a_failed_apply_and_its_resume_are_one_chain() {
    let s = Scratch::project("audit-resume");
    s.write("p.df", PROG);
    mock(&s, &["apply", "--chaos", "fail=compute.vm[\"app\"]"]).failure();
    mock(&s, &["apply"]).success();
    let es = entries(&s);
    let failed: Vec<&serde_json::Value> = es
        .iter()
        .filter(|e| e["kind"] == "action" && e["result"] == "failed")
        .collect();
    assert_eq!(failed.len(), 1, "{es:#?}");
    assert_eq!(failed[0]["address"], "compute.vm[\"app\"]");
    assert!(failed[0]["error"].is_string());
    let ends: Vec<&str> = es
        .iter()
        .filter(|e| e["kind"] == "apply_end")
        .map(|e| e["result"].as_str().unwrap())
        .collect();
    assert_eq!(ends, ["failed", "ok"]);
    // The resume applies the vm.
    let last_action = es.iter().rev().find(|e| e["kind"] == "action").unwrap();
    assert_eq!(last_action["address"], "compute.vm[\"app\"]");
    assert_eq!(last_action["result"], "ok");
    mock(&s, &["log", "verify"]).success();
}

#[test]
fn an_edited_entry_is_named() {
    let s = Scratch::project("audit-edit");
    s.write("p.df", PROG);
    mock(&s, &["apply"]).success();
    let log = s.read("w.state.audit.jsonl");
    // The subnet's action (entry n) claims another remote id.
    let lines: Vec<&str> = log.lines().collect();
    let i = lines
        .iter()
        .position(|l| l.contains("\"kind\":\"action\"") && l.contains("net.subnet"))
        .unwrap();
    let n = i + 1;
    let edited: serde_json::Value = serde_json::from_str(lines[i]).unwrap();
    assert_eq!(edited["address"], "net.subnet[\"a\"]");
    let remote = edited["remote"].as_str().unwrap();
    let forged = lines[i].replace(&format!("\"remote\":\"{remote}\""), "\"remote\":\"forged\"");
    assert_ne!(forged, lines[i]);
    s.write("w.state.audit.jsonl", &log.replace(lines[i], &forged));
    let r = mock(&s, &["log", "verify"]).failure();
    assert!(
        r.stderr.contains(&format!(
            "entry {n} (action) was altered: its hash does not match its content"
        )),
        "{}",
        r.stderr
    );
    // Removing it instead breaks the next one's link.
    let without: Vec<&str> = lines
        .iter()
        .enumerate()
        .filter(|(j, _)| *j != i)
        .map(|(_, l)| *l)
        .collect();
    s.write("w.state.audit.jsonl", &(without.join("\n") + "\n"));
    let r = mock(&s, &["log", "verify"]).failure();
    assert!(
        r.stderr.contains(&format!(
            "entry {n} (state): its prev is not entry {}'s hash",
            n - 1
        )),
        "{}",
        r.stderr
    );
}

#[test]
fn a_controller_event_is_in_the_chain() {
    let s = Scratch::project("audit-controller");
    s.write("p.df", PROG);
    s.run(&["controller", "run", "--once", "p.df"]).success();
    s.run(&["controller", "run", "--once", "p.df"]).success();
    let r = s.run(&["log", "--json", "p.df"]).success();
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
    let r = s.run(&["log", "verify", "p.df"]).success();
    assert!(r.stdout.contains("the chain holds"), "{}", r.stdout);
}

#[test]
fn secrets_never_appear() {
    let s = Scratch::project("audit-secrets");
    s.write(
        "p.df",
        "\n\nresource leaky.vault v {\n  password = \"VAULT-SECRET-DO-NOT-LOG\"\n}\nuse fake\n",
    );
    let schema = repo().join("tests/fixtures/providers/leaky/schema.df");
    let args = ["--provider", schema.to_str().unwrap()];
    mock(&s, &[&args[..], &["apply"]].concat()).success();
    s.write(
        "p.df",
        "\n\nresource leaky.vault v {\n  password = \"ANOTHER-SECRET-DO-NOT-LOG\"\n}\nuse fake\n",
    );
    mock(&s, &[&args[..], &["apply"]].concat()).success();
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
fn a_sink_gets_the_audit_entries_and_its_failure_is_a_warning() {
    let s = Scratch::project("audit-sink");
    s.write("p.df", PROG);
    mock(&s, &["--audit-sink", "cat >> sink.jsonl", "apply"]).success();
    // Every entry but the state's own (`[defaults] audit_sink_entries`).
    let audit: String = s
        .read("w.state.audit.jsonl")
        .lines()
        .filter(|l| !l.contains("\"kind\":\"state\"") && !l.contains("\"kind\":\"lease\""))
        .map(|l| format!("{l}\n"))
        .collect();
    assert_eq!(s.read("sink.jsonl"), audit);
    s.write("p.df", &PROG.replace("10.0.1.0/24", "10.0.2.0/24"));
    let r = mock(&s, &["--audit-sink", "exit 3", "apply"]).success();
    assert!(
        r.stderr
            .contains("warning: audit sink `exit 3`: it exited with exit status: 3; the local log has the entry"),
        "{}",
        r.stderr
    );
    mock(&s, &["log", "verify"]).success();
}

#[test]
fn a_handover_is_logged_where_the_state_goes() {
    let s = Scratch::project("audit-handover");
    s.write("p.df", PROG);
    s.run(&["apply", "p.df"]).success();
    s.run(&["stack", "handover", "p", "--to", "local(\"moved\")"])
        .success();
    let log = s.read("moved/state.audit.jsonl");
    let last: serde_json::Value = serde_json::from_str(log.lines().last().unwrap()).unwrap();
    assert_eq!(last["kind"], "handover");
    assert_eq!(last["to"], "local(\"moved\")");
    let r = s.run(&["log", "verify", "p.df"]).success();
    assert!(r.stdout.contains("moved/state.audit.jsonl"), "{}", r.stdout);
}

#[test]
fn a_rekey_is_logged_where_the_state_goes() {
    let s = Scratch::project("audit-rekey");
    s.write(
        "k.df",
        "\n\
         \n\
         key env: string = \"a\"\n\
         \n\
         use fake\n\
         \n\
         resource net.vpc main {\n\
           cidr = \"10.0.0.0/16\"\n\
         }\n\
         ",
    );
    s.run(&["apply", "k.df", "env=a"]).success();
    s.run(&["stack", "rekey", "k", "env=a", "env=b"]).success();
    let r = s.run(&["log", "--json", "k.df", "env=b"]).success();
    let es: Vec<serde_json::Value> = serde_json::from_str(&r.stdout).unwrap();
    let last = es.last().unwrap();
    assert_eq!(last["kind"], "rekey");
    assert_eq!(
        (&last["from"], &last["to"]),
        (&"k[env=a]".into(), &"k[env=b]".into())
    );
    s.run(&["log", "verify", "k.df", "env=b"]).success();
}
