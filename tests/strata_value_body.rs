//! A value body known only at run time (R-126, `resource T N = d where d
//! in yaml.decode(..)`) writes a path per key of its document, which the
//! strata cannot see; a rule reading one attribute of `T` and writing
//! another was a cycle through it. On such a cycle the body is written
//! once per attribute `T` declares and once for every other key (After
//! R-126, as R-116 writes a variable type once per type), so the rule is
//! no cycle; a rule reading and writing the same attribute still is.

mod common;
use common::Scratch;

/// Two ConfigMaps as a manifest ships them.
const STREAM: &str = "\
---
apiVersion: v1
kind: ConfigMap
metadata:
  name: a
data:
  k: v
---
apiVersion: v1
kind: ConfigMap
metadata:
  name: b
data:
  k: w
";

const BODY: &str = "use k8s\n\
    resource k8s.config_map \"${d.metadata.name}\" = d where d in yaml.decode(io.read(\"cms.yml\"))\n";

fn scratch(name: &str, rule: &str) -> Scratch {
    let s = Scratch::new(name);
    s.write("cms.yml", STREAM);
    s.write("p.df", &format!("{BODY}{rule}\n"));
    s
}

/// A label written where the document's `data` says so: the rule reads
/// `data` and writes `metadata`, of the body's type.
#[test]
fn a_rule_reading_one_attribute_of_the_bodys_type_writes_another() {
    let s = scratch(
        "strata-value-body",
        "set c.metadata.labels.team = \"x\" where c in k8s.config_map, c.data.k == \"v\"",
    );
    let r = s.run(&["plan", "-v", "p.df"]).success();
    assert_eq!(r.summary(), "plan: 2 changes (2 create) over 1 tick");
    let (a, b) = r.stdout.split_once("+ k8s.config_map b").unwrap();
    assert!(
        a.contains("      metadata.labels.team = \"x\"  p.df:3\n")
            && a.contains("      apiVersion = \"v1\"\n")
            && a.contains("      kind = \"ConfigMap\"\n"),
        "{}",
        r.stdout
    );
    assert!(!b.contains("team"), "{}", r.stdout);
    // The body's nodes, one per attribute ConfigMap declares and one for
    // every other key; the label's write sits above `data`.
    let strata = s.run(&["dev", "strata", "p.df"]).success().stdout;
    let at = |node: &str| -> usize {
        let line = strata
            .lines()
            .find(|l| l.split_once(' ').is_some_and(|(_, n)| n.trim() == node))
            .unwrap_or_else(|| panic!("no node {node}:\n{strata}"));
        line.split_whitespace().next().unwrap().parse().unwrap()
    };
    let other = "(arg, k8s.config_map, !{binaryData,data,immutable,metadata})";
    assert!(at("(attr, k8s.config_map, data)") < at("(arg, k8s.config_map, metadata)"));
    assert!(at(other) < at("(attr, k8s.config_map, metadata)"));
}

/// A key the type does not declare (`kind`) is read from the body's
/// other keys, below the rule that reads it.
#[test]
fn a_key_the_type_does_not_declare_is_read_from_the_body() {
    let s = scratch(
        "strata-value-body-kind",
        "set c.metadata.labels.kind = c.kind where c in k8s.config_map, c.data.k == \"v\"",
    );
    let r = s.run(&["plan", "--json", "p.df"]).success();
    assert!(
        r.stdout.contains("\"metadata.labels.kind\"") && r.stdout.contains("\"ConfigMap\""),
        "{}",
        r.stdout
    );
}

/// Reading an attribute to write that same attribute is a cycle whatever
/// the body's keys are.
#[test]
fn reading_and_writing_one_attribute_is_still_a_cycle() {
    let s = scratch(
        "strata-value-body-cycle",
        "set c.data.z = \"y\" where c in k8s.config_map, not has c.data.k",
    );
    let r = s.run(&["plan", "p.df"]).failure();
    assert!(
        r.stderr
            .contains("program is not stratifiable: negative cycle through")
            && r.stderr.contains("(arg, k8s.config_map, data)"),
        "{}",
        r.stderr
    );
}
