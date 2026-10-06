//! Documents (R-39): a loader, `yaml(path)`, `toml`, `json`, `csv`, each
//! also over `git(repo, ref, path)`, is a document value; `input p from
//! TERM` reads a relation's rows out of one by column name, through a
//! selector (`.name`, `[*]`), out of a loader call, a `let` or an input.

mod common;
mod tables_common;
use tables_common::{push, repo, scratch};

/// One TOML document holds several relations: the whole document takes
/// `[[p]]` by the relation's name, a selector another key's tables; a
/// `let` of it is a value, read through.
#[test]
fn a_toml_document_holds_several_relations() {
    let s = scratch("doc-toml");
    s.write(
        "net.toml",
        "region = \"eu\"\n\n[[az]]\nname = \"a\"\nindex = 1\n\n[[az]]\nname = \"b\"\nindex = 2\n\n\
         [[peerings]]\nname = \"shared\"\npeer = \"vpc-0a1b\"\n",
    );
    s.write(
        "p.df",
        "\ninput az from toml(\"net.toml\")\n\
         input peering from toml(\"net.toml\").peerings\n\
         decl az(name: string, index: int)\ndecl peering(name: string, peer: string)\n\
         provider fake\nlet net = toml(\"net.toml\")\n\
         resource net.subnet \"${n}-${net.region}\" { cidr = \"10.0.${i}.0/24\" } where az(n, i)\n\
         resource net.vpc_peering \"${n}\" { accepter_vpc = p } where peering(n, p)\n",
    );
    let r = s.run(&["plan", "--why=none", "p.df"]).success();
    for want in [
        "+ net.subnet[\"a-eu\"]\n  cidr = \"10.0.1.0/24\"",
        "+ net.subnet[\"b-eu\"]\n  cidr = \"10.0.2.0/24\"",
        "+ net.vpc_peering[\"shared\"]\n  accepter_vpc = \"vpc-0a1b\"",
    ] {
        assert!(r.stdout.contains(want), "{want}: {}", r.stdout);
    }
    // A row's provenance is its line.
    let r = s.run(&["why", "az(\"b\", X)", "p.df"]).success();
    assert!(r.stdout.contains("net.toml:7"), "{}", r.stdout);
}

/// A selector is a path into the document: `.name` a field, `[*]` every
/// element, a list at its end its elements. A column the row lacks is
/// the nearest enclosing object's; a row's field no column takes, and a
/// column nothing gives, are errors naming the row.
#[test]
fn a_selector_reads_rows_and_their_enclosing_objects() {
    let s = scratch("doc-select");
    s.write(
        "regions.yaml",
        "regions:\n  - region: eu\n    zones:\n      - name: a\n      - name: b\n  \
         - region: us\n    zones:\n      - name: c\n",
    );
    let program = |cols: &str| {
        format!(
            "\ninput zone from yaml(\"regions.yaml\").regions[*].zones\n\
             decl zone({cols})\nprovider fake\n\
             resource net.subnet \"${{r}}-${{z}}\" {{ cidr = \"10.0.0.0/24\" }} where zone(r, z)\n"
        )
    };
    s.write("p.df", &program("region: string, name: string"));
    let r = s.run(&["plan", "--why=none", "p.df"]).success();
    for z in ["eu-a", "eu-b", "us-c"] {
        assert!(
            r.stdout.contains(&format!("+ net.subnet[\"{z}\"]")),
            "{}",
            r.stdout
        );
    }
    s.write("p.df", &program("zone: string, name: string"));
    let r = s.run(&["plan", "--why=none", "p.df"]).failure();
    assert!(
        r.stderr.contains("regions.yaml:row 1: no column zone"),
        "{}",
        r.stderr
    );
    s.write(
        "p.df",
        &program("region: string, name: string").replace(".regions[*].zones", ".regions[*].zonez"),
    );
    let r = s.run(&["plan", "--why=none", "p.df"]).failure();
    assert!(
        r.stderr.contains("`.regions[*]` has no field zonez"),
        "{}",
        r.stderr
    );
}

/// `from` takes any document value: an input of a list of objects (so the
/// outside gives a table by giving the input), a `let`, a field of one.
#[test]
fn a_relation_is_read_from_an_input_or_a_let() {
    let s = scratch("doc-value");
    s.write(
        "teams.json",
        "{\"teams\": [{\"name\": \"web\", \"port\": 80}]}",
    );
    s.write(
        "p.df",
        "\ninput vlans: list(any) = [{ id: 10, cidr: \"10.0.10.0/24\" }]\n\
         input vlan from vlans\ninput team from teams.teams\n\
         decl vlan(id: int, cidr: inet)\ndecl team(name: string, port: int)\nprovider fake\n\
         let teams = json(\"teams.json\")\n\
         resource net.subnet \"v${i}\" { cidr = c } where vlan(i, c)\n\
         resource compute.vm \"${n}\" { port = p } where team(n, p)\n",
    );
    let r = s.run(&["plan", "--why=none", "p.df"]).success();
    assert!(
        r.stdout
            .contains("+ net.subnet[\"v10\"]\n  cidr = \"10.0.10.0/24\""),
        "{}",
        r.stdout
    );
    assert!(
        r.stdout.contains("+ compute.vm[\"web\"]\n  port = 80"),
        "{}",
        r.stdout
    );
    s.write("vlans.yaml", "- id: 20\n  cidr: 10.0.20.0/24\n");
    let r = s
        .run(&["plan", "--why=none", "p.df", "--set", "vlans=@vlans.yaml"])
        .success();
    assert!(r.stdout.contains("+ net.subnet[\"v20\"]"), "{}", r.stdout);
    assert!(!r.stdout.contains("v10"), "{}", r.stdout);
}

/// A loader over `git(repo, ref, path)` reads the file at the commit the
/// ref names, which the plan file records; a CSV document is a list of
/// objects by its header.
#[test]
fn a_document_is_read_at_a_git_commit() {
    let s = scratch("doc-git");
    repo(&s, "ops.git");
    let commit = push(&s, "pins.csv", "app,image\nweb,web:1\n", "main");
    s.write(
        "p.df",
        "\nprovider fake\n\
         let pins = csv(git(\"ops.git\", \"main\", \"pins.csv\"))\n\
         resource compute.vm web { image = pins[0].image }\n",
    );
    let r = s
        .run(&["plan", "--why=none", "--out", "plan.json", "p.df"])
        .success();
    assert!(r.stdout.contains("image = \"web:1\""), "{}", r.stdout);
    assert!(
        s.read("plan.json").contains(&commit),
        "the plan records the commit"
    );
}
