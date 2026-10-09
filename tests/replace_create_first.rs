//! Create-first replacement (R-189): `lifecycle(r, "create_first")`, or
//! the type's `type_replace`, makes the replacement before the old object
//! is deleted, which waits a tick for what reads the object to move. An
//! object whose identity is an id the provider assigns is replaced as it
//! is (the fake vpc: tests/executor_replace.rs); one whose name is its
//! identity only where its provider generates names
//! (`type_remote_name`): the replacement is given the next generation of
//! the program's name, state records it, and a reference to the name
//! answers it. Elsewhere it is refused at plan.

mod common;
use common::Scratch;

/// A name-keyed type whose provider generates names (`app.config`), a
/// reader of it, and a name-keyed type whose provider does not
/// (`app.fixed`, `app.rolled` replaced create-first by its type).
const NAMES: &str = r#"
type_provider(app.config, "fakecloud")
type_attr(app.config, "name", "string", ["required", "id"])
type_attr(app.config, "version", "int", ["force_new"])
type_remote_name(app.config, "name")
type_provider(app.reader, "fakecloud")
type_attr(app.reader, "id", "string", ["computed", "id"])
type_provider(app.fixed, "fakecloud")
type_attr(app.fixed, "name", "string", ["required", "id"])
type_attr(app.fixed, "version", "int", ["force_new"])
type_provider(app.rolled, "fakecloud")
type_attr(app.rolled, "name", "string", ["required", "id"])
type_attr(app.rolled, "version", "int", ["force_new"])
type_replace(app.rolled, "create_first")
"#;

const APP: &str = r#"
resource app.config cfg { name = "cfg", version = 1 }
resource app.reader r { config = cfg.name }
lifecycle(cfg, "create_first")
"#;

/// `dform dev --world w.json --provider fake --provider names.df ARGS
/// p.df` in `s`.
fn run(s: &Scratch, args: &[&str]) -> common::Run {
    s.run(&common::on(
        "p.df",
        &[
            "--world",
            "w.json",
            "--provider",
            "fake",
            "--provider",
            "names.df",
        ],
        args,
    ))
}

/// A scratch with APP applied at version 1, then written at `version`.
fn applied(name: &str, version: u32) -> Scratch {
    let s = Scratch::new(name);
    s.write("names.df", NAMES);
    s.write("p.df", APP);
    run(&s, &["apply"]).success();
    s.write(
        "p.df",
        &APP.replace("version = 1", &format!("version = {version}")),
    );
    s
}

/// The world's objects of type `typ`, by key, each with its attributes.
fn objects(s: &Scratch, typ: &str) -> Vec<(String, serde_json::Value)> {
    let w = s.json("w.json");
    w["resources"]
        .as_object()
        .unwrap()
        .iter()
        .filter(|(k, _)| k.starts_with(&format!("{typ}::")))
        .map(|(k, v)| (k.clone(), v["attrs"].clone()))
        .collect()
}

#[test]
fn a_name_keyed_replacement_gets_the_next_generation_and_readers_follow() {
    let s = applied("create-first-named", 2);
    let r = run(&s, &["plan"]).success();
    assert!(
        r.stdout
            .contains("  ± app.config cfg  replace, create first (cfg → cfg-2)  p.df:2"),
        "{}",
        r.stdout
    );
    // The reader's name is unknown until the replacement is made: it
    // moves in tick 2, before the old object goes.
    let tick2 = r.stdout.split("tick 2").nth(1).unwrap_or_default();
    assert!(
        tick2.contains("~ app.reader r") && tick2.contains("- app.config cfg  (deposed)"),
        "{}",
        r.stdout
    );
    run(&s, &["apply"]).success();
    assert_eq!(
        objects(&s, "app.config"),
        [(
            "app.config::cfg-2".to_string(),
            serde_json::json!({"name": "cfg-2", "version": 2})
        )]
    );
    assert_eq!(
        objects(&s, "app.reader")[0].1,
        serde_json::json!({"config": "cfg-2"})
    );
    let st = s.json("w.state.json");
    assert_eq!(st["resources"]["app.config::cfg"]["name"], "cfg-2");
    assert!(st.get("deposed").is_none(), "{st}");
    let r = run(&s, &["plan"]).success();
    assert!(
        r.stdout.ends_with("stack p is up to date\n"),
        "{}",
        r.stdout
    );
    // A read of the name answers it, and says where it came from.
    let r = run(&s, &["why", "cfg.name"]).success();
    assert!(
        r.stdout
            .contains("= \"cfg-2\" @override  dform's name for a replacement of \"cfg\""),
        "{}",
        r.stdout
    );

    // The next replacement takes the next generation.
    s.write("p.df", &APP.replace("version = 1", "version = 3"));
    let r = run(&s, &["plan"]).success();
    assert!(
        r.stdout
            .contains("± app.config cfg  replace, create first (cfg-2 → cfg-3)"),
        "{}",
        r.stdout
    );
    run(&s, &["apply"]).success();
    assert_eq!(objects(&s, "app.config")[0].0, "app.config::cfg-3");
    assert_eq!(objects(&s, "app.reader")[0].1["config"], "cfg-3");
}

