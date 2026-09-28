//! Eventual consistency: Read returning nothing for a while after Create is
//! not drift. Read is retried up to the type's `type_retry` attempts
//! (default 3), each retry logged on stderr.

mod common;
use common::Scratch;

const PROG: &str = r#"edition 2026

resource net.vpc main { cidr = "10.0.0.0/16" }
resource net.subnet a { vpc_id = ref(net.vpc, "main", "id"), tier = "web" }
"#;

fn dform(s: &Scratch, args: &[&str]) -> common::Run {
    s.run(&[&["--file", "p.df", "--world", "w.json"][..], args].concat())
}

#[test]
fn a_read_lag_within_the_retry_budget_is_not_drift() {
    let s = Scratch::new("retry-ok");
    s.write("p.df", PROG);
    dform(&s, &["apply", "--chaos", "read-lag=net.vpc/main:2"]).success();
    let r = dform(&s, &["plan"]).success();
    assert_eq!(
        r.stderr,
        "retry net.vpc/main read (2/3)\nretry net.vpc/main read (3/3)\n"
    );
    assert_eq!(r.stdout, "stack p is undeformed\n");
}

/// `type_retry(T, N)` in the provider schema sets the budget per type.
#[test]
fn type_retry_sets_the_budget_per_type() {
    let s = Scratch::new("retry-type");
    s.write("p.df", PROG);
    s.write("retry.df", "edition 2026\ntype_retry(net.vpc, 6)\n");
    let args = ["--provider", "fake", "--provider", "retry.df"];
    dform(
        &s,
        &[&args[..], &["apply", "--chaos", "read-lag=net.vpc/main:4"]].concat(),
    )
    .success();
    let r = dform(&s, &[&args[..], &["plan"]].concat()).success();
    assert_eq!(
        r.stderr,
        "retry net.vpc/main read (2/6)\nretry net.vpc/main read (3/6)\n\
         retry net.vpc/main read (4/6)\nretry net.vpc/main read (5/6)\n"
    );
    assert!(
        r.stdout.ends_with("stack p is undeformed\n"),
        "{}",
        r.stdout
    );
    // The default budget would have taken it as gone.
    let s = Scratch::new("retry-type-default");
    s.write("p.df", PROG);
    dform(&s, &["apply", "--chaos", "read-lag=net.vpc/main:4"]).success();
    let r = dform(&s, &["plan"]).success();
    assert!(r.stderr.contains("taken as gone"), "{}", r.stderr);
}

/// At a phase boundary inside one apply the refresh retries too: the second
/// tick does not create the lagging database again.
#[test]
fn a_boundary_refresh_retries() {
    let s = Scratch::new("retry-boundary");
    s.write(
        "w.json",
        r#"{"resources": {"compute.vm::app": {"typ": "compute.vm", "name": "app",
            "attrs": {"db_host": "old.db.fake"}, "computed": {"id": "vm-1"}}}}"#,
    );
    s.write(
        "p.df",
        "edition 2026\nresource db.postgres main { size = 1 }\nresource compute.vm app { db_host = ref(db.postgres, \"main\", \"endpoint\") }\n",
    );
    let r = dform(&s, &["apply", "--chaos", "read-lag=db.postgres/main:1"]).success();
    assert!(
        r.stderr.contains("retry db.postgres/main read (2/5)\n"),
        "{}",
        r.stderr
    );
    assert!(
        r.stdout
            .contains("tick 2:\nplan: 1 deformation (1 update)\n"),
        "{}",
        r.stdout
    );
}
