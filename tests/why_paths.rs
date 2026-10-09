//! After R-217: `why` takes every path the plan prints, an element of a
//! keyed list by its key (`pg.spec.ports[port=5432,protocol=TCP].protocol`,
//! R-158) and an element of any list by its position, and says a value
//! as the plan does: in the program's field order, a default no write of
//! the program made with its note (R-217).

mod common;
use common::Scratch;

const SCHEMA: &str = r#"
type_provider("kube.service", "kube")
type_attr("kube.service", "metadata.name", "string", ["required"])
type_attr("kube.service", "spec.selector", "map", [])
type_attr("kube.service", "spec.ports", "list", ["required"])
type_list_key("kube.service", "spec.ports", ["port", "protocol"])
type_default("kube.service", "spec.ports.protocol", "TCP")
type_attr("kube.service", "spec.aliases", "list", [])
"#;

const MAIN: &str = r#"use kube

resource kube.service pg {
  metadata.name = "pg"
  spec = { selector: { app: "pg" }, ports: [{ port: 5432, targetPort: 5432 }] }
  spec.aliases = ["db", "postgres"]
}
"#;

fn project(name: &str) -> Scratch {
    let s = Scratch::project(name);
    s.write(
        "dform.toml",
        "[project]\nedition = \"2026\"\n\n[providers]\nkube = \"providers/kube\"\n",
    );
    s.write("providers/kube/schema.df", SCHEMA);
    s.write("main.df", MAIN);
    s
}

fn run(s: &Scratch, args: &[&str]) -> common::Run {
    let mut all = vec!["dev", "--world", "w.json"];
    all.extend_from_slice(args);
    s.run(&all)
}

/// Every leaf path `plan -vv` prints is one `why` takes, and `why` heads
/// it with the same path: a keyed element's field with the chain of the
/// write that made it, the default the schema gave with its note and no
/// chain; an element by its position is said by its key, as the plan
/// says it.
#[test]
fn why_takes_each_path_the_plan_prints() {
    let s = project("why-paths-keyed");
    let plan = run(&s, &["plan", "-vv", "main.df"]).success().stdout;
    let paths: Vec<&str> = plan
        .lines()
        .filter_map(|l| l.trim().split_once(" = "))
        .map(|(p, _)| p)
        .collect();
    assert!(
        paths.contains(&"spec.ports[port=5432,protocol=TCP].protocol"),
        "{plan}"
    );
    for p in &paths {
        let why = run(&s, &["why", &format!("pg.{p}"), "main.df"])
            .success()
            .stdout;
        assert!(
            why.starts_with(&format!("kube.service pg.{p} = ")),
            "{p}:\n{why}"
        );
    }
    let field = "kube.service pg.spec.ports[port=5432,protocol=TCP].targetPort = 5432\n  \
                 = [{ port: 5432, targetPort: 5432 }]  main.df:5\n";
    for p in [
        "pg.spec.ports[port=5432,protocol=TCP].targetPort",
        "kube.service pg.spec.ports[port=5432,protocol=TCP].targetPort",
        "pg.spec.ports[0].targetPort",
    ] {
        let why = run(&s, &["why", p, "main.df"]).success().stdout;
        assert_eq!(why, field, "{p}");
    }
    let why = run(
        &s,
        &[
            "why",
            "pg.spec.ports[port=5432,protocol=TCP].protocol",
            "main.df",
        ],
    )
    .success()
    .stdout;
    assert_eq!(
        why,
        "kube.service pg.spec.ports[port=5432,protocol=TCP].protocol = \"TCP\" (schema default)\n"
    );
    let why = run(&s, &["why", "pg.spec.aliases[1]", "main.df"])
        .success()
        .stdout;
    assert_eq!(
        why,
        "kube.service pg.spec.aliases[1] = \"postgres\"\n  \
         = [\"db\", \"postgres\"]  main.df:6\n"
    );
    let json = run(
        &s,
        &["why", "--json", "pg.spec.ports[0].protocol", "main.df"],
    )
    .success()
    .stdout;
    let json: serde_json::Value = serde_json::from_str(&json).unwrap();
    assert_eq!(
        json[0]["fact"],
        "kube.service pg.spec.ports[port=5432,protocol=TCP].protocol"
    );
    assert_eq!(json[0]["value"], "TCP");
}

