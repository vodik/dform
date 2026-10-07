//! Redaction is by taint, not by text (R-128): a value prints
//! `(sensitive)` because its cell, or the expression it came from, holds a
//! secret, never because another secret's bytes occur in its text.
//!
//! The shape of a real project: a component's Secret holds the database's
//! user name (`database = "synapse"`, a plain input) beside a generated
//! password; the stack names other things after the same word: a
//! resource `synapse`, a name `"synapse-config"`, a function argument
//! `random.signing_key("synapse")`, an address segment `synapse_db`. None
//! of them is redacted; the Secret's data is, and so is what a secret
//! flows into. The last line holds too: a secret's bytes, as a whole
//! value, never print.

mod common;
use common::Scratch;

/// A Secret whose `data` is sensitive as a whole, and a public config.
const SCHEMA: &str = r#"
type_provider(kube.secret, "fakecloud")
type_provider(kube.config, "fakecloud")
type_attr(kube.secret, "id", "string", ["computed", "id"])
type_attr(kube.secret, "name", "string", [])
type_attr(kube.secret, "data", "map", ["sensitive"])
type_attr(kube.config, "id", "string", ["computed", "id"])
type_attr(kube.config, "name", "string", [])
type_attr(kube.config, "key", "string", [])
"#;

const POSTGRES: &str = r#"
component postgres {
  input database: string
  resource kube.secret creds {
    name = "${database}-creds"
    data = { user: database, password: random.password("${database}-pw") }
  }
}
"#;

const APPS: &str = r#"
use fake
resource postgres.postgres synapse_db { database = "synapse" }
resource kube.config synapse { name = "synapse-config", key = "synapse" }
let signing = random.signing_key("synapse")
resource kube.secret homeserver {
  name = "synapse-config"
  data = { "homeserver.yaml": "signing: ${signing}" }
}
signed(s) where s = str.format("key=%s", signing)
"#;

fn project(name: &str) -> Scratch {
    let s = Scratch::project(name);
    s.write("schema.df", SCHEMA);
    s.write("postgres.df", POSTGRES);
    s.write("stacks/apps.df", APPS);
    s
}

fn dev(s: &Scratch, args: &[&str]) -> common::Run {
    let mut a = vec!["dev", "--provider", "schema.df", "--world", "w.json"];
    a.extend_from_slice(args);
    s.run(&a)
}

/// No output has a `(sensitive ..)` inside a word or an address, the
/// rewrite a search for a secret's bytes made.
fn unmangled(out: &str) {
    for bad in ["(sensitive kube.secret synapse_db", ")_db", ")-config"] {
        assert!(!out.contains(bad), "{bad}: {out}");
    }
}

#[test]
fn a_plain_word_inside_a_secret_object_prints_everywhere_else() {
    let s = project("redact-taint-plan");
    let r = dev(&s, &["plan", "apps", "-vv"]).success();
    let out = &r.stdout;
    unmangled(out);
    // A literal elsewhere, the same word whole, and an address.
    assert!(out.contains("name = \"synapse-config\""), "{out}");
    assert!(out.contains("key = \"synapse\""), "{out}");
    assert!(out.contains("+ kube.config synapse "), "{out}");
    assert!(out.contains("+ kube.secret synapse_db.creds "), "{out}");
    assert!(out.contains("+ postgres.postgres synapse_db\n"), "{out}");
    // A function argument, in the label of the secret it derives.
    assert!(
        out.contains("(sensitive random.signing_key(\"synapse\"))"),
        "{out}"
    );
    // The secrets: by path in the plan.
    assert!(
        out.contains("data.\"homeserver.yaml\" = (sensitive)"),
        "{out}"
    );
    assert!(
        out.contains("data.password = (sensitive random.password(\"synapse-pw\"))"),
        "{out}"
    );

    let r = dev(&s, &["plan", "apps", "--json"]).success();
    unmangled(&r.stdout);
    assert!(
        r.stdout.contains("\"after\": \"synapse-config\""),
        "{}",
        r.stdout
    );
    assert!(r.stdout.contains("\"after\": \"synapse\""), "{}", r.stdout);
    assert!(
        r.stdout.contains("\"name\": \"synapse_db.creds\""),
        "{}",
        r.stdout
    );
}

