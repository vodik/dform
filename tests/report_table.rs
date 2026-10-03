//! The result-set printer (R-63): what `query` and `output` print. A
//! header line, aligned rows, two-space gutters; a string longer than the
//! screen (100 columns when stdout is not a terminal, or 24 lines) folded
//! in its cell; a secret as its size; `--json` an array of objects keyed
//! by column. `dform output STACK NAME` prints a value's bare bytes.

mod common;
use common::Scratch;
use serde_json::json;

/// A stack with every kind of output: scalars, a long string, a
/// relation, a secret.
fn applied() -> Scratch {
    let s = Scratch::project("report-table");
    let long: Vec<String> = (0..40).map(|i| format!("line {i}: some yaml")).collect();
    s.write(
        "stacks/app.df",
        &format!(
            "\nkey env: string = \"dev\"\n\
             input pw: secret(string) = \"hunter2hunter2\"\nprovider fake\n\
             resource net.vpc main {{ cidr = \"10.0.0.0/16\" }}\n\
             decl zone(name: string, n: int)\nzone(\"${{env}}-a\", 0)\nzone(\"${{env}}-b\", 1)\n\
             output zone\noutput url = \"https://x.example/${{env}}\"\noutput count = 3\n\
             output wide = \"{wide}\"\noutput kubeconfig = \"{long}\"\n\
             output token: secret(string) = p where pw(p)\n",
            wide = "w".repeat(120),
            long = long.join("\\n"),
        ),
    );
    s.run(&["apply", "app", "env=prod"]).success();
    s
}

/// `dform output STACK`: the scalars as a key/value table, a long string
/// folded to its first line with its size and lines, a string wider than
/// the screen folded too, a secret by what state knows of it (no bytes),
/// and each relation as its own table under its name, its columns its
/// `decl`'s.
#[test]
fn output_lists_scalars_then_each_relation() {
    let s = applied();
    let r = s.run(&["output", "app", "env=prod"]).success();
    let w = "w".repeat(40);
    assert_eq!(
        r.stdout,
        format!(
            "count       3\n\
             kubeconfig  \"line 0: some yaml .. (749 B, 40 lines)\n\
             token       secret\n\
             url         \"https://x.example/prod\"\n\
             wide        \"{} .. (120 B, 1 line)\n\
             \n\
             zone\n\
             name      n\n\
             \"prod-a\"  0\n\
             \"prod-b\"  1\n",
            &w[..39]
        )
    );
    assert!(!r.stdout.contains("hunter2"), "{}", r.stdout);
}

/// `dform output STACK NAME` is for the shell: a string's bytes exactly,
/// unfolded and unquoted; another scalar as the program spells it; a
/// relation's rows tab-separated.
#[test]
fn output_name_prints_bare_bytes() {
    let s = applied();
    let out = |name: &str| s.run(&["output", "app", "env=prod", name]).success().stdout;
    assert_eq!(out("url"), "https://x.example/prod");
    assert_eq!(out("count"), "3\n");
    let kubeconfig = out("kubeconfig");
    assert_eq!(kubeconfig.lines().count(), 40);
    assert!(kubeconfig.starts_with("line 0: some yaml\nline 1:"));
    assert_eq!(out("zone"), "prod-a\t0\nprod-b\t1\n");
    let r = s.run(&["output", "app", "env=prod", "token"]).failure();
    assert!(
        r.stderr
            .contains("output token of stack app[env=prod] is secret: its state keeps no bytes"),
        "{}",
        r.stderr
    );
    let r = s.run(&["output", "app", "env=prod", "nope"]).failure();
    assert!(
        r.stderr.contains(
            "stack app[env=prod] has no output nope \
             (its outputs: count, kubeconfig, token, url, wide, zone)"
        ),
        "{}",
        r.stderr
    );
}

/// `--json`: a relation is an array of objects keyed by column, a secret
/// its label, a string whole.
#[test]
fn output_json_keys_rows_by_column() {
    let s = applied();
    let r = s.run(&["output", "app", "env=prod", "--json"]).success();
    let doc: serde_json::Value = serde_json::from_str(&r.stdout).unwrap();
    assert_eq!(
        doc["zone"],
        json!([{"name": "prod-a", "n": 0}, {"name": "prod-b", "n": 1}])
    );
    assert_eq!(doc["token"], json!({"sensitive": "output.token"}));
    assert_eq!(doc["count"], 3);
    assert_eq!(doc["wide"], "w".repeat(120));
    let r = s
        .run(&["output", "app", "env=prod", "zone", "--json"])
        .success();
    let rows: serde_json::Value = serde_json::from_str(&r.stdout).unwrap();
    assert_eq!(rows, doc["zone"]);
}

/// `query`: a secret is its size in a cell and its label in `--json`; a
/// result set past five rows is counted.
#[test]
fn query_prints_a_secret_by_its_size_and_counts_past_five() {
    let s = Scratch::new("report-table-query");
    s.write(
        "p.df",
        "\ninput pw: secret(string) = \"hunter2hunter2\"\nprovider fake\n\
         n(1)\nn(2)\nn(3)\nn(4)\nn(5)\nn(6)\n",
    );
    let q = |args: &[&str]| {
        let mut all = vec!["query"];
        all.extend_from_slice(args);
        all.push("p.df");
        s.run(&all).success().stdout
    };
    assert_eq!(q(&["pw(P)"]), "P\nsecret(14 B)\n");
    let rows: serde_json::Value = serde_json::from_str(&q(&["pw(P)", "--json"])).unwrap();
    assert_eq!(rows, json!([{"P": {"sensitive": "input.pw"}}]));
    assert_eq!(q(&["n(X), X < 6"]), "X\n1\n2\n3\n4\n5\n");
    assert_eq!(q(&["n(X)"]), "X\n1\n2\n3\n4\n5\n6\n(6 rows)\n");
    assert_eq!(q(&["n(X), X > 6"]), "X\n(0 rows)\n");
}
