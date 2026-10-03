//! Tables (README "Documents and tables"): `input p from FORMAT(SOURCE)` reads rows of
//! `decl p(col: type, ...)` from CSV, JSON, YAML and TOML; a row that is
//! not its columns' types is an error naming its line.

mod common;
mod tables_common;
use tables_common::scratch;

/// A table of peerings in `format`, and a vpc per dev row.
fn program(format: &str) -> String {
    format!(
        r#"edition 2026

input peering from {format}("data/p.{format}")

decl peering(env: enum("dev", "prod"), name: string, port: int, cidr: inet, on: bool)

resource net.vpc "v-${{name}}" {{
  cidr = string(c)
  port = port
}} where peering(env: "dev", name: name, port: port, cidr: c, on: true)
provider fake
"#
    )
}

/// Each format's rows: a, dev, 443, 10.1.0.0/16; b, prod; and c, dev,
/// whose `port` is `bad` (a string where an int goes, on line `bad_line`).
fn rows(format: &str, bad: &str) -> (String, usize) {
    match format {
        "csv" => (
            format!(
                "env,name,port,cidr,on\n\
                 dev,a,443,10.1.0.0/16,true\n\
                 prod,b,80,10.2.0.0/16,true\n\
                 dev,c,{bad},10.3.0.0/16,true\n"
            ),
            4,
        ),
        "json" => (
            format!(
                "[\n  {{\"env\": \"dev\", \"name\": \"a\", \"port\": 443, \"cidr\": \"10.1.0.0/16\", \"on\": true}},\n  \
                 {{\"env\": \"prod\", \"name\": \"b\", \"port\": 80, \"cidr\": \"10.2.0.0/16\", \"on\": true}},\n  \
                 {{\"env\": \"dev\", \"name\": \"c\", \"port\": {bad}, \"cidr\": \"10.3.0.0/16\", \"on\": true}}\n]\n"
            ),
            4,
        ),
        "yaml" => (
            format!(
                "- {{env: dev, name: a, port: 443, cidr: 10.1.0.0/16, on: true}}\n\
                 - env: prod\n  name: b\n  port: 80\n  cidr: 10.2.0.0/16\n  on: true\n\
                 - {{env: dev, name: c, port: {bad}, cidr: 10.3.0.0/16, on: true}}\n"
            ),
            7,
        ),
        "toml" => (
            format!(
                "[[peering]]\nenv = \"dev\"\nname = \"a\"\nport = 443\ncidr = \"10.1.0.0/16\"\non = true\n\n\
                 [[peering]]\nenv = \"prod\"\nname = \"b\"\nport = 80\ncidr = \"10.2.0.0/16\"\non = true\n\n\
                 [[peering]]\nenv = \"dev\"\nname = \"c\"\nport = {bad}\ncidr = \"10.3.0.0/16\"\non = true\n"
            ),
            15,
        ),
        _ => unreachable!(),
    }
}

const FORMATS: [&str; 4] = ["csv", "json", "yaml", "toml"];

#[test]
fn every_format_reads_typed_rows() {
    for format in FORMATS {
        let s = scratch(&format!("read-{format}"));
        s.write("p.df", &program(format));
        s.write(&format!("data/p.{format}"), &rows(format, "22").0);
        let r = s.run(&["plan", "p.df"]).success();
        assert_eq!(r.summary(), "plan: 2 deformations (2 create)", "{format}");
        assert!(
            r.stdout.contains("+ net.vpc[\"v-a\"]\n"),
            "{format}: {}",
            r.stdout
        );
        assert!(
            r.stdout.contains("  cidr = \"10.1.0.0/16\"\n  port = 443"),
            "{format}: {}",
            r.stdout
        );
        assert!(
            r.stdout.contains("+ net.vpc[\"v-c\"]\n"),
            "{format}: {}",
            r.stdout
        );
        assert!(!r.stdout.contains("v-b"), "{format}: {}", r.stdout);
    }
}

/// The ticket's bad-type row: an error naming the file and the row's line,
/// the column, the value and the type.
#[test]
fn a_row_of_the_wrong_type_is_an_error_naming_its_line() {
    for format in FORMATS {
        let s = scratch(&format!("bad-{format}"));
        s.write("p.df", &program(format));
        let bad = if format == "csv" {
            "https"
        } else {
            "\"https\""
        };
        let (text, line) = rows(format, bad);
        s.write(&format!("data/p.{format}"), &text);
        let r = s.run(&["plan", "p.df"]).failure();
        let want = format!("data/p.{format}:{line}: column port: \"https\" is not int");
        assert!(
            r.stderr.contains(&want),
            "{format}: want {want}\n{}",
            r.stderr
        );
        assert!(
            r.stderr
                .contains(&format!("input relation peering from \"data/p.{format}\"")),
            "{}",
            r.stderr
        );
    }
}