/// `query` and `why`: a secret object prints as one secret, not field by
/// field, and the plain values beside it as themselves.
#[test]
fn query_and_why_redact_by_taint() {
    let s = project("redact-taint-query");
    let r = dev(&s, &["query", "arg(t, a, p, v, r)", "apps"]).success();
    let out = &r.stdout;
    unmangled(out);
    assert!(
        out.contains("\"synapse_db.creds\"  \"data\"      secret("),
        "{out}"
    );
    assert!(!out.contains("{password: secret"), "{out}");
    assert!(out.contains("\"synapse-config\""), "{out}");
    assert!(
        out.contains("\"synapse_db\"        \"database\"  \"synapse\""),
        "{out}"
    );
    // What a secret flows into is one, by the relation's column.
    let r = dev(&s, &["query", "signed(S)", "apps", "--json"]).success();
    assert!(
        r.stdout.contains("\"sensitive\": \"signed#0\""),
        "{}",
        r.stdout
    );

    let r = dev(&s, &["why", "kube.config synapse", "apps"]).success();
    unmangled(&r.stdout);
    assert!(r.stdout.contains("key = \"synapse\""), "{}", r.stdout);
    assert!(
        r.stdout.contains("name = \"synapse-config\""),
        "{}",
        r.stdout
    );
    let r = dev(&s, &["why", "kube.secret homeserver", "apps"]).success();
    unmangled(&r.stdout);
    assert!(
        r.stdout.contains(
            "data.\"homeserver.yaml\" = (sensitive kube.secret homeserver.data.homeserver.yaml)"
        ),
        "{}",
        r.stdout
    );
    assert!(
        r.stdout.contains("name = \"synapse-config\""),
        "{}",
        r.stdout
    );
}

/// A conflict's witnesses quote rule text: the literals print, and the
/// address is the resource's.
#[test]
fn a_conflict_witness_quotes_plain_literals() {
    let s = project("redact-taint-conflict");
    s.write(
        "stacks/apps.df",
        &format!("{APPS}ok(1)\nset synapse.name = \"synapse-two\" where ok(1)\n"),
    );
    let r = dev(&s, &["plan", "apps", "-vv"]).failure();
    unmangled(&r.stdout);
    assert!(
        r.stdout
            .contains("! kube.config synapse.name: two contributions disagree"),
        "{}",
        r.stdout
    );
    assert!(r.stdout.contains("\"synapse-config\""), "{}", r.stdout);
    assert!(r.stdout.contains("\"synapse-two\""), "{}", r.stdout);
}

/// The last line: the secrets' bytes, as the provider holds them, are in
/// no output, before the apply or after it.
#[test]
fn a_secrets_bytes_never_print() {
    let s = project("redact-taint-guard");
    let printed = |s: &Scratch| -> Vec<String> {
        let mut out = Vec::new();
        for args in [
            &["plan", "apps", "-vv"][..],
            &["plan", "apps", "--json"],
            &["query", "arg(t, a, p, v, r)", "apps"],
            &["query", "arg(t, a, p, v, r)", "apps", "--json"],
            &["query", "signed(S)", "apps"],
            &["why", "kube.secret homeserver", "apps"],
            &["why", "kube.secret synapse_db.creds", "apps"],
            &["why", "signed(S)", "apps"],
        ] {
            let r = dev(s, args);
            out.push(format!("{args:?}\n{}{}", r.stdout, r.stderr));
        }
        out
    };
    let mut outs = printed(&s);
    dev(&s, &["apply", "apps", "--yes"]).success();
    outs.extend(printed(&s));
    let world: serde_json::Value = serde_json::from_str(&s.read("w.json")).unwrap();
    let res = &world["resources"];
    let password = res["kube.secret::synapse_db.creds"]["attrs"]["data"]["password"]
        .as_str()
        .unwrap()
        .to_string();
    let yaml = res["kube.secret::homeserver"]["attrs"]["data"]["homeserver.yaml"]
        .as_str()
        .unwrap()
        .to_string();
    let key = yaml.strip_prefix("signing: ").unwrap().to_string();
    assert!(password.len() > 16 && key.len() > 16, "{password} {key}");
    for out in &outs {
        for secret in [&password, &yaml, &key] {
            assert!(!out.contains(secret.as_str()), "{secret}: {out}");
        }
    }
}
