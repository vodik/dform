//! `plan --json` and `query --json`: one stable JSON document each, the
//! thing CI and editors consume.

mod common;
use common::{Scratch, repo};
use serde_json::{Value, json};

fn gke_json(s: &Scratch) -> Value {
    let prog = repo().join("examples/gke/stacks/gke_two_phase.df");
    let r = s
        .run(&[
            "dev",
            "--provider",
            "gke",
            "--world",
            "w.json",
            "plan",
            "--json",
            prog.to_str().unwrap(),
        ])
        .success();
    serde_json::from_str(&r.stdout).unwrap_or_else(|e| panic!("{e}: {}", r.stdout))
}

#[test]
fn plan_json_has_every_section() {
    let s = Scratch::new("json-gke");
    let p = gke_json(&s);
    assert_eq!(p["stack"], "gke_two_phase");
    assert_eq!(p["undeformed"], false);
    assert_eq!(p["summary"]["deformations"], 3);
    assert_eq!(p["summary"]["create"], 3);
    assert_eq!(p["summary"]["pending"], 4);
    assert_eq!(p["summary"]["undetermined"], 1);
    assert_eq!(p["definite"].as_array().unwrap().len(), 3);
    let subnet = &p["definite"][0];
    assert_eq!(subnet["action"], "create");
    assert_eq!(
        subnet["address"],
        "google.compute_subnetwork[\"gke_subnet\"]"
    );
    assert_eq!(subnet["type"], "google.compute_subnetwork");
    assert_eq!(
        subnet["changes"][0],
        json!({"op": "set", "path": "ip_cidr_range", "before": null, "after": "10.141.76.0/22"})
    );
    let block = &p["pending"][0];
    assert_eq!(
        block["on"][0],
        json!({"null": "google.container_cluster[\"pngu\"].ca_certificate", "class": "open"})
    );
    assert_eq!(block["resolves_after"], 1);
    let secret = block["deformations"]
        .as_array()
        .unwrap()
        .iter()
        .find(|d| d["name"] == "db_credentials")
        .unwrap();
    let password = secret["changes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["path"] == "data.password")
        .unwrap();
    assert_eq!(
        password["after"],
        json!({"sensitive": "google.secret_manager_secret_version[\"db_pw\"].secret_data"})
    );
    assert_eq!(p["pending_groups"][0]["pattern"], "google.container_node_pool[?]");
    assert_eq!(p["undetermined"][0]["kind"], "undetermined");
    assert_eq!(p["undetermined"][0]["after"], 1);
    assert_eq!(p["apply_order"][1]["tick"], 2);
    assert_eq!(p["shadowed"], json!([]));
    assert_eq!(p["conflicts"], json!([]));
}

/// Nulls carry their class: a fresh id.
#[test]
fn plan_json_nulls_carry_their_class() {
    let s = Scratch::new("json-null");
    let p = gke_json(&s);
    let addr = p["definite"]
        .as_array()
        .unwrap()
        .iter()
        .find(|d| d["name"] == "static_ip")
        .unwrap();
    let sub = addr["changes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["path"] == "subnetwork_id")
        .unwrap();
    assert_eq!(
        sub["after"],
        json!({"null": "google.compute_subnetwork[\"gke_subnet\"]", "class": "fresh"})
    );
}

#[test]
fn an_undeformed_stack_is_a_document_too() {
    let s = Scratch::new("json-undeformed");
    s.write(
        "p.df",
        "edition 2026\nresource net.vpc main { cidr = \"10.0.0.0/16\" }\nprovider fake\n",
    );
    s.run(&common::on("p.df", &["--world", "w.json"], &["apply"]))
        .success();
    let r = s
        .run(&common::on(
            "p.df",
            &["--world", "w.json"],
            &["plan", "--json"],
        ))
        .success();
    let p: Value = serde_json::from_str(&r.stdout).unwrap();
    assert_eq!(p["undeformed"], true);
    assert_eq!(p["summary"]["deformations"], 0);
}

#[test]
fn query_json_lists_the_facts() {
    let s = Scratch::new("json-query");
    s.write(
        "p.df",
        "edition 2026\nresource net.vpc main { cidr = \"10.0.0.0/16\" }\nprovider fake\n",
    );
    let r = s
        .run(&[
            "dev", "--world", "w.json", "query", "want", "--json", "p.df",
        ])
        .success();
    let q: Value = serde_json::from_str(&r.stdout).unwrap();
    assert_eq!(q["query"], "want");
    assert_eq!(q["count"], 1);
    assert_eq!(
        q["facts"][0],
        json!({"pred": "want", "args": ["net.vpc", "main"]})
    );
}

/// `query --json` spells a secret and a null the way `plan --json` does.
#[test]
fn query_json_redacts_like_the_plan() {
    let s = Scratch::new("json-query-secret");
    s.write(
        "p.df",
        "edition 2026\nresource leaky.vault v { password = \"VAULT-SECRET-DO-NOT-PRINT\" }\nresource net.subnet a { vpc_id = ref(net.vpc, \"main\", \"id\") }\nresource net.vpc main { cidr = \"10.0.0.0/16\" }\nprovider fake\n",
    );
    let leaky = repo().join("tests/fixtures/providers/leaky/schema.df");
    let r = s
        .run(&[
            "dev",
            "--provider",
            "fake",
            "--provider",
            leaky.to_str().unwrap(),
            "--world",
            "w.json",
            "query",
            "attr(T, A, P, V), P = \"password\"",
            "--json",
            "p.df",
        ])
        .success();
    assert!(!r.stdout.contains("VAULT-SECRET"), "{}", r.stdout);
    let q: Value = serde_json::from_str(&r.stdout).unwrap();
    assert_eq!(
        q["rows"][0]["V"],
        json!({"sensitive": "leaky.vault[\"v\"].password"})
    );
    let r = s
        .run(&[
            "dev",
            "--provider",
            "fake",
            "--provider",
            leaky.to_str().unwrap(),
            "--world",
            "w.json",
            "query",
            "attr(net.subnet, \"a\", \"vpc_id\", V)",
            "--json",
            "p.df",
        ])
        .success();
    let q: Value = serde_json::from_str(&r.stdout).unwrap();
    assert_eq!(
        q["rows"][0]["V"],
        json!({"null": "net.vpc[\"main\"]", "class": "fresh"})
    );
}

/// The executor's plan entries in JSON: a replace with its order, the
/// prevent_destroy deny, and a moved rename.
#[test]
fn replace_denied_and_moved_are_in_the_document() {
    let s = Scratch::new("json-replace");
    let net = "edition 2026\nresource net.vpc main { cidr = \"10.0.0.0/16\" }\nprovider fake\n";
    s.write("p.df", net);
    s.run(&common::on("p.df", &["--world", "w.json"], &["apply"]))
        .success();
    s.write(
        "p.df",
        &format!(
            "{}lifecycle(main, \"prevent_destroy\")\n",
            net.replace("10.0.0.0/16", "10.1.0.0/16")
        ),
    );
    let r = s
        .run(&common::on(
            "p.df",
            &["--world", "w.json"],
            &["plan", "--json"],
        ))
        .failure();
    let p: Value = serde_json::from_str(&r.stdout).unwrap();
    assert_eq!(p["summary"]["replace"], 1);
    assert_eq!(p["definite"][0]["action"], "replace");
    assert_eq!(p["definite"][0]["create_first"], false);
    assert_eq!(
        p["denied"][0],
        "lifecycle prevent_destroy: the plan would replace net.vpc[\"main\"]"
    );

    s.write(
        "p.df",
        "edition 2026\nresource net.vpc core { cidr = \"10.0.0.0/16\" }\nmoved(net.vpc, \"main\", core)\nprovider fake\n",
    );
    let r = s
        .run(&common::on(
            "p.df",
            &["--world", "w.json"],
            &["plan", "--json"],
        ))
        .success();
    let p: Value = serde_json::from_str(&r.stdout).unwrap();
    assert_eq!(
        p["moved"][0],
        json!({
            "from": {"address": "net.vpc[\"main\"]", "type": "net.vpc", "name": "main"},
            "to": {"address": "net.vpc[\"core\"]", "type": "net.vpc", "name": "core"}
        })
    );
    assert_eq!(p["undeformed"], true);
}

/// R-15: `plan --why --json` carries each deformation's explanation as a
/// `why` array of `{kind, at, text}`; without `--why` there is none.
#[test]
fn plan_json_why_explains_each_deformation() {
    let s = Scratch::new("json-why");
    let prog = repo().join("examples/gke/stacks/gke_two_phase.df");
    let run = |extra: &[&str]| -> Value {
        let mut args = vec![
            "dev",
            "--provider",
            "gke",
            "--world",
            "w.json",
            "plan",
            "--json",
        ];
        args.extend_from_slice(extra);
        args.push(prog.to_str().unwrap());
        let r = s.run(&args).success();
        serde_json::from_str(&r.stdout).unwrap_or_else(|e| panic!("{e}: {}", r.stdout))
    };
    let p = run(&["--why"]);
    let subnet = &p["definite"][0];
    assert_eq!(subnet["name"], "gke_subnet");
    let why = subnet["why"].as_array().unwrap();
    assert_eq!(why[0]["kind"], "rule");
    assert!(
        why[0]["at"]
            .as_str()
            .unwrap()
            .ends_with("examples/gke/stacks/gke_two_phase.df:38"),
        "{}",
        why[0]
    );
    assert!(
        why[0]["text"]
            .as_str()
            .unwrap()
            .starts_with("resource google.compute_subnetwork gke_subnet"),
        "{}",
        why[0]
    );
    assert!(
        why.iter().any(|b| b["kind"] == "fact"
            && b["text"] == "settings[\"dev\"].gke.subnet_cidr = 10.141.76.0/22"),
        "{subnet}"
    );
    // A pending deformation is explained too.
    let pending = p["pending"][0]["deformations"].as_array().unwrap();
    assert!(
        pending
            .iter()
            .all(|d| !d["why"].as_array().unwrap().is_empty()),
        "{pending:?}"
    );
    let p = run(&[]);
    assert!(
        p["definite"][0].get("why").is_none(),
        "{}",
        p["definite"][0]
    );
}
