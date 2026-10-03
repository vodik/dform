//! Typed stack inputs: a default is a `@default` contribution, `--set` and
//! `--input-file` normal ones; types, required inputs and refinements are
//! checked.

mod common;
use common::Scratch;

const P: &str = r#"
input env: enum("dev", "staging", "prod") = "staging"
input replicas: int = 2 check 1 <= replicas, replicas <= 5
input nets: list(inet) = []
input owner: string
resource net.vpc main {
  env = e
  replicas = r
  owner = o
} where env(e), replicas(r), owner(o)
resource net.subnet s {
  cidr = n
} where nets(ns), n = ns[i], s = format("s%s", i)
provider fake
"#;

fn scratch() -> Scratch {
    let s = Scratch::project("lang-inputs");
    s.write("p.df", P);
    s
}

fn plan(s: &Scratch, extra: &[&str]) -> common::Run {
    let mut a = vec!["plan"];
    a.extend(extra);
    s.run(&common::on("p.df", &["--world", "w.json"], &a))
}

#[test]
fn a_default_yields_to_set() {
    let s = scratch();
    let r = plan(&s, &["--set", "owner=ops"]).success();
    assert!(r.stdout.contains("env = \"staging\""), "{}", r.stdout);
    assert!(r.stdout.contains("replicas = 2"), "{}", r.stdout);
    let r = plan(&s, &["--set", "owner=ops", "--set", "env=prod"]).success();
    assert!(r.stdout.contains("env = \"prod\""), "{}", r.stdout);
}

#[test]
fn a_required_input_with_no_value_is_an_error_naming_it() {
    let s = scratch();
    let r = plan(&s, &[]).failure();
    assert!(
        r.stderr
            .contains("p.df:5:1: input owner is required and has no value"),
        "{}",
        r.stderr
    );
}

#[test]
fn set_is_checked_against_the_declaration() {
    let s = scratch();
    let r = plan(&s, &["--set", "owner=ops", "--set", "env=qa"]).failure();
    assert!(
        r.stderr
            .contains("--set env=qa: input env is enum(dev, staging, prod)"),
        "{}",
        r.stderr
    );
    let r = plan(&s, &["--set", "owner=ops", "--set", "region=x"]).failure();
    assert!(
        r.stderr
            .contains("--set region: the program declares no input region"),
        "{}",
        r.stderr
    );
    // An int where a string is declared is its text.
    let r = plan(&s, &["--set", "owner=42"]).success();
    assert!(r.stdout.contains("owner = \"42\""), "{}", r.stdout);
}

/// `1 <= replicas <= 5` fits the checkable table: a refinement of the
/// input's cell, violated by the winning value (E DR-13).
#[test]
fn a_refinement_on_an_input_is_a_deny() {
    let s = scratch();
    let r = plan(&s, &["--set", "owner=ops", "--set", "replicas=9"]).failure();
    assert!(
        r.stderr.contains(
            "- refinement violated ctx={\"addr\":\"\",\"at\":\"p.df:3:1\",\"constraint\":\"range(1, 5)\",\"path\":\"replicas\",\"reason\":\"9 violates range(1, 5)\",\"type\":\"input\",\"value\":9,"
        ),
        "{}",
        r.stderr
    );
}

