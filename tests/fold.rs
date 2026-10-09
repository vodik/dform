//! R-217: a written object prints as the document the provider receives.
//! What the program did not write inside a value it wrote is folded in
//! with a dim note (`protocol: "TCP" (schema default)`), which breaks the
//! value over lines; a default no written value holds keeps its own line,
//! with the same note; a secret inside a value is `(sensitive)` in place;
//! `-v` lays the value out the same; `why` keeps each leaf's chain.

mod common;
use common::Scratch;

const SCHEMA: &str = r#"
type_provider("kube.service", "kube")
type_attr("kube.service", "metadata.name", "string", ["required"])
type_attr("kube.service", "metadata.labels", "map", [])
type_attr("kube.service", "spec.selector", "map", [])
type_attr("kube.service", "spec.ports", "list", ["required"])
type_list_key("kube.service", "spec.ports", ["port", "protocol"])
type_default("kube.service", "spec.ports.protocol", "TCP")
type_attr("kube.service", "spec.token", "string", ["sensitive"])
type_attr("kube.service", "status.ports", "list", [])
type_list_key("kube.service", "status.ports", ["port", "protocol"])
type_default("kube.service", "status.ports.protocol", "TCP")
"#;

fn project(name: &str, main: &str) -> Scratch {
    let s = Scratch::project(name);
    s.write(
        "dform.toml",
        "[project]\nedition = \"2026\"\n\n[providers]\nkube = \"providers/kube\"\n",
    );
    s.write("providers/kube/schema.df", SCHEMA);
    s.write("main.df", main);
    s
}

fn run(s: &Scratch, args: &[&str]) -> String {
    let mut all = vec!["dev", "--world", "w.json"];
    all.extend_from_slice(args);
    s.run(&all).success().stdout
}

/// The apps plan's Service: the `spec` the program wrote is the document
/// sent, the port's protocol the schema defaults inside it with its note,
/// no separate `schema default` line; `-v` the same value.
#[test]
fn a_default_inside_a_written_value_is_folded_in() {
    let s = project(
        "fold-service",
        r#"use kube

resource kube.service pg {
  metadata.name = "pg"
  spec = { selector: { app: "pg" }, ports: [{ port: 5432, targetPort: 5432 }] }
}
"#,
    );
    let want = r#"  + kube.service pg  main.df:3
      metadata.name = "pg"
      spec = {
        selector: { app: "pg" },
        ports: [{
          port: 5432,
          targetPort: 5432,
          protocol: "TCP" (schema default),
        }],
      }
"#;
    let plan = run(&s, &["plan", "main.df"]);
    assert!(plan.contains(want), "{plan}");
    assert!(!plan.contains("  schema default"), "{plan}");
    let how = run(&s, &["plan", "-v", "main.df"]);
    assert!(how.contains(&want[want.find('\n').unwrap()..]), "{how}");
    // `-vv` says each leaf with its chain; the default is its own.
    let full = run(&s, &["plan", "-vv", "main.df"]);
    assert!(
        full.contains("      spec.ports[port=5432,protocol=TCP].protocol = \"TCP\"\n"),
        "{full}"
    );
    // The note is dim, as every note is.
    let color = run(&s, &["plan", "--color", "always", "main.df"]);
    assert!(
        color.contains("protocol: \"TCP\" \x1b[2m(schema default)\x1b[0m,"),
        "{color}"
    );
}

/// A default no written value holds (an element the program wrote one
/// field of, which is its own line) keeps its own line, with the same
/// note; a value without a note stays on one line until it is wider than
/// the page.
#[test]
fn a_default_no_written_value_holds_keeps_its_line() {
    let s = project(
        "fold-alone",
        r#"use kube

resource kube.service pg {
  metadata = { name: "pg", labels: { app: "pg", tier: "db" } }
  spec.ports = [{ port: 5432 }]
}

resource kube.service wide {
  metadata = { name: "wide", labels: { app: "a-long-application-name", team: "a-long-team-name", tier: "db" } }
  spec.ports = [{ port: 1, protocol: "UDP" }]
}
"#,
    );
    let plan = run(&s, &["plan", "main.df"]);
    for l in [
        "      metadata = { name: \"pg\", labels: { app: \"pg\", tier: \"db\" } }\n",
        "      spec.ports[port=5432,protocol=TCP].port = 5432\n      \
         spec.ports[port=5432,protocol=TCP].protocol = \"TCP\" (schema default)\n",
        "      metadata = {\n        name: \"wide\",\n        labels: { app: \"a-long-application-name\", \
         team: \"a-long-team-name\", tier: \"db\" },\n      }\n",
    ] {
        assert!(plan.contains(l), "{l}\n{plan}");
    }
}

/// A secret inside a written value is `(sensitive)` where its value
/// would be: a value, no note after one, so it breaks nothing by itself.
/// `why` of the attribute still gives the chain of the write that made
/// it, the default no write of the program.
#[test]
fn a_sensitive_field_inside_a_value_says_no_value() {
    let s = project(
        "fold-sensitive",
        r#"use kube

resource kube.service pg {
  metadata.name = "pg"
  spec = { token: "hunter2", selector: { app: "pg" } }
  spec.ports = [{ port: 1 }, { port: 2, protocol: "UDP" }]
}
"#,
    );
    let plan = run(&s, &["plan", "main.df"]);
    assert!(!plan.contains("hunter2"), "{plan}");
    assert!(
        plan.contains("      spec = { token: (sensitive), selector: { app: \"pg\" } }\n"),
        "{plan}"
    );
    assert!(
        plan.contains(
            "      spec.ports = [\n        {\n          port: 1,\n          \
             protocol: \"TCP\" (schema default),\n        },\n        \
             { port: 2, protocol: \"UDP\" },\n      ]\n"
        ),
        "{plan}"
    );
    let why = run(&s, &["why", "pg.spec.ports", "main.df"]);
    assert!(
        why.contains(
            "kube.service pg.spec.ports[0] = {port: 1, protocol: \"TCP\"}\n  \
             = [{port: 1}, {port: 2, protocol: \"UDP\"}]  main.df:6\n"
        ),
        "{why}"
    );
}

/// A value said as its document's row (R-131) says no default inside it:
/// each is its own line after the row, with its note.
#[test]
fn a_default_inside_a_document_row_is_its_own_line() {
    let s = project(
        "fold-row",
        r#"use kube

resource kube.service "${d.metadata.name}" = d where d in yaml.decode(io.read("svc.yml"))
"#,
    );
    s.write(
        "svc.yml",
        "metadata:\n  name: pg\nspec:\n  ports:\n    - port: 5432\n      targetPort: 5432\n\
         ---\nmetadata:\n  name: web\nspec:\n  ports:\n    - port: 80\n      protocol: UDP\n",
    );
    let plan = run(&s, &["plan", "main.df"]);
    assert!(
        plan.contains(
            "      = svc.yml:1  (77 B)\n      \
             spec.ports[port=5432,protocol=TCP].protocol = \"TCP\" (schema default)\n"
        ),
        "{plan}"
    );
}