#[test]
fn an_enum_a_missing_and_an_extra_column_are_errors() {
    let s = scratch("columns");
    s.write("p.df", &program("csv"));
    let header = "env,name,port,cidr,on\n";
    let cases = [
        (
            format!("{header}qa,a,1,10.1.0.0/16,true\n"),
            "data/p.csv:2: column env: \"qa\" is not enum(dev, prod)",
        ),
        (
            "env,name,port,cidr\ndev,a,1,10.1.0.0/16\n".to_string(),
            "data/p.csv:2: no column on",
        ),
        (
            "env,name,port,cidr,on,zone\ndev,a,1,10.1.0.0/16,true,z\n".to_string(),
            "data/p.csv:2: zone is not a column of peering (its columns: env, name, port, cidr, on)",
        ),
        (
            format!("{header}dev,a,1,10.1.0.0/99,true\n"),
            "data/p.csv:2: column cidr: \"10.1.0.0/99\" is not inet",
        ),
    ];
    for (text, want) in cases {
        s.write("data/p.csv", &text);
        let r = s.run(&["plan", "p.df"]).failure();
        assert!(r.stderr.contains(want), "want {want}\n{}", r.stderr);
    }
    // A JSON number is not a string: nothing is coerced.
    let s = scratch("strict");
    s.write(
        "p.df",
        "edition 2026\ninput t from json(\"t.json\")\ndecl t(name: string)\nwarn \"${n}\" where t(n)\nprovider fake\n",
    );
    s.write("t.json", "[{\"name\": 3}]");
    let r = s.run(&["plan", "p.df"]).failure();
    assert!(
        r.stderr.contains("t.json:1: column name: 3 is not string"),
        "{}",
        r.stderr
    );
}

/// `why` names the row's line; the source is computed from an input.
#[test]
fn why_names_the_row_and_a_computed_source_follows_its_input() {
    let s = scratch("why");
    s.write(
        "p.df",
        r#"edition 2026

input env: enum("dev", "prod") = "dev"
input node from csv("data/${env}.csv")

decl node(name: string)

resource compute.vm "${n}" {
  size = 1
} where node(n)
provider fake
"#,
    );
    s.write("data/dev.csv", "name\nd1\n");
    s.write("data/prod.csv", "name\np1\np2\n");
    let r = s.run(&["plan", "p.df"]).success();
    assert!(r.stdout.contains("+ compute.vm[\"d1\"]"), "{}", r.stdout);
    let r = s.run(&["plan", "--set", "env=prod", "p.df"]).success();
    assert_eq!(r.summary(), "plan: 2 deformations (2 create)");
    let r = s
        .run(&["--set", "env=prod", "why", r#"node("p2")"#, "p.df"])
        .success();
    assert!(r.stdout.contains("   data/prod.csv:3\n"), "{}", r.stdout);
}

/// The orchestrator's note: a table's source computed from its own rows
/// is the extern-in-a-recursive-rule error.
#[test]
fn a_table_whose_source_reads_its_rows_is_a_compile_error() {
    let s = scratch("cycle");
    s.write(
        "p.df",
        "edition 2026\ninput t from csv(\"${src}\")\ndecl t(p: string)\nlet src = p where t(p)\nprovider fake\n",
    );
    let r = s.run(&["plan", "p.df"]).failure();
    assert!(
        r.stderr
            .contains("p.df:2:14: input relation t: its source reads its own rows"),
        "{}",
        r.stderr
    );
    assert!(
        r.stderr.contains("t depends on itself through src"),
        "{}",
        r.stderr
    );
}

/// A relation's rows are every source's and the facts the program states
/// (R-55): two tables and a fact are one relation.
#[test]
fn a_tables_rows_and_stated_facts_are_one_relation() {
    let s = scratch("mixed");
    s.write(
        "p.df",
        "edition 2026\ninput t from csv(\"t.csv\")\ninput t from json(\"u.json\") where \"a\" != \"b\"\n\
         decl t(p: string)\nt(\"x\")\nprovider fake\n",
    );
    s.write("t.csv", "p\ny\n");
    s.write("u.json", "[{\"p\": \"z\"}]");
    let r = s.run(&["query", "t(p)", "p.df"]).success();
    for p in ["\"x\"", "\"y\"", "\"z\""] {
        assert!(r.stdout.contains(p), "{p}: {}", r.stdout);
    }
    // With no `decl`, a table has no columns to read.
    s.write(
        "p.df",
        "edition 2026\ninput t from csv(\"t.csv\")\nprovider fake\n",
    );
    let r = s.run(&["plan", "p.df"]).failure();
    assert!(
        r.stderr.contains("input t from ..: t has no columns"),
        "{}",
        r.stderr
    );
}