/// An input file holds one fact per input; its values are program terms,
/// so lists and `inet(...)` work, and a value of the wrong type is a
/// violation naming the input.
#[test]
fn an_input_file_gives_inputs_as_facts() {
    let s = scratch();
    s.write(
        "prod.df",
        "\nenv(\"prod\")\nowner(\"ops\")\nnets([inet(\"10.0.0.0/24\"), inet(\"10.0.1.0/24\")])\n",
    );
    let r = plan(&s, &["--input-file", "prod.df"]).success();
    assert!(r.stdout.contains("env = \"prod\""), "{}", r.stdout);
    assert!(
        r.stdout
            .contains("+ net.subnet[\"s1\"]\n  cidr = \"10.0.1.0/24\""),
        "{}",
        r.stdout
    );

    s.write("bad.df", "\nowner(\"ops\")\nnets([\"x\"])\n");
    let r = plan(&s, &["--input-file", "bad.df"]).failure();
    assert!(
        r.stderr.contains("input nets: [\"x\"] is not list(inet)"),
        "{}",
        r.stderr
    );

    s.write("stray.df", "\nowner(\"ops\")\nregion(\"x\")\n");
    let r = plan(&s, &["--input-file", "stray.df"]).failure();
    assert!(
        r.stderr
            .contains("stray.df:3:1: region/1 is not an input of the program"),
        "{}",
        r.stderr
    );
}

/// A value a rule computes is checked after evaluation.
#[test]
fn a_component_input_of_the_wrong_type_is_a_violation() {
    let s = Scratch::project("lang-inputs-module");
    s.write(
        "p.df",
        "\ncomponent m {\n  input n: int\n  resource net.vpc v {\n    n = n_\n  } where n(n_)\n}\ninstance m a { n = format(\"%s\", \"three\") }\nprovider fake\n",
    );
    let r = s
        .run(&["dev", "--world", "w.json", "plan", "p.df"])
        .failure();
    assert!(
        r.stderr.contains("input n of a: three is not int"),
        "{}",
        r.stderr
    );
}

/// The plan file records each input file's digest: `apply PLAN` reads the
/// same file, and refuses when it changed.
#[test]
fn the_plan_file_records_input_files() {
    let s = scratch();
    s.write("prod.df", "\nenv(\"prod\")\nowner(\"ops\")\n");
    s.run(&[
        "dev",
        "--world",
        "w.json",
        "--input-file",
        "prod.df",
        "plan",
        "--out",
        "plan.json",
        "p.df",
    ])
    .success();
    assert!(s.read("plan.json").contains("\"input_files\""));
    s.run(&["apply", "plan.json"]).success();
    s.write("prod.df", "\nenv(\"dev\")\nowner(\"ops\")\n");
    let r = s.run(&["apply", "plan.json"]).failure();
    assert!(
        r.stderr
            .contains("--input-file prod.df: changed since the plan"),
        "{}",
        r.stderr
    );
}

/// An input file may hold a secret: the plan file records its digest keyed
/// with the stack's plan key (HMAC-SHA256, as a sensitive leaf), never an
/// unkeyed hash of its bytes that could be brute-forced.
#[test]
fn the_plan_file_digests_input_files_with_the_stack_key() {
    let text = "\nenv(\"prod\")\nowner(\"hunter2\")\n";
    let fnv = {
        let mut h: u64 = 0xcbf29ce484222325;
        for b in text.bytes() {
            h ^= b as u64;
            h = h.wrapping_mul(0x100000001b3);
        }
        format!("{h:016x}")
    };
    let digest = |name: &str| -> String {
        let s = Scratch::project(name);
        s.write("p.df", P);
        s.write("prod.df", text);
        let args = [
            "--input-file",
            "prod.df",
            "plan",
            "--out",
            "plan.json",
            "p.df",
        ];
        s.run(&args).success();
        let f: serde_json::Value = serde_json::from_str(&s.read("plan.json")).unwrap();
        let files = f["inputs"]["input_files"].as_array().unwrap().clone();
        assert_eq!(files.len(), 1, "{f}");
        assert_eq!(files[0]["path"], "prod.df");
        let d = files[0]["digest"].as_str().unwrap_or_default().to_string();
        assert!(!s.read("plan.json").contains(&fnv), "{f}");
        s.run(&args).success();
        let again: serde_json::Value = serde_json::from_str(&s.read("plan.json")).unwrap();
        assert_eq!(
            again["inputs"]["input_files"][0]["digest"],
            d.as_str(),
            "same key, same digest"
        );
        d
    };
    let (a, b) = (digest("lang-inputs-key-a"), digest("lang-inputs-key-b"));
    assert_eq!(a.len(), 64, "{a}");
    assert_ne!(a, b, "another stack key, another digest");
}