/// `why` of a written value says it as the plan prints it: the same
/// lines, in the program's field order, the default inside it with its
/// note; an element of the list the same as the list holds it.
#[test]
fn why_says_a_value_as_the_plan_does() {
    let s = project("why-paths-fold");
    let plan = run(&s, &["plan", "main.df"]).success().stdout;
    let value = r#"spec = {
  selector: { app: "pg" },
  ports: [{
    port: 5432,
    targetPort: 5432,
    protocol: "TCP" (schema default),
  }],
}
"#;
    let indented: String = value.lines().map(|l| format!("      {l}\n")).collect();
    assert!(plan.contains(&indented), "{plan}");
    let why = run(&s, &["why", "pg.spec", "main.df"]).success().stdout;
    assert!(
        why.starts_with(&format!(
            "kube.service pg.{value}  \
             = {{ selector: {{ app: \"pg\" }}, ports: [{{ port: 5432, targetPort: 5432 }}] }}  \
             main.df:5\n"
        )),
        "{why}"
    );
    let why = run(
        &s,
        &["why", "pg.spec.ports[port=5432,protocol=TCP]", "main.df"],
    )
    .success()
    .stdout;
    assert_eq!(
        why,
        "kube.service pg.spec.ports[port=5432,protocol=TCP] = {\n  port: 5432,\n  \
         targetPort: 5432,\n  protocol: \"TCP\" (schema default),\n}\n  \
         = [{ port: 5432, targetPort: 5432 }]  main.df:5\n"
    );
}

/// A path past a list that names no element of it, or no field of the
/// element, says what it names and the nearest the value has, as the
/// plan prints it; what `why` cannot read at all says the forms it
/// takes, in the plan's spelling.
#[test]
fn a_path_that_reaches_nothing_says_the_nearest() {
    let s = project("why-paths-bad");
    for (p, want) in [
        (
            "pg.spec.ports[port=1]",
            "why: kube.service pg.spec.ports has no element [port=1]: an element is named as \
             the plan prints it, by its key in a keyed list, else by its position\n  \
             help: the nearest it has is 'pg.spec.ports[port=5432,protocol=TCP]'\n",
        ),
        (
            "pg.spec.ports[port=5432,protocol=TCP].prot",
            "why: kube.service pg.spec.ports[port=5432,protocol=TCP] has no field prot: a field \
             is named by its key\n  \
             help: the nearest it has is 'pg.spec.ports[port=5432,protocol=TCP].port'\n",
        ),
        (
            "pg.spec.aliases[2]",
            "why: kube.service pg.spec.aliases has no element [2]: an element is named as the \
             plan prints it, by its key in a keyed list, else by its position\n  \
             help: the nearest it has is 'pg.spec.aliases[1]'\n",
        ),
        (
            "pg.spec[0]",
            "why: kube.service pg.spec is not a list, so it has no element [0]\n",
        ),
    ] {
        let r = run(&s, &["why", p, "main.df"]).failure();
        assert_eq!(r.stderr, format!("Error: {want}"), "{p}");
    }
    let r = run(&s, &["why", "not a path", "main.df"]).failure();
    assert!(
        r.stderr.contains(
            "why: expected an address as the plan prints it, 'net.vpc main' or its path \
             'main', an attribute such as 'main.cidr' or \
             'pg.spec.ports[port=5432,protocol=TCP].protocol'"
        ),
        "{}",
        r.stderr
    );
}

/// The full address, `T["A"]`, is the plan file's and `--json`'s: past
/// it a path names an element as the plan prints it too, by its key.
#[test]
fn the_full_address_takes_a_keyed_element() {
    let s = project("why-paths-full");
    let why = run(
        &s,
        &[
            "why",
            r#"kube.service["pg"].spec.ports[port=5432,protocol=TCP].port"#,
            "main.df",
        ],
    )
    .success()
    .stdout;
    assert!(
        why.starts_with("kube.service pg.spec.ports[port=5432,protocol=TCP].port = 5432\n"),
        "{why}"
    );
}
