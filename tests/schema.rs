//! Provider schemas are facts loaded from `providers/<name>/schema.df`.

mod common;
use common::{Scratch, repo};

#[test]
fn schema_facts_are_queryable_edb() {
    let s = Scratch::new("schema-query");
    let prog = repo().join("dform.df");
    let r = s
        .run(&["--file", prog.to_str().unwrap(), "query", "type_attr"])
        .success();
    assert!(r.stdout.contains("matches: 14"), "{}", r.stdout);
    assert!(
        r.stdout
            .contains(r#"Val(Str("db.postgres")), Val(Str("endpoint"))"#),
        "{}",
        r.stdout
    );
}

#[test]
fn a_schema_file_in_the_working_directory_is_a_provider() {
    let s = Scratch::new("schema-local");
    s.write(
        "providers/ldap/schema.df",
        "type_provider(ldap.group, ldap).\ntype_attr(ldap.group, dn, string, [computed, id]).\n",
    );
    s.write("p.df", "resource ldap.group admins { cn = \"admins\" }.\n");
    let r = s
        .run(&[
            "--file",
            "p.df",
            "--provider",
            "ldap",
            "query",
            "type_provider",
        ])
        .success();
    assert!(
        r.stdout.contains("ldap.group") && r.stdout.contains("matches: 1"),
        "{}",
        r.stdout
    );
    let bad = s
        .run(&["--file", "p.df", "--provider", "nope", "plan"])
        .failure();
    assert!(
        bad.stderr.contains("unknown provider 'nope'"),
        "{}",
        bad.stderr
    );
}
