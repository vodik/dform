//! Giving inputs (R-38): `set k = t where B`, and `set { k = t .. } where
//! B` for several under one clause, are guarded contributions to the
//! program's inputs, at the normal rank unless marked; `set from DOC`
//! gives every leaf of a document to the input at its path; `--set` is
//! `@override`. Overlapping blocks resolve by rank, never by specificity.

mod common;
mod tables_common;
use tables_common::scratch;

const PROGRAM: &str = r#"

key env: enum("dev", "prod") = "dev"
input db { size: int = 1, zone: string = "a" }
use fake

set from FORMAT.decode(io.read("config/${env}.FORMAT"))

resource db.postgres main {
  size = db.size
  zone = db.zone
}
"#;

/// prod's size is 3 in every format; its zone stays the declaration's.
#[test]
fn every_leaf_of_a_document_gives_the_input_at_its_path() {
    for (format, prod) in [
        ("yaml", "db:\n  size: 3\n"),
        ("json", "{\"db\": {\"size\": 3}}"),
        ("toml", "[db]\nsize = 3\n"),
        ("csv", "path,value\ndb.size,3\n"),
    ] {
        let s = scratch(&format!("leaf-{format}"));
        s.write("p.df", &PROGRAM.replace("FORMAT", format));
        s.write(&format!("config/prod.{format}"), prod);
        s.write(
            &format!("config/dev.{format}"),
            match format {
                "csv" => "path,value\n",
                "toml" => "",
                _ => "{}",
            },
        );
        let r = s.run(&["plan", "--why=none", "p.df", "env=prod"]).success();
        assert!(
            r.stdout.contains("  size = 3\n  zone = \"a\""),
            "{format}: {}",
            r.stdout
        );
        let r = s.run(&["plan", "--why=none", "p.df"]).success();
        assert!(r.stdout.contains("  size = 1\n"), "{format}: {}", r.stdout);
    }
}

/// A leaf at no input's path is a typo: a deny naming where the file has
/// it and the inputs there are. `why` names the leaf's line.
#[test]
fn a_leaf_that_is_no_input_is_a_deny() {
    let s = scratch("typo");
    s.write("p.df", &PROGRAM.replace("FORMAT", "yaml"));
    s.write("config/prod.yaml", "db:\n  size: 3\n  zome: b\n");
    let r = s.run(&["plan", "--why=none", "p.df", "env=prod"]).failure();
    assert!(
        r.stderr.contains(
            "- config/prod.yaml:3: db.zome is not an input (its inputs: db.size, db.zone)"
        ),
        "{}",
        r.stderr
    );
    s.write("config/prod.yaml", "db:\n  size: 3\n");
    let r = s
        .run(&["why", "--tree", "db.size", "p.df", "env=prod"])
        .success();
    assert!(
        r.stdout
            .contains("p.df:7  set from yaml.decode(io.read(\"config/${env}.yaml\"))")
            && r.stdout.contains("   config/prod.yaml:2\n"),
        "{}",
        r.stdout
    );
}

/// A composite key: blocks guarded by any subset of it overlap, and rank
/// decides, never specificity: two normal blocks that both hold and
/// disagree are a conflict naming both; the narrower one marked
/// `@override` wins; `--set` (an override too) wins over the defaults and
/// the normal blocks.
const KEYED: &str = r#"

key env: enum("dev", "prod") = "dev"
key region: enum("us", "eu") = "us"
input db { days: int = 3, multi_az: bool = false }
use fake

set { db.days = 14, db.multi_az = true } where env == "prod"
set { db.days = 30 } where env == "prod", region == "eu"

resource db.postgres main {
  backup_days = db.days
  multi_az = db.multi_az
}
"#;

#[test]
fn overlapping_blocks_resolve_by_rank() {
    let s = scratch("keyed");
    s.write("p.df", KEYED);
    let r = s
        .run(&["plan", "--why=none", "p.df", "env=prod", "region=us"])
        .success();
    assert!(
        r.stdout.contains("  backup_days = 14\n  multi_az = true\n"),
        "{}",
        r.stdout
    );
    let r = s.run(&["plan", "--why=none", "p.df"]).success();
    assert!(r.stdout.contains("  backup_days = 3\n"), "{}", r.stdout);
    // Both hold in prod/eu, at one rank, and disagree: a conflict naming both.
    let r = s
        .run(&["plan", "--why=none", "p.df", "env=prod", "region=eu"])
        .failure();
    assert!(
        r.stdout.contains("two contributions disagree")
            && r.stdout.contains("(at p.df:8:")
            && r.stdout.contains("(at p.df:9:"),
        "{}",
        r.stdout
    );
    // Marked, the narrower block wins.
    s.write(
        "p.df",
        &KEYED.replace("{ db.days = 30 } where", "{ db.days = 30 } @override where"),
    );
    let r = s
        .run(&["plan", "--why=none", "p.df", "env=prod", "region=eu"])
        .success();
    assert!(r.stdout.contains("  backup_days = 30\n"), "{}", r.stdout);
    // `--set` is an override: over a normal block.
    let r = s
        .run(&[
            "plan",
            "--why=none",
            "p.df",
            "env=prod",
            "region=us",
            "--set",
            "db.days=7",
        ])
        .success();
    assert!(r.stdout.contains("  backup_days = 7\n"), "{}", r.stdout);
}

