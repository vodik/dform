//! A value a loader read says the row it is, not its content (R-131):
//! R-124's fold continued. A resource whose body is a document, or an
//! attribute whose one contribution is a document value, prints the file
//! and the line its document starts on (and the steps into it for a
//! selection) with its size; `-v` lays the value out; an update says the
//! leaves that change; `why` points at the row.

mod common;
mod tables_common;
use tables_common::{push, repo, scratch};

const MAPS: &str = "\
---
metadata:
  name: settings
data:
  mode: fast
  size: small
---
metadata:
  name: flags
data:
  beta: \"on\"
";

/// An update of a resource made of a document says the leaf that
/// changed, before and after, not the row.
#[test]
fn an_update_says_the_leaves_that_change() {
    let s = scratch("rows-update");
    s.write("maps.yml", MAPS);
    s.write(
        "p.df",
        "\nuse k8s\nresource k8s.config_map \"${d.metadata.name}\" = d where d in yaml.decode(io.read(\"maps.yml\"))\n",
    );
    s.run(&["apply", "--yes", "p.df"]).success();
    s.write("maps.yml", &MAPS.replace("size: small", "size: large"));
    let r = s.run(&["plan", "p.df"]).success();
    assert!(
        r.stdout.contains(
            "  ~ k8s.config_map settings  p.df:3\n      data.size = \"small\" → \"large\"\n"
        ),
        "{}",
        r.stdout
    );
    assert!(!r.stdout.contains("maps.yml"), "{}", r.stdout);
}

/// A selection into a document names the steps to the value, a body's
/// and an attribute's; a document that is the whole file is the file; a
/// leaf another write made inside one is its own line after it.
#[test]
fn a_selection_names_its_steps() {
    let s = scratch("rows-selection");
    s.write(
        "teams.yml",
        "teams:\n  - metadata: { name: a }\n    data: { x: \"1\", y: \"2\" }\n  \
         - metadata: { name: b }\n    data: { x: \"3\" }\n",
    );
    s.write("labels.yml", "team: platform\ntier: web\n");
    s.write(
        "p.df",
        "\nuse k8s\n\
         resource k8s.config_map \"${t.metadata.name}\" = t where t in yaml.decode(io.read(\"teams.yml\")).teams\n\
         resource k8s.config_map whole {\n  metadata.name = \"whole\"\n  \
         metadata.labels = yaml.decode(io.read(\"labels.yml\"))\n  data = yaml.decode(io.read(\"teams.yml\")).teams[0].data\n}\n\
         set c.data.z = \"9\" where c in k8s.config_map\n",
    );
    let r = s.run(&["plan", "p.df"]).success();
    for want in [
        "  + k8s.config_map a      p.df:3\n      = teams.yml .teams[0]  (50 B)\n      \
         data.z = \"9\"        p.df:9\n",
        "  + k8s.config_map whole  p.df:4\n      data = teams.yml .teams[0].data  (17 B)\n      \
         data.z = \"9\"        p.df:9\n      \
         metadata.labels = labels.yml  (32 B)\n      metadata.name = \"whole\"\n",
    ] {
        assert!(r.stdout.contains(want), "{want}\n{}", r.stdout);
    }
}

/// `why` says the binding a document's row by its row; `-vv` says each
/// leaf, with its chain.
#[test]
fn why_points_at_the_row() {
    let s = scratch("rows-why");
    s.write("maps.yml", MAPS);
    s.write(
        "p.df",
        "\nuse k8s\nresource k8s.config_map \"${d.metadata.name}\" = d where d in yaml.decode(io.read(\"maps.yml\"))\n",
    );
    let why = s.run(&["why", "k8s.config_map flags", "p.df"]).success();
    assert!(
        why.stdout
            .starts_with("k8s.config_map flags  p.df:3  with d = maps.yml:8  (50 B)\n"),
        "{}",
        why.stdout
    );
    let full = s.run(&["plan", "-vv", "p.df"]).success();
    assert!(
        full.stdout
            .contains("      data.mode = \"fast\"\n      data.size = \"small\"\n"),
        "{}",
        full.stdout
    );
}

/// A stream read from a repository is said where the rows of a table
/// read from one are: the repository at its commit, the path, the line.
#[test]
fn a_document_read_from_git_says_its_commit() {
    let s = scratch("rows-git");
    repo(&s, "maps.git");
    let commit = push(&s, "maps.yml", MAPS, "main");
    s.write(
        "p.df",
        "\nuse k8s\nresource k8s.config_map \"${d.metadata.name}\" = d \
         where d in yaml.decode(io.read(\"git+file:maps.git/maps.yml?ref=main\"))\n",
    );
    let r = s.run(&["plan", "p.df"]).success();
    let want = format!("      = maps.git@{}:maps.yml:8  (50 B)\n", &commit[..7]);
    assert!(r.stdout.contains(&want), "{want}\n{}", r.stdout);
}
