//! Provider schemas are facts loaded from `providers/<name>/schema.df`.

mod common;
use common::{Scratch, repo};

#[test]
fn schema_facts_are_queryable_edb() {
    let s = Scratch::new("schema-query");
    let prog = repo().join("examples/demo/stacks/dform.df");
    let r = s
        .run(&["query", "type_attr", prog.to_str().unwrap()])
        .success();
    assert!(r.stdout.contains("matches: 16"), "{}", r.stdout);
    assert!(
        r.stdout.contains(r#"type_attr("db.postgres", "endpoint""#),
        "{}",
        r.stdout
    );
}

#[test]
fn a_schema_file_in_the_working_directory_is_a_provider() {
    let s = Scratch::new("schema-local");
    s.write(
        "providers/ldap/schema.df",
        "edition 2026\ntype_provider(ldap.group, \"ldap\")\ntype_attr(ldap.group, \"dn\", \"string\", [\"computed\", \"id\"])\n",
    );
    s.write(
        "p.df",
        "edition 2026\nresource ldap.group admins { cn = \"admins\" }\n",
    );
    let r = s
        .run(&[
            "dev",
            "--provider",
            "ldap",
            "query",
            "type_provider",
            "p.df",
        ])
        .success();
    assert!(
        r.stdout.contains("ldap.group") && r.stdout.contains("matches: 1"),
        "{}",
        r.stdout
    );
    let bad = s
        .run(&["dev", "--provider", "nope", "plan", "p.df"])
        .failure();
    assert!(
        bad.stderr.contains("unknown provider 'nope'"),
        "{}",
        bad.stderr
    );
}