/// The program naming the object another way takes the name back: it is
/// the program's again, and state drops the generation.
#[test]
fn a_new_name_in_the_program_is_the_objects() {
    let s = applied("create-first-renamed", 2);
    run(&s, &["apply"]).success();
    s.write(
        "p.df",
        &APP.replace("version = 1", "version = 2")
            .replace("\"cfg\"", "\"conf\""),
    );
    run(&s, &["apply"]).success();
    assert_eq!(objects(&s, "app.config")[0].1["name"], "conf");
    assert_eq!(objects(&s, "app.reader")[0].1["config"], "conf");
    let st = s.json("w.state.json");
    assert!(
        st["resources"]["app.config::cfg"].get("name").is_none(),
        "{st}"
    );
    let r = run(&s, &["plan"]).success();
    assert!(
        r.stdout.ends_with("stack p is up to date\n"),
        "{}",
        r.stdout
    );
}

/// Where the name is the identity and the provider cannot generate one,
/// create_first is refused at plan, by the lifecycle fact or by the
/// type's order.
#[test]
fn create_first_is_refused_where_the_name_is_the_identity() {
    let s = Scratch::new("create-first-refused");
    s.write("names.df", NAMES);
    s.write(
        "p.df",
        "resource app.fixed x { name = \"x\", version = 1 }\nlifecycle(x, \"create_first\")\n",
    );
    let r = run(&s, &["plan"]).failure();
    assert!(
        r.stderr.contains(
            "app.fixed x: create_first is not possible: name is its identity, and its \
             provider cannot generate one"
        ),
        "{}",
        r.stderr
    );

    s.write(
        "p.df",
        "resource app.rolled y { name = \"y\", version = 1 }\n",
    );
    run(&s, &["apply"]).success();
    s.write(
        "p.df",
        "resource app.rolled y { name = \"y\", version = 2 }\n",
    );
    let r = run(&s, &["plan"]).failure();
    assert!(
        r.stderr.contains(
            "app.rolled y: create_first is not possible: name is its identity, and its \
             provider cannot generate one"
        ),
        "{}",
        r.stderr
    );

    // The mock Kubernetes provider: a Namespace's name is its identity.
    let k = Scratch::new("create-first-namespace");
    k.write(
        "p.df",
        "resource k8s.namespace x { metadata.name = \"x\" }\nlifecycle(x, \"create_first\")\n",
    );
    let r = k
        .run(&common::on(
            "p.df",
            &["--world", "w.json", "--provider", "k8s"],
            &["plan"],
        ))
        .failure();
    assert!(
        r.stderr.contains(
            "k8s.namespace x: create_first is not possible: metadata.name is its identity, \
             and its provider cannot generate one"
        ),
        "{}",
        r.stderr
    );
}