/// A `set` entry names an input: the program's own, a field of an object
/// one, a used module's. Anything else is an error naming it, and one
/// with no clause is the H-5 error naming the block to write it in.
#[test]
fn an_entry_is_an_inputs_path() {
    let s = scratch("paths");
    s.write("m.df", "\ninput email: string\n");
    s.write(
        "p.df",
        "\ninput db { size: int = 1 }\nuse m\nuse fake\n\
         set { m.email = \"ops@example.com\" } where db.size == 1\n\
         set { db.sz = 2 } where db.size == 1\n\
         resource db.postgres main { size = db.size, owner = m.email }\n",
    );
    let r = s.run(&["plan", "--why=none", "p.df"]).failure();
    assert!(
        r.stderr.contains("`set db.sz`: input db has no field sz"),
        "{}",
        r.stderr
    );
    s.write(
        "p.df",
        &s.read("p.df")
            .replace("set { db.sz = 2 } where db.size == 1\n", ""),
    );
    let r = s.run(&["plan", "--why=none", "p.df"]).success();
    assert!(
        r.stdout.contains("  owner = \"ops@example.com\""),
        "{}",
        r.stdout
    );
    // With no clause it is an entry of the `use` block (H-5).
    s.write(
        "p.df",
        &s.read("p.df").replace(" } where db.size == 1\n", " }\n"),
    );
    let r = s.run(&["plan", "--why=none", "p.df"]).failure();
    assert!(
        r.stderr
            .contains("`set m.email` with no condition is an entry of `use m`"),
        "{}",
        r.stderr
    );
}

/// An input with no default that a `set` gives is required only where
/// none of them holds: a violation of that deployment, not an error of
/// every one.
#[test]
fn a_required_input_a_set_gives_is_missing_only_where_none_holds() {
    let s = scratch("required");
    s.write(
        "p.df",
        "\nkey env: enum(\"dev\", \"prod\") = \"dev\"\ninput owner: string\n\
         use fake\nset owner = \"ops\" where env == \"prod\"\n\
         resource db.postgres main { owner }\n",
    );
    s.run(&["plan", "--why=none", "p.df", "env=prod"]).success();
    let r = s.run(&["plan", "--why=none", "p.df"]).failure();
    assert!(
        r.stderr.contains(
            "input owner is required and has no value: no `set` gives it in this \
             deployment"
        ),
        "{}",
        r.stderr
    );
}

/// dform.toml's `config` is gone, and says what replaces it.
#[test]
fn a_stack_config_names_set_from() {
    let s = scratch("no-config");
    s.write(
        "dform.toml",
        "[project]\nedition = \"2026\"\n\n[stacks.p]\nconfig = 'yaml(\"config/{env}.yaml\")'\n",
    );
    s.write("p.df", "\nuse fake\n");
    let r = s.run(&["plan", "--why=none", "p.df"]).failure();
    assert!(
        r.stderr.contains("a stack's config is gone (R-38)")
            && r.stderr
                .contains("`set from yaml.decode(io.read(\"config/${env}.yaml\"))`"),
        "{}",
        r.stderr
    );
}

/// `set from` takes any document (R-39): a selection into a loaded
/// one, `toml.decode(io.read("cfg.toml")).prod`, or a `let` of one.
#[test]
fn set_from_a_selection_or_a_let() {
    let s = scratch("from-select");
    s.write("cfg.toml", "[prod.db]\nsize = 3\n\n[dev.db]\nsize = 2\n");
    s.write(
        "p.df",
        "\nkey env: enum(\"dev\", \"prod\") = \"dev\"\n\
         input db { size: int = 1, zone: string = \"a\" }\nuse fake\n\
         let cfg = toml.decode(io.read(\"cfg.toml\"))\n\
         set from toml.decode(io.read(\"cfg.toml\")).prod where env == \"prod\"\n\
         set from cfg.dev where env == \"dev\"\n\
         resource db.postgres main { size = db.size }\n",
    );
    let r = s.run(&["plan", "--why=none", "p.df", "env=prod"]).success();
    assert!(r.stdout.contains("  size = 3\n"), "{}", r.stdout);
    let r = s.run(&["plan", "--why=none", "p.df"]).success();
    assert!(r.stdout.contains("  size = 2\n"), "{}", r.stdout);
}
