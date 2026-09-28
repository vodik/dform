//! A keyed stack's config (README "Tables"): `stack app[env] { config =
//! yaml("config/{env}.yaml") }` makes every leaf of the deployment's file a
//! settings contribution, over the program's `@default` layer.

mod common;
mod tables_common;
use tables_common::scratch;

const PROGRAM: &str = r#"edition 2026

input env: enum("dev", "prod") = "dev"
stack app[env] { config = FORMAT("config/{env}.FORMAT") }

settings dev @default {
  db = { size: 1, zone: "a" }
}
settings prod @default {
  db = { size: 1, zone: "a" }
}

let cfg = settings[env]

resource db.postgres main {
  size = cfg.db.size
  zone = cfg.db.zone
}
"#;

fn program(format: &str) -> String {
    PROGRAM.replace("FORMAT", format)
}

/// prod's size is 3 in every format; its zone stays the default layer's.
#[test]
fn every_leaf_is_a_setting_of_the_deployment() {
    for (format, prod) in [
        ("yaml", "db:\n  size: 3\n"),
        ("json", "{\"db\": {\"size\": 3}}"),
        ("toml", "[db]\nsize = 3\n"),
        ("csv", "path,value\ndb.size,3\n"),
    ] {
        let s = scratch(&format!("leaf-{format}"));
        s.write("p.df", &program(format));
        s.write(&format!("config/prod.{format}"), prod);
        s.write(
            &format!("config/dev.{format}"),
            if format == "csv" {
                "path,value\n"
            } else if format == "toml" {
                ""
            } else {
                "{}"
            },
        );
        let r = s.run(&["plan", "p.df", "env=prod"]).success();
        let size = if format == "csv" { "\"3\"" } else { "3" };
        assert!(
            r.stdout
                .contains(&format!("  size = {size}\n  zone = \"a\"")),
            "{format}: {}",
            r.stdout
        );
        let r = s.run(&["plan", "p.df"]).success();
        assert!(r.stdout.contains("  size = 1\n"), "{format}: {}", r.stdout);
    }
}

/// A leaf the program neither writes nor reads is a typo: a deny naming
/// where the file has it. `why` names the leaf's line.
#[test]
fn a_leaf_the_program_does_not_know_is_a_deny() {
    let s = scratch("typo");
    s.write("p.df", &program("yaml"));
    s.write("config/prod.yaml", "db:\n  size: 3\n  zome: b\n");
    let r = s.run(&["apply", "p.df", "env=prod"]).failure();
    assert!(
        r.stderr
            .contains("- config/prod.yaml:3: db.zome is not a setting the program writes or reads"),
        "{}",
        r.stderr
    );
    s.write("config/prod.yaml", "db:\n  size: 3\n");
    let r = s
        .run(&[
            "why",
            r#"attr(settings, "prod", "db.size", 3)"#,
            "p.df",
            "env=prod",
        ])
        .success();
    assert!(
        r.stdout.contains("fact, config/prod.yaml:2"),
        "{}",
        r.stdout
    );
}

/// Two keys name the row by both values.
#[test]
fn a_stack_keyed_twice_names_the_row_by_both() {
    let s = scratch("two-keys");
    s.write(
        "p.df",
        r#"edition 2026

input env: string = "dev"
input region: string = "eu"
stack app[env, region] { config = yaml("config/{env}-{region}.yaml") }

settings "dev/eu" @default {
  size = 1
}

resource db.postgres main {
  size = settings["dev/eu"].size
}
"#,
    );
    s.write("config/dev-eu.yaml", "size: 5\n");
    let r = s.run(&["plan", "p.df"]).success();
    assert!(r.stdout.contains("  size = 5\n"), "{}", r.stdout);
}