/// An apply stopped between the ticks leaves both objects in state, the
/// old one deposed; the next apply moves the reader and deletes it, and
/// makes nothing again.
#[test]
fn an_interrupted_replacement_resumes_with_the_delete() {
    let s = applied("create-first-resume", 2);
    run(&s, &["apply", "--max-ticks", "1"]).failure();
    let st = s.json("w.state.json");
    assert_eq!(st["resources"]["app.config::cfg"]["name"], "cfg-2");
    assert_eq!(st["deposed"]["app.config::cfg"]["remote"], "cfg");
    assert_eq!(objects(&s, "app.config").len(), 2);
    let r = run(&s, &["apply"]).success();
    assert!(
        !r.stdout.contains("+ app.config") && !r.stdout.contains("± app.config"),
        "{}",
        r.stdout
    );
    assert_eq!(objects(&s, "app.config")[0].0, "app.config::cfg-2");
    assert_eq!(objects(&s, "app.config").len(), 1);
    assert_eq!(objects(&s, "app.reader")[0].1["config"], "cfg-2");
    assert!(s.json("w.state.json").get("deposed").is_none());
}

/// A replacement whose answer was lost: the next run finds what its
/// idempotency key made and records the name it was given.
#[test]
fn a_lost_answer_keeps_the_generated_name() {
    let s = applied("create-first-lost", 2);
    run(&s, &["apply", "--chaos", "timeout=app.config[\"cfg\"]"]).failure();
    let r = run(&s, &["apply"]).success();
    let st = s.json("w.state.json");
    assert_eq!(
        st["resources"]["app.config::cfg"]["name"], "cfg-2",
        "{}",
        r.stdout
    );
    assert_eq!(objects(&s, "app.config").len(), 1);
    assert_eq!(objects(&s, "app.reader")[0].1["config"], "cfg-2");
    let r = run(&s, &["plan"]).success();
    assert!(
        r.stdout.ends_with("stack p is up to date\n"),
        "{}",
        r.stdout
    );
}

/// `moved` to another address keeps the generation: the object is not
/// replaced, and its name stays the one dform gave it.
#[test]
fn moved_keeps_the_generation() {
    let s = applied("create-first-moved", 2);
    run(&s, &["apply"]).success();
    s.write(
        "p.df",
        &(APP
            .replace("version = 1", "version = 2")
            .replace("app.config cfg", "app.config conf")
            .replace("cfg.name", "conf.name")
            .replace("lifecycle(cfg", "lifecycle(conf")
            + "moved(app.config, \"cfg\", conf)\n"),
    );
    let r = run(&s, &["plan"]).success();
    assert!(
        r.stdout
            .contains("moved app.config[\"cfg\"] -> app.config[\"conf\"]")
            && r.stdout.ends_with("stack p is up to date\n"),
        "{}",
        r.stdout
    );
    assert!(
        !r.stdout.contains("± ") && !r.stdout.contains("~ app"),
        "{}",
        r.stdout
    );
    run(&s, &["apply"]).success();
    let st = s.json("w.state.json");
    assert_eq!(st["resources"]["app.config::conf"]["name"], "cfg-2");
    assert_eq!(objects(&s, "app.config")[0].1["name"], "cfg-2");
}

/// `status` names an object by the name dform gave it, and `--json` says
/// it as `remote_name`.
#[test]
fn status_says_the_generated_name() {
    let s = Scratch::project("create-first-status");
    s.write(
        "providers/app/schema.df",
        &NAMES.replace("\"fakecloud\"", "\"app\""),
    );
    s.write("stacks/app.df", &format!("use app\n{APP}"));
    s.run(&["apply", "app"]).success();
    s.write(
        "stacks/app.df",
        &format!("use app\n{}", APP.replace("version = 1", "version = 2")),
    );
    s.run(&["apply", "app"]).success();
    let r = s.run(&["status", "app"]).success();
    let lines: Vec<String> = r
        .stdout
        .lines()
        .map(|l| l.split_whitespace().collect::<Vec<_>>().join(" "))
        .collect();
    assert!(
        lines.iter().any(|l| l == "app.config cfg named cfg-2 -"),
        "{}",
        r.stdout
    );
    let r = s.run(&["status", "app", "--json"]).success();
    let doc: serde_json::Value = serde_json::from_str(&r.stdout).unwrap();
    let cfg = doc["objects"]
        .as_array()
        .unwrap()
        .iter()
        .find(|o| o["name"] == "cfg")
        .unwrap();
    assert_eq!(cfg["remote_name"], "cfg-2");
}