/// `--set k=@FILE` reads a document as the input's value, parsed as its
/// type the way a YAML cell is (`net` an inet); a `.df` file of one fact
/// is the facts form. A plan file records the file's digest, so an edited
/// file makes it stale.
#[test]
fn set_reads_a_file_as_the_inputs_type() {
    let s = Scratch::project("lang-inputs-file");
    s.write(
        "p.df",
        r#"
input db: { size: int, net: inet, zones: list(string) }
provider fake
resource net.vpc main {
  cidr = inet.subnet(db.net, 8, db.size)
  zones = db.zones
}
"#,
    );
    s.write("db.yaml", "size: 2\nnet: 10.0.0.0/16\nzones: [a, b]\n");
    s.write(
        "db.json",
        r#"{"size": 3, "net": "10.1.0.0/16", "zones": ["c"]}"#,
    );
    s.write("db.toml", "size = 4\nnet = \"10.2.0.0/16\"\nzones = []\n");
    s.write(
        "db.df",
        "db({ size: 5, net: \"10.3.0.0/16\", zones: [\"d\"] })\n",
    );
    for (file, cidr) in [
        ("db.yaml", "10.0.2.0/24"),
        ("db.json", "10.1.3.0/24"),
        ("db.toml", "10.2.4.0/24"),
        ("db.df", "10.3.5.0/24"),
    ] {
        let set = format!("db=@{file}");
        let r = s.run(&["plan", "p.df", "--set", &set]).success();
        assert!(
            r.stdout.contains(&format!("cidr = \"{cidr}\"")),
            "{file}: {}",
            r.stdout
        );
    }
    let r = s.run(&["plan", "p.df", "--set", "db=@db.txt"]).failure();
    assert!(
        r.stderr
            .contains("--set db=@db.txt: a .yaml, .json, .toml or .df file"),
        "{}",
        r.stderr
    );
    // The plan file records the document's digest.
    s.run(&["plan", "p.df", "--set", "db=@db.yaml", "--out", "plan.json"])
        .success();
    let plan = s.read("plan.json");
    assert!(plan.contains(r#""set": "db=@db.yaml""#), "{plan}");
    s.write("db.yaml", "size: 9\nnet: 10.0.0.0/16\nzones: [a, b]\n");
    let r = s.run(&["apply", "plan.json"]).failure();
    assert!(r.stderr.contains("is stale"), "{}", r.stderr);
}

const OBJECT: &str = r#"
input nodes {
  flavor: string = "b3-8"
  count: int = 1 check 1 <= count, count <= 3
  pool: {
    size: int = 4
  }
}
provider fake
resource net.vpc main {
  flavor = nodes.flavor
  count = nodes.count
  size = nodes.pool.size
}
"#;

fn object_plan(s: &Scratch, extra: &[&str]) -> common::Run {
    let mut a = vec!["dev", "--world", "w.json", "plan", "p.df"];
    a.extend(extra);
    s.run(&a)
}

/// `input k { f: T = d check B .. }` (R-54): an object input by its
/// fields, nested ones in braces, each field's default and check its own
/// leaf's; `--set` gives a leaf by its path and is read as its type.
#[test]
fn an_object_input_is_a_block_of_fields() {
    let s = Scratch::project("lang-inputs-object");
    s.write("p.df", OBJECT);
    let r = object_plan(&s, &[]).success();
    assert!(
        r.stdout
            .contains("  count = 1\n  flavor = \"b3-8\"\n  size = 4\n"),
        "{}",
        r.stdout
    );
    let r = object_plan(
        &s,
        &["--set", "nodes.count=2", "--set", "nodes.pool.size=8"],
    )
    .success();
    assert!(
        r.stdout
            .contains("  count = 2\n  flavor = \"b3-8\"\n  size = 8\n"),
        "{}",
        r.stdout
    );
    // The field's check is the leaf's refinement.
    let r = object_plan(&s, &["--set", "nodes.count=9"]).failure();
    assert!(
        r.stderr
            .contains("\"path\":\"nodes.count\",\"reason\":\"9 violates range(1, 3)\""),
        "{}",
        r.stderr
    );
    // `why` shows the leaf's layers: the default and the `--set`.
    let r = s
        .run(&[
            "dev",
            "--world",
            "w.json",
            "why",
            "nodes.count",
            "p.df",
            "--set",
            "nodes.count=2",
        ])
        .success();
    assert!(
        r.stdout
            .starts_with("input nodes = {count: 2, flavor: \"b3-8\", pool: {size: 4}}\n"),
        "{}",
        r.stdout
    );
    assert!(
        r.stdout.contains("{count: 1} @default   p.df:4"),
        "{}",
        r.stdout
    );
    assert!(r.stdout.contains("└─ --set nodes.count=2"), "{}", r.stdout);
    // A leaf is read as its type.
    let r = object_plan(&s, &["--set", "nodes.count=two"]).failure();
    assert!(
        r.stderr
            .contains("--set nodes.count=two: input nodes.count is int"),
        "{}",
        r.stderr
    );
}

/// A path that names no field is an error naming the object's fields.
#[test]
fn set_of_a_field_the_object_has_not_names_its_fields() {
    let s = Scratch::project("lang-inputs-object-path");
    s.write("p.df", OBJECT);
    let r = object_plan(&s, &["--set", "nodes.cnt=2"]).failure();
    assert!(
        r.stderr.contains(
            "--set nodes.cnt: input nodes has no field cnt (its fields: flavor, count, pool)"
        ),
        "{}",
        r.stderr
    );
    let r = object_plan(&s, &["--set", "nodes.pool.sz=2"]).failure();
    assert!(
        r.stderr
            .contains("--set nodes.pool.sz: input nodes.pool has no field sz (its fields: size)"),
        "{}",
        r.stderr
    );
    // An object is given a document, not a scalar.
    let r = object_plan(&s, &["--set", "nodes=3"]).failure();
    assert!(
        r.stderr.contains(
            "--set nodes=3: input nodes is an object of flavor, count, pool.size: give a field"
        ),
        "{}",
        r.stderr
    );
}

/// `--set k=@FILE` gives the object a document: each field it has wins
/// over that leaf's default, a field it leaves out keeps the default, and
/// a field the object has not is an error.
#[test]
fn set_of_an_object_from_a_document_gives_its_fields() {
    let s = Scratch::project("lang-inputs-object-file");
    s.write("p.df", OBJECT);
    s.write("n.yaml", "flavor: c3-4\npool:\n  size: 6\n");
    let r = object_plan(&s, &["--set", "nodes=@n.yaml"]).success();
    assert!(
        r.stdout
            .contains("  count = 1\n  flavor = \"c3-4\"\n  size = 6\n"),
        "{}",
        r.stdout
    );
    s.write("bad.yaml", "flavour: c3-4\n");
    let r = object_plan(&s, &["--set", "nodes=@bad.yaml"]).failure();
    assert!(
        r.stderr.contains(
            "--set nodes: input nodes has no field flavour (its fields: flavor, count, pool)"
        ),
        "{}",
        r.stderr
    );
}

/// `set k.f = t where B` is the same contribution from inside the
/// program, to the leaf (R-38's settings block is its sugar).
#[test]
fn set_of_a_field_where_a_condition_holds() {
    let s = Scratch::project("lang-inputs-object-set");
    s.write(
        "p.df",
        &format!(
            "{}set nodes.count = 3 where env == \"prod\"\n",
            OBJECT.replacen("\n", "\nkey env: enum(\"dev\", \"prod\") = \"dev\"\n", 1)
        ),
    );
    let r = object_plan(&s, &[]).success();
    assert!(r.stdout.contains("  count = 1\n"), "{}", r.stdout);
    let r = s
        .run(&["dev", "--world", "w.json", "plan", "p.df", "env=prod"])
        .success();
    assert!(r.stdout.contains("  count = 3\n"), "{}", r.stdout);
    s.write(
        "p.df",
        &s.read("p.df")
            .replace("set nodes.count = 3", "set nodes.cnt = 3"),
    );
    let r = object_plan(&s, &[]).failure();
    assert!(
        r.stderr
            .contains("`set nodes.cnt`: input nodes has no field cnt"),
        "{}",
        r.stderr
    );
}

/// The alias form, `input k: T = { .. }` with an object type, is the same
/// input as the block form: the same leaves, the same plan.
#[test]
fn the_alias_form_and_the_block_form_plan_identically() {
    let s = Scratch::project("lang-inputs-object-alias");
    s.write("p.df", OBJECT);
    let block = object_plan(&s, &["--set", "nodes.count=2"])
        .success()
        .stdout;
    s.write(
        "p.df",
        &(OBJECT.replace(
            "input nodes {\n  flavor: string = \"b3-8\"\n  count: int = 1 check 1 <= count, count <= 3\n  pool: {\n    size: int = 4\n  }\n}\n",
            "input nodes: node_pool = { flavor: \"b3-8\", count: 1, pool: { size: 4 } }\n",
        ) + "type node_pool = { flavor: string, count: int, pool: { size: int } }\n"),
    );
    let alias = object_plan(&s, &["--set", "nodes.count=2"])
        .success()
        .stdout;
    assert_eq!(block, alias);
}

/// A field with no default is required like an input, named by its path;
/// a component's object input is given a leaf or the whole object in its
/// instance block; a key is a scalar.
#[test]
fn a_field_with_no_default_is_required() {
    let s = Scratch::project("lang-inputs-object-required");
    s.write(
        "p.df",
        "\ninput db {\n  size: int\n  zone: string = \"a\"\n}\nprovider fake\n\
         resource net.vpc main {\n  size = db.size\n  zone = db.zone\n}\n",
    );
    let r = object_plan(&s, &[]).failure();
    assert!(
        r.stderr
            .contains("p.df:3:3: input db.size is required and has no value"),
        "{}",
        r.stderr
    );
    let r = object_plan(&s, &["--set", "db.size=3"]).success();
    assert!(
        r.stdout.contains("  size = 3\n  zone = \"a\"\n"),
        "{}",
        r.stdout
    );

    s.write(
        "p.df",
        "\ncomponent m {\n  input db {\n    size: int\n    zone: string = \"a\"\n  }\n\
         resource net.vpc v {\n    size = db.size\n    zone = db.zone\n  }\n}\n\
         instance m a { db.size = 2 }\ninstance m b { db = { size: 5, zone: \"b\" } }\n\
         provider fake\n",
    );
    let r = object_plan(&s, &[]).success();
    assert!(
        r.stdout
            .contains("  + net.vpc[\"a/v\"]\n    size = 2\n    zone = \"a\"\n"),
        "{}",
        r.stdout
    );
    assert!(
        r.stdout
            .contains("  + net.vpc[\"b/v\"]\n    size = 5\n    zone = \"b\"\n"),
        "{}",
        r.stdout
    );
    s.write("p.df", &s.read("p.df").replace("db.size = 2", "db.sz = 2"));
    let r = object_plan(&s, &[]).failure();
    assert!(
        r.stderr.contains("input db of component m has no field sz"),
        "{}",
        r.stderr
    );

    s.write("p.df", "\nkey env {\n  a: int\n}\n");
    let r = object_plan(&s, &[]).failure();
    assert!(
        r.stderr.contains("a relation or an object is not a key"),
        "{}",
        r.stderr
    );
}
