//! Documents (R-39, R-155): a read, `io.read(LOCATION)`, and a decode of
//! one, `yaml.decode(io.read(path))`, `toml`, `json`, `csv`, is a document
//! value; `input p from TERM` reads a relation's rows out of one by column
//! name, through a selector (`.name`, `[*]`), out of a read, a `let` or an
//! input.

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
        "\ninput az from toml.decode(io.read(\"net.toml\"))\n\
         input peering from toml.decode(io.read(\"net.toml\")).peerings\n\
         decl az(name: string, index: int)\ndecl peering(name: string, peer: string)\n\
         use fake\nlet net = toml.decode(io.read(\"net.toml\"))\n\
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
            "\ninput zone from yaml.decode(io.read(\"regions.yaml\")).regions[*].zones\n\
             decl zone({cols})\nuse fake\n\
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
         decl vlan(id: int, cidr: inet)\ndecl team(name: string, port: int)\nuse fake\n\
         let teams = json.decode(io.read(\"teams.json\"))\n\
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
        "\nuse fake\n\
         let pins = csv.decode(io.read(\"git+file:ops.git/pins.csv?ref=main\"))\n\
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

/// The one read (R-155): `io.read` is the text, a format's decode of it
/// the document with its rows at their lines, a decode of a text in hand
/// the pure function; the loaders of before, `text(..)` and `use file`
/// are errors naming the read.
#[test]
fn a_read_is_io_read_and_a_decode_of_it() {
    let s = scratch("doc-io-read");
    s.write("hosts.csv", "name,port\nweb,80\ndb,5432\n");
    s.write(
        "p.df",
        "\ninput host from csv.decode(io.read(\"hosts.csv\"))\n\
         decl host(name: string, port: int)\nuse fake\n\
         let text = io.read(\"hosts.csv\")\n\
         let rows = csv.decode(text)\n\
         let again = csv.encode(rows)\n\
         first(n) where n = rows[0].name\n\
         same() where again == text\n\
         port(n, p) where host(n, p)\n",
    );
    let r = s.run(&["query", "first(N)", "p.df"]).success();
    assert!(r.stdout.ends_with("\n\"web\"\n"), "{}", r.stdout);
    let r = s.run(&["query", "same()", "p.df"]).success();
    assert!(r.stdout.contains("yes"), "{}", r.stdout);
    let r = s.run(&["why", "port(\"db\", P)", "p.df"]).success();
    assert!(r.stdout.contains("hosts.csv:3"), "{}", r.stdout);
    for (old, new) in [
        ("csv(\"hosts.csv\")", "`csv.decode(io.read(\"hosts.csv\"))`"),
        ("text(\"hosts.csv\")", "`io.read(\"hosts.csv\")`"),
    ] {
        s.write("p.df", &format!("\nuse fake\nlet t = {old}\n"));
        let r = s.run(&["plan", "p.df"]).failure();
        assert!(
            r.stderr
                .contains("is gone (R-155): a location is read by `io.read`")
                && r.stderr.contains(new),
            "{old}: {}",
            r.stderr
        );
    }
    s.write("p.df", "\nuse fake\nuse file\n");
    let r = s.run(&["plan", "p.df"]).failure();
    assert!(
        r.stderr.contains("the file provider is gone (R-155)")
            && r.stderr.contains("`io.read(\"PATH\")`"),
        "{}",
        r.stderr
    );
}
