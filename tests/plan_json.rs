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
    assert_eq!(p["up_to_date"], false);
    assert_eq!(p["summary"]["changes"], 6);
    assert_eq!(p["summary"]["create"], 6);
    assert_eq!(p["summary"]["ticks"], 2);
    assert_eq!(p["summary"]["undetermined"], 1);
    let ticks = p["ticks"].as_array().unwrap();
    assert_eq!(ticks.len(), 2);
    assert_eq!(ticks[0]["tick"], 1);
    assert_eq!(ticks[0]["after"], Value::Null);
    assert_eq!(ticks[0]["changes"].as_array().unwrap().len(), 3);
    let subnet = &ticks[0]["changes"][0];
    assert_eq!(subnet["kind"], "create");
    assert_eq!(
        subnet["address"],
        "google.compute_subnetwork[\"gke_subnet\"]"
    );
    assert_eq!(subnet["type"], "google.compute_subnetwork");
    let cidr = &subnet["changes"][0];
    assert_eq!(
        (&cidr["op"], &cidr["path"], &cidr["before"], &cidr["after"]),
        (
            &json!("set"),
            &json!("ip_cidr_range"),
            &Value::Null,
            &json!("10.141.76.0/22")
        )
    );
    // The second tick: after the first, with the values it waits on.
    assert_eq!(ticks[1]["tick"], 2);
    assert_eq!(ticks[1]["after"], 1);
    assert_eq!(
        ticks[1]["waits_on"][0],
        json!({"null": "google.container_cluster[\"pngu\"].ca_certificate", "class": "open"})
    );
    let secret = ticks[1]["changes"]
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
    // Tick 2's: the rule that may add an unknown number once tick 1 has
    // run (R-156); `later`'s: the deny undetermined until a tick.
    let groups = ticks[1]["groups"].as_array().unwrap();
    assert_eq!(groups[0]["kind"], "group");
    assert_eq!(
        groups[0]["address"],
        "google.container_node_pool[\"np-${z}\"]"
    );
    let later = p["later"].as_array().unwrap();
    assert_eq!(later[0]["kind"], "deny");
    assert_eq!(later[0]["status"], "undetermined");
    assert_eq!(later[0]["after"], 1);
    // Nothing to decide: no `apply` line.
    assert_eq!(p["apply"], json!(null));
    assert_eq!(p["shadowed"], json!([]));
    assert_eq!(p["conflicts"], json!([]));
}

/// Nulls carry their class: a fresh id.
#[test]
fn plan_json_nulls_carry_their_class() {
    let s = Scratch::new("json-null");
    let p = gke_json(&s);
    let addr = p["ticks"][0]["changes"]
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
fn an_up_to_date_stack_is_a_document_too() {
    let s = Scratch::new("json-up-to-date");
    s.write(
        "p.df",
        "\nresource net.vpc main { cidr = \"10.0.0.0/16\" }\nuse fake\n",
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
    assert_eq!(p["up_to_date"], true);
    assert_eq!(p["summary"]["changes"], 0);
    assert_eq!(p["ticks"], json!([]));
}

#[test]
fn query_json_lists_the_facts() {
    let s = Scratch::new("json-query");
    s.write(
        "p.df",
        "\nresource net.vpc main { cidr = \"10.0.0.0/16\" }\nuse fake\n",
    );
    let r = s
        .run(&[
            "dev", "--world", "w.json", "query", "want", "--json", "p.df",
        ])
        .success();
    let q: Value = serde_json::from_str(&r.stdout).unwrap();
    assert_eq!(q, json!([{"type": "net.vpc", "address": "main"}]));
}

/// `query --json` spells a secret and a null the way `plan --json` does.
#[test]
fn query_json_redacts_like_the_plan() {
    let s = Scratch::new("json-query-secret");
    s.write(
        "p.df",
        "\nresource leaky.vault v { password = \"VAULT-SECRET-DO-NOT-PRINT\" }\nresource net.subnet a { vpc_id = ref(net.vpc, \"main\", \"id\") }\nresource net.vpc main { cidr = \"10.0.0.0/16\" }\nuse fake\n",
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
        q[0]["V"],
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
        q[0]["V"],
        json!({"null": "net.vpc[\"main\"]", "class": "fresh"})
    );
}

/// The executor's plan entries in JSON: a replace with its order, the
/// prevent_destroy deny, and a moved rename.
#[test]
fn replace_denied_and_moved_are_in_the_document() {
    let s = Scratch::new("json-replace");
    let net = "\nresource net.vpc main { cidr = \"10.0.0.0/16\" }\nuse fake\n";
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
    let replace = &p["ticks"][0]["changes"][0];
    assert_eq!(replace["kind"], "replace");
    assert_eq!(replace["create_first"], false);
    assert_eq!(replace["immutable"], json!(["cidr"]));
    assert_eq!(
        p["denied"][0]["text"],
        "lifecycle prevent_destroy: the plan would replace net.vpc[\"main\"]"
    );

    s.write(
        "p.df",
        "\nresource net.vpc core { cidr = \"10.0.0.0/16\" }\nmoved(net.vpc, \"main\", core)\nuse fake\n",
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
    assert_eq!(p["up_to_date"], true);
}

/// R-15, R-79: `plan --why --json` carries each change's explanation as a
/// `why` array of `{kind, at, text}`; by default (`--why=line`) each change
/// and attribute carries its `site` and the change its `because`, and
/// `--why=none` carries neither.
#[test]
fn plan_json_why_explains_each_change() {
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
    let subnet = &p["ticks"][0]["changes"][0];
    assert_eq!(subnet["name"], "gke_subnet");
    // Each attribute's chain (R-122): the expression, then the settings
    // block that gives the input it reads, each where it is written,
    // relative to the project's root (R-38).
    let cidr = &subnet["changes"][0];
    assert_eq!(cidr["path"], "ip_cidr_range");
    assert_eq!(
        cidr["chain"],
        json!([
            {"expr": "gke.subnet_cidr", "at": "stacks/gke_two_phase.df:43"},
            {"expr": "\"10.141.76.0/22\"", "at": "stacks/gke_two_phase.df:28"},
        ]),
        "{subnet}"
    );
    assert!(subnet.get("why").is_none(), "{subnet}");
    // A change of a later tick is explained too.
    let pending = p["ticks"][1]["changes"].as_array().unwrap();
    assert!(
        pending.iter().all(|d| d["changes"][0]["chain"]
            .as_array()
            .is_some_and(|c| !c.is_empty())),
        "{pending:?}"
    );
    let p = run(&[]);
    let subnet = &p["ticks"][0]["changes"][0];
    assert!(subnet["changes"][0].get("chain").is_none(), "{subnet}");
    assert_eq!(
        subnet["site"]["at"], "stacks/gke_two_phase.df:38",
        "{subnet}"
    );
    assert_eq!(
        subnet["changes"][1]["site"]["entry"], "name = \"${name}-gke-subnet\"",
        "{subnet}"
    );
    assert!(subnet.get("because").is_some(), "{subnet}");
    let p = run(&["--why=none"]);
    let subnet = &p["ticks"][0]["changes"][0];
    assert!(subnet.get("site").is_none(), "{subnet}");
    assert!(subnet["changes"][0].get("site").is_none(), "{subnet}");
}
