//! A program that names only built-in providers (`file`, `env`, `time`)
//! starts no provider (R-26 leftover): the mock's `fake` schema is not
//! started in its place, so its prelude does not expand into the program,
//! and the audit log records no provider.

mod common;
use common::Scratch;
use serde_json::Value;

const PROGRAM: &str = "edition 2026\nprovider file\nprovider time\n\
    now(t) where t = time.now()\nnote(x) where file.text(\"note.txt\", x)\n";

#[test]
fn a_built_in_only_program_starts_no_fake() {
    let s = Scratch::project("builtin-only");
    s.write("p.df", PROGRAM);
    s.write("note.txt", "hello");
    s.run(&["apply", "p.df"]).success();
    let r = s.run(&["log", "--json", "p.df"]).success();
    let es: Vec<Value> = serde_json::from_str(&r.stdout).unwrap();
    let start = es.iter().find(|e| e["kind"] == "apply_start").unwrap();
    assert_eq!(start["providers"], serde_json::json!([]), "{start}");
    // strata and effects read only the named providers' schemas: none.
    for args in [&["dev", "strata", "p.df"][..], &["dev", "effects", "p.df"]] {
        let r = s.run(args).success();
        assert!(!r.stdout.contains("compute.vm"), "{args:?}: {}", r.stdout);
    }
    let r = s.run(&["query", "note", "p.df"]).success();
    assert!(r.stdout.contains("\"hello\""), "{}", r.stdout);
}
